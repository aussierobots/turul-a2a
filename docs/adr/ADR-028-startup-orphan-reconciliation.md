# ADR-028: Startup Reconciliation of Orphaned Tasks

- **Status:** Superseded by ADR-029
- **Date:** 2026-10-25
- **Related:** ADR-009 (durable event coordination), ADR-010 (executor lifecycle, tracked handle), ADR-012 (cancellation; §5 orphan cancel, §10 supervisor-scope trait), ADR-018 (Lambda durable executor), ADR-026 (retention)

## Context

A long-lived server runs each executor as a tokio task inside its own process. The task record is durable; the executor is not. When the process exits (crash, OOM kill, deploy, host restart), every task whose executor was running stays in `SUBMITTED` or `WORKING` in storage, and nothing ever moves it:

- there is no executor left to emit a terminal;
- the in-flight registry that `CancelTask` would trip died with the process;
- retention (ADR-026) is off by default, and when on it deletes the task rather than resolving it.

A client polling `GetTask` or `ListTasks` sees a task that claims to be working forever. A subscriber receives the snapshot and then nothing. ADR-012 §5 resolves such a task only if a client happens to cancel it, and then records `CANCELED`, which attributes a crash to a client decision.

The A2A spec gives a server no operation to resume a lost executor's work, and a terminal task never leaves its terminal state. The only truthful outcome for an orphaned task is therefore a terminal one, `FAILED`, with a reason the client can read.

**Which tasks are candidates.** The states split by who holds the task:

- `SUBMITTED` and `WORKING` are **executor-held**. An executor is expected to be running and to move the task on.
- `INPUT_REQUIRED` and `AUTH_REQUIRED` are **client-held**. The executor ended its turn, and the task waits for the client's next message, which starts a fresh executor. A restart changes nothing for them.
- Terminal states are final.

**Ownership.** The framework records no per-task executor owner. On a single instance, every executor-held task found at startup is an orphan, because the only process that could hold it is the one that died. With several instances sharing storage, a starting instance cannot tell an orphan from a task that a live peer is running.

## Decision

### 1. Owner and entry point

`A2aServer::fail_orphaned_tasks(&mut self) -> Result<usize, A2aError>` is the single entry point. It lists every executor-held task across all tenants and owners (§4), commits `FAILED` for each (§3), and returns how many it failed. The algorithm lives in one crate-internal function behind it, and no other path fails orphans.

It runs before the server serves. Every serving path consumes the `A2aServer` value (`run`, `bind`, `into_router`, `into_routers`, `into_tonic_router`), so once serving has started the method cannot be called on that server.

### 2. Builder opt-in

`A2aServerBuilder::fail_orphaned_tasks_on_start()` takes no argument; without it, nothing changes. When it is set:

- `A2aServer::bind()`, and so `A2aServer::run()`, calls `fail_orphaned_tasks` before binding the listener. An error aborts startup and is returned to the caller.
- `into_router()`, `into_routers()` and `into_tonic_router()` are synchronous and cannot do storage I/O. If the flag is set and `fail_orphaned_tasks` has not completed on this server, they **panic** with a message naming `fail_orphaned_tasks`. An adopter that mounts the router itself calls `server.fail_orphaned_tasks().await?` first. A flag that some serving paths silently ignore is not acceptable.

The method can also be called without the flag, as an explicit opt-in. Once it has completed on a server, `bind()` does not run it a second time.

### 3. The commit

Each candidate is failed through `A2aAtomicStore::update_task_status_with_events`, the same compare-and-set every terminal write uses (ADR-010 §7.1).

**The FAILED write.** The status carries an agent-role message: `task failed: the server restarted while the task was running; its executor did not survive the restart`. This follows the precedent of the other framework-committed `FAILED` terminals (blocking-send hard timeout, executor error, executor panic), which all carry a reason. A `FAILED` with no reason tells the client nothing. The `CancelTask` grace-fallback `CANCELED` (ADR-012 §8) keeps `message = None`; that rule covers only that fallback.

**One commit path for framework terminals.** Every terminal the framework writes on an executor's behalf commits through one function, `turul_a2a::router::commit_framework_terminal` (public but `#[doc(hidden)]`, so the serverless adapter can reach it): the cancel grace fallback (ADR-012 §2), the blocking-send hard timeout (ADR-010 §4.1), the FAILED compensation for a turn whose executor never started (inline push-config registration or durable enqueue failure), the Lambda durable executor's CANCELED for a job cancelled before dispatch (ADR-018), and this reconciliation. For a given status it:

- builds the `TaskStatusUpdateEvent`;
- commits it with the compare-and-set;
- on success, hands the event to the push dispatcher when push delivery is wired;
- wakes the event broker.

Each caller keeps its own mapping of the commit's errors.

**Commit outcomes:**

| Outcome | Effect |
|---|---|
| `Ok` | Counted; logged at WARN, target `turul_a2a::orphaned_task_failed`, with `tenant` and `task_id`. |
| `TerminalStateAlreadySet`, `TaskNotFound` | Skipped, not counted. Something else already resolved the task. |
| Any other storage error | Returned immediately. Tasks already failed stay failed, and a rerun is idempotent because failed tasks are no longer candidates. |

A completed run logs the total at INFO, target `turul_a2a::orphaned_tasks_reconciled`.

### 4. Storage listing

The listing is a required method on the supervisor-scope trait:

`A2aCancellationSupervisor::supervisor_list_executor_held_tasks(&self) -> Result<Vec<ExecutorHeldTask>, A2aStorageError>`

