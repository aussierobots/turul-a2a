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

### 2. Semantic: age-based everywhere

Expiry is **age-based for both tasks and events on every backend**, with no state filter.

- An **event** is expired when `created_at < now - event_ttl`.
- A **task** is expired when its retention timestamp is older than `now - task_ttl`, **regardless of `TaskState`** — a task past its TTL is reaped whether it is terminal (Completed/Failed/Canceled/Rejected) or still live (Submitted/Working/InputRequired/AuthRequired).

This matches DynamoDB's engine-native TTL, which reaps on the `ttl` attribute without consulting task state. Choosing the same semantic everywhere means the four backends agree: there is no backend where a TTL'd task survives because it happens to be non-terminal.

**Documented hazard.** Because task expiry ignores state, **setting a non-zero task TTL can reap a long-running live task** while its executor is still working. A deployment whose tasks can legitimately outlive the configured task TTL must either leave task TTL at `0` (no expiry) or set it comfortably above the worst-case task lifetime. This hazard is intentional and is the price of cross-backend parity with DynamoDB native TTL; it is called out in the retention config docs and the operator-facing README, not hidden.

`0` means **no expiry** for that class. The default for SQL and in-memory backends is `0` (retention OFF) so adopting this release changes no behaviour until an operator opts in. DynamoDB keeps its existing 24h task/event defaults so its behaviour is unchanged.

### 3. `cleanup_expired()` is the single idempotent maintenance entry point

`cleanup_expired()` is the one maintenance operation. A single call deletes **all** expired events and **all** expired tasks for the backend and returns the **total** count of rows removed (events + tasks). It is idempotent: calling it when nothing is expired returns `Ok(0)`; calling it repeatedly converges to a clean store.

Per-backend implementation:

- **PostgreSQL / SQLite:** real `DELETE ... WHERE <timestamp column> < (now - ttl)` against `a2a_task_events` (by `created_at`) and `a2a_tasks` (by its retention timestamp column). When a TTL is `0`, that class is skipped entirely (no DELETE issued).
- **In-memory:** a timestamp sweep over the task and event maps, deleting entries older than their respective TTL.
- **DynamoDB:** stays `Ok(0)`. DynamoDB reaps via its engine-native TTL on the `ttl` attribute; the application MUST NOT issue scans-and-deletes to duplicate that work. This divergence is intentional and documented: on DynamoDB, retention is the engine's job, and `cleanup_expired()` is a deliberate no-op. Operators who want app-driven reaping do not get it on DynamoDB; they configure the table's TTL attribute instead.

Deletion is **batched**. A single `cleanup_expired()` call still drains everything eligible, but it does so in committed chunks of `cleanup_batch_size` rows (default 1000), looping until a batch comes back short. On SQL each batch is its own autocommitted statement, so a large backlog never holds one long transaction or bloats the WAL; in-memory releases the write lock between batches so a large sweep does not stall concurrent access.

The backend column names backing the SQL DELETEs are verified against the live schema before the implementing change lands; this ADR does not pin column names because they are an implementation detail of the storage layer, not part of the retention contract.

### 4. Backend-agnostic retention configuration

A backend-agnostic `RetentionConfig` carries the two TTLs (seconds; `0` = no expiry) plus the `cleanup_batch_size`, and is consumed by the app-driven backends (in-memory, SQLite, PostgreSQL) through a `.with_retention()` builder. SQL and in-memory backends default both TTLs to `0`.

To keep this a backward-compatible change, `DynamoDbConfig` is left untouched: it keeps its own `task_ttl_seconds` / `event_ttl_seconds` fields (24h defaults), which it writes as the native `ttl` attribute. DynamoDB does not consume `RetentionConfig` — it never runs app-driven cleanup, so there is nothing to unify, and folding its fields away would be a needless breaking change. `RetentionConfig` brings the TTL concept to the other three backends without disturbing DynamoDB.

`RetentionConfig` is the single place the TTL semantic is documented for the app-driven backends, including the live-task-reaping hazard from §2.

### 5. Trigger: configuration, two modes by deployment topology

`cleanup_expired()` does work, but **the framework does not call it on its own by default.** Who invokes it is a deployment decision with two shapes, chosen by topology:

