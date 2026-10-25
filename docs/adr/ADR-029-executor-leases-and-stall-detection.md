# ADR-029: Executor Leases and Stall Detection

- **Status:** Accepted
- **Date:** 2026-10-25
- **Supersedes:** ADR-028 (startup orphan reconciliation)
- **Related:** ADR-010 (executor lifecycle, tracked handle), ADR-012 (cancellation; §3 abort after grace, §10 supervisor-scope trait), ADR-018 (Lambda durable executor), ADR-026 (retention)

## Context

The task record is durable; the executor running it is not. ADR-028 resolved tasks left `SUBMITTED`/`WORKING` by a dead process with a sweep at startup that failed every executor-held task. Because the framework recorded nothing about which executors were alive, the sweep was safe only when exactly one instance used the storage: on a shared store, or during a rolling deploy, it failed tasks a live peer was running.

Two failures remain unresolved:

1. **Dead process, several instances.** A task whose instance died stays executor-held forever unless a client cancels it.
2. **Live process, stuck executor.** An executor that hangs (a deadlocked await, a lost upstream call with no timeout) inside a healthy process keeps its task `WORKING` forever. Only blocking sends have a deadline; non-blocking and streaming sends have none.

These are different failures with different detectors. A process that dies stops writing to storage, which any instance can observe. An executor that hangs inside a live process can only be seen by that process.

## Decision

### 1. Executor lease (process liveness)

`A2aServerBuilder::executor_lease(ttl: Duration)` opts in; without it no lease is written, renewed or reaped. `ttl` must be non-zero; `build()` rejects zero.

**State.** A task carries an optional lease expiry in epoch microseconds. It is storage-internal: it is not part of the `Task` wire shape and never reaches a client.

- **SQL**: nullable column `lease_expires_at` on `a2a_tasks`. Task writes are `UPDATE`s that never name it.
- **In-memory**: a field on the stored task record, carried across every record replacement.
- **DynamoDB**: a separate **lease item** in the tasks table, keyed apart from the task item, carrying `tenant`, `taskId` and `leaseExpiresAt` and no `ttl`. Task writes replace the whole task item (read, then `Put`); a lease kept on the task item would be overwritten by any task write whose read preceded a renewal, which an executor streaming artifacts would do continuously. A separate item is never touched by task writes. Every tasks-table read that scans or queries for task items ignores lease items.

**Owner.** All lease writes and reads go through the supervisor-scope trait (ADR-012 §10), owner-unscoped like the other supervisor methods and never reachable from a request handler's authorization scope:

- `A2aCancellationSupervisor::supervisor_renew_executor_leases(&self, tenant: &str, task_ids: &[String], expires_at_micros: Option<i64>) -> Result<(), A2aStorageError>` sets the expiry on the named tasks, or clears it with `None`. It never creates a task. On SQL and in-memory, ids that do not exist are ignored. On DynamoDB, `Some` is a plain write of the lease item and `None` deletes it; the write does not check that the task item exists, because a transaction condition-checking the task item would make concurrent ordinary task writes fail with a transaction conflict. A lease item whose task item is missing is deleted by the next expired-lease listing.
- `A2aCancellationSupervisor::supervisor_list_expired_executor_leases(&self, now_micros: i64) -> Result<Vec<ExecutorHeldTask>, A2aStorageError>` returns every task, across all tenants and owners, that is `SUBMITTED` or `WORKING` **and** has a lease **and** whose lease expiry is earlier than `now_micros`. A task without a lease is never returned. On DynamoDB the listing also deletes, conditioned on the expiry it read, the expired lease items whose task item no longer exists or is no longer `SUBMITTED`/`WORKING`; that is its only side effect. On SQL and in-memory such a lease is a column value on a resolved task and is never selected.

Both are required trait methods. `ExecutorHeldTask` (`#[non_exhaustive]`, built with `ExecutorHeldTask::new`) is kept from ADR-028. `supervisor_list_executor_held_tasks` is deleted.

**Write at turn start.** When the lease is enabled, `start_task_turn` writes the lease (`now + ttl`) **before** it commits `WORKING`: for a new task after the `SUBMITTED` create, for a continuation before the `INPUT_REQUIRED`/`AUTH_REQUIRED` → `WORKING` commit. A continuation's previous lease has long expired; writing the new one first means no reaper ever sees the task executor-held with a stale lease. A lease-write error fails the request like any other storage error in turn start.