`ExecutorHeldTask` carries `tenant`, `task_id`, `owner` and `context_id`. It is `#[non_exhaustive]` and built with `ExecutorHeldTask::new`, like the push-claim rows, so a later field does not break backends outside the workspace. The method is owner-unscoped, like the other supervisor reads: ADR-012 §10's trait split keeps it off every request-handler path, and its only caller is the startup reconciliation, which acts for the framework, not for a requester.

Per backend:

- **In-memory**: a pass over the task map under the read lock.
- **SQLite / PostgreSQL**: `SELECT tenant, task_id, owner, context_id FROM a2a_tasks WHERE status_state IN ('TASK_STATE_SUBMITTED', 'TASK_STATE_WORKING')`. `status_state` stores the wire names.
- **DynamoDB**: a paginated, consistent-read `Scan` of the tasks table filtered on `statusState`. It runs once per start, and its cost scales with table size.

### 5. Single instance only

**Operator contract:** enable reconciliation only when exactly one server instance uses the storage. Run on one of several instances, it fails tasks that a live peer is running. The peer's executor then finds its sink closed and its result is lost. The framework cannot detect a second instance.

"Exactly one" includes deploys. A rolling or surge deploy (Kubernetes `RollingUpdate` with surge, ECS with `minimumHealthyPercent` = 100) starts the new instance while the old one is still serving and running executors, so for that window there are two instances; the new one would fail the old one's live tasks. A deployment that enables reconciliation uses a stop-first (recreate) rollout. Any other live writer of the same store also counts as a second instance, including an AWS Lambda handler sharing it.

Reconciling across several instances would need a per-task executor owner with a liveness lease. That is a different mechanism, with its own failure modes, and is not part of this decision.

The AWS Lambda adapter has no `A2aServer` and does not expose this. Under ADR-018's durable executor, a `SUBMITTED` task sits in a queue; it is not orphaned.

### 6. What it does not do

- **It does not restart work.** Whether to start the work again is the adopter's decision. Doing so means a new `SendMessage`, which creates a new task; that task can point at the failed one with `referenceTaskIds`.
- **It does not touch client-held or terminal tasks**, and it does not read or write the cancel marker.
- **It does not run periodically.** Without per-task ownership, a sweep while serving cannot tell an orphan from a live task. Orphans that arise without a restart are resolved where they arise, or are left to a client's cancel:
  - an executor panic: the supervisor commits `FAILED` at the moment of the panic (ADR-010 §4.4);
  - a cancel whose request is dropped mid-grace: the grace wait and fallback commit run detached from the request, so the commit still lands (ADR-012 §2);
  - a terminal commit that fails on a storage error after the executor has exited (the post-execute, panic and cancel-fallback commits are single attempts), or a cancel-handling instance that dies mid-grace after the owning instance aborted the executor: the task stays executor-held until a client cancels it (ADR-012 §5) or a restart reconciles it.

### 7. Version classification

**Patch.** The new surface is opt-in and off by default. `A2aCancellationSupervisor` gains one required method. A storage backend implemented outside this workspace must add it to compile against this release; the CHANGELOG states that under Compatibility, together with the new `abort_grace` parameter of `run_cross_instance_cancel_poller` (ADR-012 §3).

## Required tests

1. **Listing parity.** On in-memory, SQLite, PostgreSQL and DynamoDB, seed tasks in every state across two tenants and two owners. `supervisor_list_executor_held_tasks` returns exactly the `SUBMITTED` and `WORKING` tasks, each with the right tenant, owner and context id.
2. **Fails executor-held tasks.** `fail_orphaned_tasks` returns the count. Each candidate reads `FAILED` with the restart reason in an agent-role message with a message id, and its event log is the events it already had followed by exactly one `FAILED` status-update event.
3. **Leaves the rest alone.** `INPUT_REQUIRED`, `AUTH_REQUIRED` and terminal tasks keep their state and their events.
4. **Idempotent.** A second call returns 0 and writes no event on any task.
5. **Builder flag with `bind()` (end to end).** Orphans are `FAILED` before `bind()` returns. Over TCP, `GetTask` returns the `FAILED` task with its reason, and `SubscribeToTask` (`GET /tasks/{id}:subscribe`, the proto binding) returns `UnsupportedOperationError` with its `google.rpc.ErrorInfo` because the task is terminal.
6. **Builder flag with `into_router()`.** Without the method call it panics, and the message names `server.fail_orphaned_tasks()`; under the `grpc` feature `into_routers()` panics the same way. With the method awaited first, the router serves and the orphan reads `FAILED`.
7. **Off by default.** With neither flag nor method call, every orphan keeps its exact state (`SUBMITTED` or `WORKING`) after `bind()`.

## Alternatives considered

- **Cancel each orphan at startup through `CancelTask`.** Records `CANCELED`, but a crash is not a cancellation. It also waits out `cancel_handler_grace` for every task.
- **Periodic background reconciler.** Without per-task ownership it cannot tell a live task from an orphan.
- **Cut-off timestamp, running concurrently with serving** (fail tasks last updated before boot). This depends on the application clock agreeing with the database clock. Running before serving removes the race without any clock.
- **Per-task owner plus liveness lease, for multiple instances.** A new mechanism, and single-instance deployments gain nothing from it.
- **Leave it to retention (ADR-026).** Deletes the task instead of telling the client what happened, and is off by default.

## Revision log

- 2026-10-25: Initial acceptance. The shared framework-terminal commit also absorbs the unstarted-turn FAILED compensation and the Lambda pre-dispatch CANCELED, which committed inline without push dispatch.
- 2026-10-25: Superseded by ADR-029. The executor lease resolves the same orphans without the single-instance restriction; `fail_orphaned_tasks`, `fail_orphaned_tasks_on_start` and `supervisor_list_executor_held_tasks` are deleted. §3 (`commit_framework_terminal` as the one commit path for framework terminals) carries over into ADR-029.