- **Self-hosted (long-lived process: ECS / Fargate / App Runner / Kubernetes / bare VM).** An **opt-in** background maintenance loop on the `A2aServer` builder. Shape: a capability-taking builder method that takes the maintenance interval (e.g. `.maintenance(interval)`), following the project's "capability, not intent" rule — no bare boolean toggle. The retention TTLs are configured on the storage backend itself (each backend exposes `.with_retention(RetentionConfig)`), making the backend the single source of truth for what gets reaped; the maintenance method only sets the cadence at which the backend's reaper is invoked. This mirrors DynamoDB, where the TTL lives in the backend config and is written onto each item. A loop with no retention configured on its backend reaps nothing. **Default OFF.** A server that never calls the maintenance method spawns no loop and reaps nothing, so this release introduces no surprise deletion for existing self-hosted adopters.

- **Lambda / serverless (no persistent process).** **No** background loop — a frozen Lambda execution environment cannot host one. Instead the adapter exposes `cleanup_expired()` through a discrete maintenance handler in `turul-a2a-aws-lambda`, invoked by an external EventBridge Scheduler cron that the operator owns and schedules. This follows the ADR-008 thin-wrapper philosophy: the handler is request/response only, adds no core churn, and the operator owns the cadence. (In practice DynamoDB-backed Lambda deployments rely on engine-native TTL and may never need this handler; it exists for completeness and for non-DynamoDB Lambda storage choices.)

Both modes call the same `cleanup_expired()` method. Neither changes default behaviour: self-hosted is opt-in, Lambda requires the operator to wire the schedule.

### 6. Version classification

This is a **patch** bump. At its core this corrects a defect — `cleanup_expired()` was documented (ADR-009) to reap expired events but did nothing on any backend and was never invoked — which is a bug fix, not a new contract. The surrounding new surface is purely additive and opt-in: the `RetentionConfig` type, per-backend `.with_retention()` builders, the self-hosted maintenance method, and the Lambda maintenance handler. `DynamoDbConfig` is left unchanged (§4), so there is no breaking change. No existing call site changes behaviour without opting in. The root `Cargo.toml` `[workspace.package].version` is bumped accordingly and `CHANGELOG.md` records the fix.

## Consequences

- ADR-009's retention claim becomes true: expired events are actually deleted — on the backends that do app-driven cleanup (SQL, in-memory) when maintenance is wired, and engine-side on DynamoDB.
- SQL and in-memory deployments can now bound storage growth, but only after an operator opts in; nothing is deleted silently on upgrade.
- The four backends agree on the age-based, state-agnostic expiry semantic, at the documented cost that a non-zero task TTL can reap live long-running tasks.
- DynamoDB's no-op `cleanup_expired()` is now a documented, intentional divergence rather than an unexplained stub.
- The app-driven backends gain a shared `RetentionConfig`; DynamoDB keeps its own TTL fields, so the fix lands without a breaking change.
- Batched, committed deletion keeps a large retention backlog from stalling the database (long transaction / WAL growth) or the in-memory write lock.

## Cross-references

- **ADR-008** (Lambda adapter and streaming coordination): the Lambda maintenance handler follows the thin-wrapper, request/response-only philosophy established here; no persistent process, no core churn.
- **ADR-009** (Durable event coordination): owns the event store, the `cleanup_expired()` trait method, and the original (unimplemented) event-TTL claim. This ADR corrects that claim and supplies the real reaping mechanics and trigger model. ADR-009 §8's TTL/Cleanup paragraph is amended to point here.
- **ADR-013** (Lambda push-delivery parity): unaffected. Push-delivery retention (`push_delivery_ttl_seconds`) remains DynamoDB-engine-owned and is out of scope for this ADR's cross-backend `cleanup_expired()` work.
- **ADR-018** (Lambda durable executor via SQS): the EventBridge-Scheduler-invoked maintenance handler is the same discrete-event-handler shape ADR-018 uses for its SQS dequeue handler — a single Lambda function classifying and dispatching multiple event shapes.

## Revision log

- 2026-06-02 — Initial acceptance. Defines age-based, state-agnostic retention for events and tasks across all four backends; introduces a backend-agnostic `RetentionConfig` (TTLs + batch size) for the app-driven backends while leaving `DynamoDbConfig` unchanged; `cleanup_expired()` becomes the single idempotent maintenance entry point (batched, committed DELETE on SQL, lock-releasing sweep in-memory, intentional no-op on DynamoDB); dual trigger model (opt-in self-hosted background loop vs Lambda EventBridge discrete handler); patch bump (fixes the never-implemented ADR-009 cleanup claim, no breaking change).
