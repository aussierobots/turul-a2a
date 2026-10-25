# ADR-026: Storage Retention and TTL Enforcement Across Backends

- **Status:** Accepted
- **Date:** 2026-06-02

## Context

ADR-009 introduced a durable event store with a `cleanup_expired()` trait method and stated that "Events older than TTL are deleted by `cleanup_expired()`, called periodically by server maintenance." That claim was never true. `cleanup_expired()` is implemented as `Ok(0)` on every backend (in-memory, SQLite, PostgreSQL, DynamoDB), and nothing in the framework ever calls it. The result is two defects:

1. **ADR drift.** The contract documented in ADR-009 §8 describes behaviour that does not exist. Operators reading the ADR reasonably expect expired events to be reaped; they are not.
2. **Unbounded growth on SQL and in-memory backends.** A long-running self-hosted deployment on PostgreSQL or SQLite accumulates task and event rows forever. DynamoDB has a `ttl` attribute wired on items and an engine-native TTL config (`DynamoDbConfig.task_ttl_seconds` / `event_ttl_seconds`, both defaulting to 24h), so DynamoDB reaps automatically — but that TTL concept is trapped inside `DynamoDbConfig` and is not available to the other three backends.

This ADR makes retention real and uniform: it defines what `cleanup_expired()` deletes, lifts the TTL configuration out of the DynamoDB-only struct into a backend-agnostic retention config, and defines who invokes the cleanup for each deployment topology. It deliberately does **not** redesign the storage traits beyond making the existing `cleanup_expired()` method do real work and giving the backends a TTL configuration to honour.

## Decision

### 1. Scope: events AND tasks

Retention enforcement covers **both** expired events and expired tasks. Event TTL was already part of the ADR-009 contract (and DynamoDB already had a task TTL); this ADR makes both real across all backends through the single `cleanup_expired()` entry point.

### 2. Semantic: age-based, and tasks only once terminal

Expiry is age-based on every backend. Events are expired by age alone; tasks are expired by age **only once they are terminal**.

- An **event** is expired when `created_at < now - event_ttl`, whatever its task's state. Streams start from a task snapshot (ADR-014), so a live task does not need its old events.
- A **task** is expired when it is terminal (`COMPLETED`, `FAILED`, `CANCELED`, `REJECTED`) **and** its retention timestamp is older than `now - task_ttl`. A `SUBMITTED`, `WORKING`, `INPUT_REQUIRED` or `AUTH_REQUIRED` task is never deleted by retention, however old.

**Why live tasks are spared.** Deleting a live task destroys work in progress: the executor's writes start failing and the client's next `GetTask` returns `TaskNotFound` for a task that never finished. Tasks whose executor died are not left live forever: the executor lease (ADR-029) moves them to `FAILED`, after which retention applies.

**Consequence.** A task a client abandons in `INPUT_REQUIRED` or `AUTH_REQUIRED` is kept until the client cancels it or the idle-task sweep (§2a) cancels it. Without the sweep, retention bounds terminal history, not abandoned conversations.

**Consequence without the lease.** The executor lease is opt-in. On a server without it, a task left `SUBMITTED` or `WORKING` by a dead process is never moved to `FAILED`, so retention never deletes it; it stays until a client cancels it. Deployments that set a task TTL should enable the lease.

DynamoDB follows the same rule through its engine-native TTL: a task item carries the `ttl` attribute only once the task is terminal. The write that makes a task terminal sets `ttl` to now + `task_ttl_seconds`; later writes to a terminal task refresh it. Non-terminal writes never set it. Event items keep their write-time `ttl`.

The task's own child items follow the task: artifact items (in the events table) and push-notification config items carry no `ttl` while the task is live, so a long-running task never loses its artifact bodies or its push callbacks. The write that moves a task from a non-terminal to a terminal state stamps the same `ttl` onto the task's existing artifact and push-config items, once; later writes to the terminal task refresh only the task item, because finding push-config items is a table scan. A child item written while the task is already terminal gets the task TTL at write time. The stamp follows the terminal commit and is best effort: a failure is logged at WARN (target `turul_a2a::retention_child_ttl_error`) and leaves those items without a `ttl`, which keeps data rather than losing it. The event-sequence counter keeps its existing lifetime (the longer of the event and task TTLs, refreshed on every event write), so it outlives the events it numbers.

`0` means **no expiry** for that class. The default for SQL and in-memory backends is `0` (retention OFF) so adopting this release changes no behaviour until an operator opts in. DynamoDB keeps its existing 24h task/event defaults.

### 2a. Idle client-held tasks are cancelled, then expire

`A2aServerBuilder::idle_task_timeout(d: Duration)` opts in; `build()` rejects zero. It sets `RuntimeConfig::idle_task_timeout`.