**Renewal.** One loop per server renews the lease of every task in this process's in-flight registry, ticking at a fixed rate of `ttl / 3` measured from tick start (a slow pass does not stretch the period), one `supervisor_renew_executor_leases` call per tenant, tenants concurrently. Backends issue per-task writes concurrently in bounded chunks and continue past a failed id. A renewal error is logged at WARN (target `turul_a2a::executor_lease_renew_error`) and retried on the next tick; three missed ticks let the lease expire, which is the intended outcome for an instance that cannot reach storage.

**Clear on executor exit.** When an executor leaves the registry having ended its turn through its sink (a terminal commit, or a latest committed status of `INPUT_REQUIRED`/`AUTH_REQUIRED`), its supervisor clears the task's lease (`None`), best effort, logging WARN on the same target on error. An executor that leaves without ending its turn (aborted by the blocking-send hard deadline or by a cancel) keeps its lease: the framework's own terminal commit follows the abort, and if that commit fails or the process dies first, the lease expires and the reaper fails the task instead of leaving it executor-held with no lease. A lease therefore exists only while an executor holds the task, or until an aborted executor's task is resolved. Without this, a lease left on a task that went `INPUT_REQUIRED` would make a later continuation on an instance that writes no lease (lease off, or the Lambda adapter) look like an expired lease to a peer's reaper, which would fail the live task. If the clear races a continuation that another instance has just started and leased, the continuation's lease is lost until that instance's next renewal tick rewrites it; the race can only remove a lease, never fail a task.

**Reaper.** One loop per server lists expired leases every `ttl` and commits `FAILED` for each through `commit_framework_terminal` (§3), with an agent-role message: `task failed: the server running its executor stopped renewing the executor's lease`. Outcomes:

| Outcome | Effect |
|---|---|
| `Ok` | Logged at WARN, target `turul_a2a::executor_lease_expired`, with `tenant` and `task_id`; the lease is cleared. |
| `TerminalStateAlreadySet`, `TaskNotFound` | Skipped; the lease is cleared. |
| Any other error | Logged at WARN, target `turul_a2a::executor_lease_reap_error`; the lease is kept and the task retried on the next tick. |

The reaper's first pass runs as soon as the loop starts, so a restarted instance fails its predecessor's tasks whose leases have already expired without waiting a tick. Every instance runs a reaper; two reapers racing on one task resolve through the terminal CAS.

**Detection latency.** A dead instance's tasks are failed between `ttl` and `2 × ttl` after its last renewal.

**Where the loops run.** The renewal loop, the reaper and the stall watchdog (§2) are spawned by `A2aServerBuilder::build()`, so they run whichever way the server is served (`run`, `bind`, `into_router`, `into_routers`, `into_tonic_router`). `build()` returns an error when either option is set and no tokio runtime is current. Each loop holds the in-flight registry weakly and exits once the server state that owns the registry is dropped.

**Operator contract.**

- Every instance writing to the store that runs executors must enable the lease with the same `ttl`, or run none. An instance without the lease writes no lease, so its tasks are never reaped (and never wrongly reaped).
- Instance clocks must agree to within `ttl / 3`; expiry is written and compared in application time on every backend. A renewed lease has between `2 × ttl / 3` and `ttl` of headroom, and a peer whose clock runs ahead by more than that fails live tasks.
- Build the server in the tokio runtime that will serve it. The loops are spawned on the runtime current at `build()`; building in a short-lived runtime and serving elsewhere stops renewal, and peers then fail this instance's tasks. A server that is built but never served still runs its loops until it is dropped.
- An instance paused longer than `ttl` (a stop-the-world stall, a suspended VM) loses its tasks: peers fail them, and the paused executors find their sinks closed when they resume. This is the standard lease trade-off; choose `ttl` above the worst expected pause.
- On DynamoDB the reaper is a consistent-read `Scan` of the tasks table every `ttl` per instance, filtered to lease items, followed by a batch read of the expired leases' task items; the scan's cost scales with table size. On SQL it is one `SELECT` over `a2a_tasks` per tick.
- The SQL column must exist before the lease is enabled. A server with the lease off never reads or writes it, so a store created by an earlier release keeps working until the lease is turned on.

**Residuals.** Each window is one storage round trip:

- A process that dies after committing a new task's `SUBMITTED` and before writing its lease leaves a task without a lease, which the reaper never selects. A client's `CancelTask` resolves it (ADR-012 §5).
- A process that dies after its executor exits and before the lease is cleared leaves a lease on a client-held task. If that task's continuation then runs on an instance that writes no lease, a peer's reaper fails it. Running every instance with the lease (operator contract) closes this.
- A renewal pass that took its registry snapshot before an executor left can land after the exit's clear and leave a lease on a client-held task. With every instance on the lease this is harmless: a continuation writes a new lease before `WORKING`, and a client-held task is never selected. With an instance that writes no lease, that instance's continuation could be failed by a peer once the stray lease expires; the operator contract excludes that deployment.
- The reaper lists, then commits. The terminal compare-and-set refuses only a terminal state, so an executor whose lease expired (its instance could not renew) and that commits `INPUT_REQUIRED` between the list and the commit has that interrupt replaced by `FAILED`. The executor's instance had already lost the lease.

**Lambda.** The AWS Lambda adapter has no `A2aServer` and writes no lease. Its tasks are never reaped. ADR-018's queued `SUBMITTED` tasks are therefore safe on a store shared with lease-enabled servers.

### 2. Stall detection (executor progress)

`A2aServerBuilder::executor_stall_timeout(timeout: Duration)` opts in; `build()` rejects zero.

**Progress.** Each in-flight handle records the time of the executor's last progress. Progress is any event its `EventSink` commits, and `EventSink::heartbeat()`, which records progress and writes nothing. An executor doing long silent work calls `heartbeat()` at least once per `timeout`.

**Watchdog.** One loop per server checks the in-flight registry every `timeout / 4`. A handle is stalled when all of these hold:

- no progress for longer than `timeout`;
- its sink has not committed a terminal state, and its latest committed status is not `INPUT_REQUIRED` or `AUTH_REQUIRED` (the handle tracks this on every status commit, so an executor that raised an interrupt and then resumed `WORKING` is watched again);
- its cancellation token has not been tripped (a cancel or blocking-send timeout already owns the outcome).

For a stalled handle the watchdog commits `FAILED` through `commit_framework_terminal` with the agent-role message `task failed: its executor made no progress for <timeout>`, logs WARN (target `turul_a2a::executor_stalled`), then trips the token and arms the abort with `timeout_abort_grace` (`InFlightHandle::cancel_then_abort_after`). `TerminalStateAlreadySet` and `TaskNotFound` (the task was deleted under its executor) skip the commit but still arm the abort. Any other commit error is logged and the handle is retried on the next tick.

While the latest committed status is `INPUT_REQUIRED` or `AUTH_REQUIRED` the executor is not policed: the task waits on the client. After a terminal commit it is never policed again.

**Residual.** The watchdog checks, then commits. An executor silent for exactly the timeout that commits `INPUT_REQUIRED` between the check and the commit has that interrupt replaced by `FAILED`; the terminal compare-and-set refuses only terminal states. The window is one storage round trip after a full timeout of silence.

### 3. One commit path for framework terminals

Every terminal the framework writes on an executor's behalf commits through one function, `turul_a2a::router::commit_framework_terminal` (public but `#[doc(hidden)]`, so the serverless adapter can reach it). Callers: the `CancelTask` grace fallback (ADR-012 §2), the blocking-send hard timeout (ADR-010 §4.1), the `FAILED` compensation for a turn whose executor never started (inline push-config registration or durable enqueue failure), the Lambda durable executor's `CANCELED` for a job cancelled before dispatch (ADR-018), the lease reaper (§1) and the stall watchdog (§2). For a given status it builds the `TaskStatusUpdateEvent`, commits it with the terminal compare-and-set (`A2aAtomicStore::update_task_status_with_events`), hands the event to the push dispatcher when push delivery is wired, and wakes the event broker. Each caller keeps its own mapping of the commit's errors. Framework `FAILED` terminals carry an agent-role reason message; the `CancelTask` fallback `CANCELED` carries none (ADR-012 §8).

This contract was introduced by ADR-028 and is owned here from this release.

### 4. Removal of startup reconciliation

`A2aServer::fail_orphaned_tasks`, `A2aServerBuilder::fail_orphaned_tasks_on_start`, the panic guard in `into_router*`, `server/orphans.rs` and the storage method `supervisor_list_executor_held_tasks` are deleted. The lease resolves the same orphans without the single-instance restriction. ADR-028 is superseded.

### 5. Version classification