**Selection.** A task is idle when its state is `INPUT_REQUIRED` or `AUTH_REQUIRED` and its status timestamp (the ListTasks sort key, set on every status write) is older than `now - d`. The storage method `A2aCancellationSupervisor::supervisor_list_idle_client_held_tasks(status_before_micros, limit)` returns at most `limit` idle tasks across all tenants and owners, with tenant, task id, owner and context id. It is owner-unscoped and store-wide; its only callers are the sweeps below. `SUBMITTED` and `WORKING` tasks are never selected: an executor holds them, and the executor lease (ADR-029) owns their liveness.

**Action.** For each idle task the sweep commits `CANCELED` through `commit_framework_terminal` (ADR-029 §3), with the agent-role message `task canceled: no client input for <d>`, and logs WARN (target `turul_a2a::idle_task_canceled`). If an executor for the task is still in this instance's in-flight registry, its token is tripped and its abort armed with `cancel_abort_grace`. `TerminalStateAlreadySet` and `TaskNotFound` are skipped. Any other error is logged (target `turul_a2a::idle_task_sweep_error`) and the task is retried on the next sweep. One pass lists and cancels in pages until a page comes back short or every commit in it failed. The `CANCELED` task then expires under §2 like any terminal task.

**Trigger.**

- **Self-hosted.** `build()` spawns one sweep loop per server, beside the lease and stall loops, holding the in-flight registry weakly. It sweeps immediately, then every `d / 4`, so an idle task is cancelled between `d` and `1.25 × d` after its last status write. Several instances sharing a store each sweep; the terminal compare-and-set lets one cancel land, and the others skip it.
- **Lambda.** No loop. `LambdaScheduledRecoveryHandler::with_idle_task_sweep(handler)` adds one sweep pass to each scheduled-recovery tick, using the `idle_task_timeout` of the given `LambdaA2aHandler`'s runtime configuration; with no timeout set, the stage does nothing. The Lambda builder keeps the option (unlike the lease and stall options, it needs no background loop).

**Residual.** The sweep lists, then commits. A client message that resumes an idle task between the two has its continuation replaced by `CANCELED`; the terminal compare-and-set refuses only terminal states. The window is one storage round trip after `d` of client silence. ADR-029 records why framework terminals carry no expected-state condition.

**Cost.** On SQL each page is one `SELECT` over `a2a_tasks` filtered by state and status timestamp. On DynamoDB the status timestamp lives in the task JSON, so each pass is a `Scan` of the tasks table filtered to client-held items, reading their task JSON; its cost scales with table size.

### 3. `cleanup_expired()` is the single idempotent maintenance entry point

`cleanup_expired()` is the one maintenance operation. A single call deletes **all** expired events and **all** expired tasks for the backend and returns the **total** count of rows removed (events + tasks). It is idempotent: calling it when nothing is expired returns `Ok(0)`; calling it repeatedly converges to a clean store.

Per-backend implementation:

- **PostgreSQL / SQLite:** real `DELETE ... WHERE <timestamp column> < (now - ttl)` against `a2a_task_events` (by `created_at`) and `a2a_tasks` (by its retention timestamp column, `updated_at`: the time of the task's last write of any kind). The retention timestamp is deliberately not the ListTasks sort key: listing orders by the status timestamp (ADR-003), so an artifact write keeps a task alive without moving it in the listing. When a TTL is `0`, that class is skipped entirely (no DELETE issued).
- **In-memory:** a timestamp sweep over the task and event maps, deleting entries older than their respective TTL.
- **DynamoDB:** stays `Ok(0)`. DynamoDB reaps via its engine-native TTL on the `ttl` attribute; the application MUST NOT issue scans-and-deletes to duplicate that work. This divergence is intentional and documented: on DynamoDB, retention is the engine's job, and `cleanup_expired()` is a deliberate no-op. Operators who want app-driven reaping do not get it on DynamoDB; they configure the table's TTL attribute instead.

Deletion is **batched**. A single `cleanup_expired()` call still drains everything eligible, but it does so in committed chunks of `cleanup_batch_size` rows (default 1000), looping until a batch comes back short. On SQL each batch is its own autocommitted statement, so a large backlog never holds one long transaction or bloats the WAL; in-memory releases the write lock between batches so a large sweep does not stall concurrent access.

The backend column names backing the SQL DELETEs are verified against the live schema before the implementing change lands; this ADR does not pin column names because they are an implementation detail of the storage layer, not part of the retention contract.

### 4. Backend-agnostic retention configuration

A backend-agnostic `RetentionConfig` carries the two TTLs (seconds; `0` = no expiry) plus the `cleanup_batch_size`, and is consumed by the app-driven backends (in-memory, SQLite, PostgreSQL) through a `.with_retention()` builder. SQL and in-memory backends default both TTLs to `0`.

To keep this a backward-compatible change, `DynamoDbConfig` is left untouched: it keeps its own `task_ttl_seconds` / `event_ttl_seconds` fields (24h defaults), which it writes as the native `ttl` attribute. DynamoDB does not consume `RetentionConfig` — it never runs app-driven cleanup, so there is nothing to unify, and folding its fields away would be a needless breaking change. `RetentionConfig` brings the TTL concept to the other three backends without disturbing DynamoDB.

`RetentionConfig` is the single place the TTL semantic is documented for the app-driven backends, including the terminal-only rule for tasks from §2.

### 5. Trigger: configuration, two modes by deployment topology

`cleanup_expired()` does work, but **the framework does not call it on its own by default.** Who invokes it is a deployment decision with two shapes, chosen by topology:

- **Self-hosted (long-lived process: ECS / Fargate / App Runner / Kubernetes / bare VM).** An **opt-in** background maintenance loop on the `A2aServer` builder. Shape: a capability-taking builder method that takes the maintenance interval (e.g. `.maintenance(interval)`), following the project's "capability, not intent" rule — no bare boolean toggle. The retention TTLs are configured on the storage backend itself (each backend exposes `.with_retention(RetentionConfig)`), making the backend the single source of truth for what gets reaped; the maintenance method only sets the cadence at which the backend's reaper is invoked. This mirrors DynamoDB, where the TTL lives in the backend config and is written onto each item. A loop with no retention configured on its backend reaps nothing. **Default OFF.** A server that never calls the maintenance method spawns no loop and reaps nothing, so this release introduces no surprise deletion for existing self-hosted adopters.

- **Lambda / serverless (no persistent process).** **No** background loop — a frozen Lambda execution environment cannot host one. Instead the adapter exposes `cleanup_expired()` through a discrete maintenance handler in `turul-a2a-aws-lambda`, invoked by an external EventBridge Scheduler cron that the operator owns and schedules. This follows the ADR-008 thin-wrapper philosophy: the handler is request/response only, adds no core churn, and the operator owns the cadence. (In practice DynamoDB-backed Lambda deployments rely on engine-native TTL and may never need this handler; it exists for completeness and for non-DynamoDB Lambda storage choices.)

Both modes call the same `cleanup_expired()` method. Neither changes default behaviour: self-hosted is opt-in, Lambda requires the operator to wire the schedule.

### 6. Version classification

This is a **patch** bump. At its core this corrects a defect — `cleanup_expired()` was documented (ADR-009) to reap expired events but did nothing on any backend and was never invoked — which is a bug fix, not a new contract. The surrounding new surface is purely additive and opt-in: the `RetentionConfig` type, per-backend `.with_retention()` builders, the self-hosted maintenance method, and the Lambda maintenance handler. `DynamoDbConfig` is left unchanged (§4), so there is no breaking change. No existing call site changes behaviour without opting in. The root `Cargo.toml` `[workspace.package].version` is bumped accordingly and `CHANGELOG.md` records the fix.

### 7. Required tests

1. **Terminal-only task expiry parity** (in-memory, SQLite, PostgreSQL). With a task TTL set and every task past it, `cleanup_expired()` deletes the `COMPLETED`, `FAILED`, `CANCELED` and `REJECTED` tasks and keeps the `SUBMITTED`, `WORKING`, `INPUT_REQUIRED` and `AUTH_REQUIRED` ones; expired events are deleted whatever their task's state.
2. **DynamoDB native TTL** (DynamoDB Local). A task item, its artifact items and its push-config items have no `ttl` while the task is non-terminal (create, WORKING, INPUT_REQUIRED, artifact writes, push-config create); the write that makes the task terminal sets the task's `ttl` to now + `task_ttl_seconds` and stamps the same value on its artifact and push-config items, and a later write to the terminal task does not re-stamp them; a push config created on a terminal task gets it at write. Event items carry their write-time `ttl`.
3. **Idle-task listing parity** (in-memory, SQLite, PostgreSQL, DynamoDB). Seeded tasks in every state across two tenants and two owners, with status timestamps before and after the cutoff: `supervisor_list_idle_client_held_tasks` returns exactly the `INPUT_REQUIRED` and `AUTH_REQUIRED` tasks older than the cutoff, with tenant, owner and context id, and at most `limit` of them; on DynamoDB it never returns a lease item.
4. **Idle sweep (end to end).** With `idle_task_timeout` set, a task left `INPUT_REQUIRED` past the timeout is `CANCELED` with the reason in an agent-role message, and its event log gains exactly one `CANCELED` status update; a fresh `INPUT_REQUIRED` task and a `WORKING` task past the timeout are untouched. Without the option nothing is cancelled. A cancelled task is deleted by `cleanup_expired()` once past the task TTL.
5. **Builder validation.** A zero `idle_task_timeout` is rejected by `build()`; the option without a current tokio runtime is rejected by `build()`.
6. **Lambda.** The Lambda builder keeps `idle_task_timeout`; a scheduled-recovery tick with `with_idle_task_sweep` cancels an idle task and reports the count; without a timeout it cancels nothing.

## Consequences

- ADR-009's retention claim becomes true: expired events are actually deleted — on the backends that do app-driven cleanup (SQL, in-memory) when maintenance is wired, and engine-side on DynamoDB.
- SQL and in-memory deployments can now bound storage growth, but only after an operator opts in; nothing is deleted silently on upgrade.
- The four backends agree on the expiry semantic: events by age, tasks by age once terminal. A live task is never reaped; an abandoned client-held task is kept until cancelled, by its client or, when `idle_task_timeout` is set, by the idle-task sweep.
- DynamoDB's no-op `cleanup_expired()` is now a documented, intentional divergence rather than an unexplained stub.
- The app-driven backends gain a shared `RetentionConfig`; DynamoDB keeps its own TTL fields, so the fix lands without a breaking change.
- Batched, committed deletion keeps a large retention backlog from stalling the database (long transaction / WAL growth) or the in-memory write lock.

## Cross-references

- **ADR-008** (Lambda adapter and streaming coordination): the Lambda maintenance handler follows the thin-wrapper, request/response-only philosophy established here; no persistent process, no core churn.
- **ADR-009** (Durable event coordination): owns the event store, the `cleanup_expired()` trait method, and the original (unimplemented) event-TTL claim. This ADR corrects that claim and supplies the real reaping mechanics and trigger model. ADR-009 §8's TTL/Cleanup paragraph is amended to point here.
- **ADR-013** (Lambda push-delivery parity): unaffected. DynamoDB push-delivery item expiry (`push_delivery_ttl_seconds`) remains engine-owned.
- **ADR-011** (Push notification delivery): `cleanup_expired()` also deletes failed push-delivery records (`GaveUp` claim rows) past their `retained_until`, set from the server's `push_failed_delivery_retention` rather than `RetentionConfig`, and counts them in its return value. DynamoDB sets the record's `ttl` to the same instant.
- **ADR-018** (Lambda durable executor via SQS): the EventBridge-Scheduler-invoked maintenance handler is the same discrete-event-handler shape ADR-018 uses for its SQS dequeue handler — a single Lambda function classifying and dispatching multiple event shapes.

## Revision log

- 2026-06-02 — Initial acceptance. Defines age-based, state-agnostic retention for events and tasks across all four backends; introduces a backend-agnostic `RetentionConfig` (TTLs + batch size) for the app-driven backends while leaving `DynamoDbConfig` unchanged; `cleanup_expired()` becomes the single idempotent maintenance entry point (batched, committed DELETE on SQL, lock-releasing sweep in-memory, intentional no-op on DynamoDB); dual trigger model (opt-in self-hosted background loop vs Lambda EventBridge discrete handler); patch bump (fixes the never-implemented ADR-009 cleanup claim, no breaking change).
- 2026-10-22 — `cleanup_expired()` additionally reaps failed push-delivery records past their retention on the SQL and in-memory backends (ADR-011); the returned count includes them.
- 2026-10-22 — Retention basis named explicitly as `updated_at` (last write of any kind), distinct from the ListTasks status timestamp. A DynamoDB live test (`test_native_ttl_attributes_on_tasks_and_events`) pins the engine-native contract: task and event items carry a numeric `ttl` of now + the configured TTL, and the task's `ttl` survives a status rewrite.
- 2026-10-25 — §2 reversed for tasks: retention deletes a task only once it is terminal, on every backend (SQL and in-memory filter on the terminal states; DynamoDB writes the task `ttl` only on terminal writes). The state-blind rule and its live-task-reaping hazard are removed. Executor-held tasks of a dead process reach `FAILED` through ADR-029's lease. The DynamoDB live test now pins: no `ttl` on a non-terminal task item; `ttl` of now + TTL once terminal. Without the lease a dead process's executor-held tasks are never reaped; stated as a consequence. DynamoDB artifact and push-config items carry no `ttl` while their task is live and are stamped when it becomes terminal.
- 2026-10-25 — §2a added: opt-in `idle_task_timeout` cancels `INPUT_REQUIRED`/`AUTH_REQUIRED` tasks whose status is older than the timeout, through `commit_framework_terminal`, so abandoned conversations reach a terminal state and expire. Self-hosted servers sweep in a background loop; Lambda sweeps in the scheduled-recovery handler. New storage method `supervisor_list_idle_client_held_tasks`. Required tests 3–6.