**Patch** (0.2.5). Adopters of `fail_orphaned_tasks` move to `executor_lease(ttl)`. `A2aCancellationSupervisor` gains two required methods and loses one. The SQL task table gains a nullable column, needed only once the lease is enabled; stores are never migrated, so the operator re-creates the store or adds the column (the CHANGELOG gives the DDL). DynamoDB needs no table change.

## Required tests

1. **Lease storage parity** (in-memory, SQLite, PostgreSQL, DynamoDB). Seeded tasks in every state across two tenants and two owners: renew sets the expiry only on the named ids of the named tenant; unknown ids are ignored and create nothing; `None` clears; `supervisor_list_expired_executor_leases` returns exactly the `SUBMITTED`/`WORKING` tasks with an expiry earlier than `now`, with tenant, owner and context id; a task without a lease, a live lease, a cleared lease, or a client-held or terminal state is never returned. Task writes after a renewal (status, artifact, full task update) leave the lease unchanged, and on DynamoDB a renewal concurrent with repeated task writes is never lost. On DynamoDB, task listing, `GetTask` and every other task-item read never return or count a lease item.
2. **Lease written before WORKING.** With the lease on, after a blocking send's turn starts (new task and continuation) the task carries a lease in the future at the moment it is `WORKING`. With the lease off, no task carries a lease.
3. **Renewal and clear.** A long-running executor's lease expiry advances while it runs; after it exits having completed or raised `INPUT_REQUIRED`, the task has no lease. An executor aborted without ending its turn keeps its lease, and if the framework's terminal commit does not land, the reaper fails the task once the lease expires. A continuation of that task on a server without the lease is not failed by a lease-enabled peer.
4. **Reaper fails a dead instance's task.** A task seeded `WORKING` with an expired lease is `FAILED` within one reaper tick, with the reason in an agent-role message, its event log gains exactly one `FAILED` status update, and its lease is cleared. A task without a lease and client-held/terminal tasks are untouched.
5. **Multi-instance safety (end to end).** Two servers share one store, both lease-enabled. A long executor runs on server A; server B's reaper runs for several `ttl`s and the task stays `WORKING`. When server A's state is dropped (renewal stops) and the executor is gone, B fails the task within `2 × ttl`. Over HTTP, `GetTask` returns the `FAILED` task with its reason.
6. **Loops on every serving path.** With the lease set, a server served through `into_router()` renews and reaps.
7. **Builder validation.** Zero `ttl` or stall timeout is rejected by `build()`; either option without a current tokio runtime is rejected by `build()`.
8. **Stall.** A silent executor's task is `FAILED` with the stall reason after the timeout and its executor is aborted after `timeout_abort_grace`; a blocking send on it returns that `FAILED` task. An executor that calls `heartbeat()` more often than the timeout is not failed. An executor that raised `INPUT_REQUIRED` and then idles is not failed. An executor that raised `INPUT_REQUIRED`, resumed `WORKING` and then goes silent is failed. A cancelled executor is not failed by the watchdog.
9. **Lambda writes no lease.** The Lambda adapter's builder clears the lease and stall options from any runtime configuration it is given.

## Alternatives considered

- **Keep the startup sweep beside the lease.** Two mechanisms for one outcome; the sweep stays unsafe on shared stores.
- **Lease owner identity on the task.** An instance id per lease adds nothing the expiry does not already decide; dropped.
- **Lease in a separate table.** Needs a new DynamoDB table the operator must provision. On DynamoDB the lease is a separate item in the existing tasks table instead.
- **Lease as an attribute of the DynamoDB task item.** Every task write replaces the whole item from an earlier read and would overwrite a concurrent renewal; conditioning those writes on the lease value would add a retry path to every task write.
- **An expected-state condition on framework terminal commits.** Would close the check-then-act windows of the reaper and the watchdog (§1, §2 residuals), but changes the atomic-store contract on every backend for a one-round-trip window.
- **Database clock for SQL expiry.** Removes skew on SQL only; DynamoDB has no server clock. One application-clock rule on every backend instead.
- **Heartbeat by renewing the lease only on executor progress.** Couples two detectors: a hung executor would be reaped by a peer that cannot abort it, leaving it running in the live process. The watchdog runs where the executor runs and can abort it.
- **Fencing tokens on executor writes.** Prevents a paused instance's late writes from landing, but the closed sink and the terminal CAS already refuse them once the task is `FAILED`.

## Revision log

- 2026-10-25: Initial acceptance. Supersedes ADR-028.
