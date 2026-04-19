# ADR-013: Lambda Push Delivery Parity

- **Status:** Proposed (rev 3 — 2026-04-19)
- **Depends on:** ADR-008 (Lambda adapter), ADR-009 (durable event coordination),
  ADR-011 (push notification delivery), E.20 (`a2a_push_pending_dispatches`
  table)
- **Spec reference:** A2A v1.0 §9 "Push Notifications"; ADR-011 §8 "Config
  lifecycle edge cases" (no-backfill invariant)

## Revision history

- **rev 1 (rejected)** — proposed streaming `a2a_task_events` and
  rescinding E.20. Rejected: event rows don't carry owner;
  `list_tasks(status=Terminal)` is owner-scoped; time-window event
  scan would backfill late configs (violating §8); schema enrichment
  cost exceeded marker-atomicity cost.
- **rev 2 (rejected)** — kept E.20's marker, made the write atomic
  inside `A2aAtomicStore::update_task_status_with_events`. Rejected on
  three grounds:
  1. Made push storage load-bearing for ALL deployments — non-push
     adopters without a provisioned `a2a_push_pending_dispatches`
     table would fail every terminal commit.
  2. "Markers deleted on fan-out preserves §8" was wrong about the
     timing: during the marker-pending window (stream worker delayed,
     throttled, or scheduler picks it up minutes later), a config
     registered AFTER the event commit is picked up by fan-out and
     receives the old terminal event. Claim fencing does not prevent
     this.
  3. Test plan incorrectly classified deleted-task stream records as
     BatchItemFailures. A deleted task is a permanent signal, not a
     transient one; retry-until-DLQ is the wrong response.
- **rev 3 (this doc)** — same atomic-marker strategy, plus three
  corrections:
  - **Opt-in atomic gate**: `A2aAtomicStore::push_dispatch_enabled()`
    trait method (default false); builder validates consistency between
    atomic-store opt-in and `push_delivery_store` presence. Non-push
    deployments are unaffected.
  - **Late-config filter**: `a2a_push_configs` gains a
    `created_at_micros` column; new trait method
    `A2aPushNotificationStorage::list_configs_eligible_at(..., threshold)`
    returns only configs registered at or before the given commit
    timestamp. Fan-out calls this with the marker's `recorded_at_micros`.
    §8 is preserved deterministically, not timing-dependent.
  - **Deleted-task handling**: stream-worker `Ok(None)` from
    `get_task` deletes the marker and returns success — no
    BatchItemFailure. BatchItemFailure reserved for transient errors
    and unparseable records.

## 1. Context

ADR-011 specified push notification delivery for turul-a2a. Its design
assumes an always-on process: a `PushDeliveryWorker` spawned at
`A2aServer::run`, a per-instance reclaim loop polling
`list_reclaimable_claims` and (since E.20) a pending-dispatch marker
table, and a dispatcher that fires fan-out via `tokio::spawn`. None of
those assumptions survive a Lambda deployment.

AWS Lambda's execution model is request-scoped. Between invocations the
execution environment MAY be frozen indefinitely. `tokio::spawn`ed
tasks created inside an invocation are only guaranteed to run while
the handler is still awaiting — once the handler returns, a spawn may
complete, may be frozen for minutes, or may never run again.
`A2aServer::run`'s sweep loops are never called under Lambda.

Today (commit `47d3694`):

- `LambdaA2aServerBuilder` has no `.push_delivery_store(...)` setter.
- The Lambda `AppState` hardcodes
  `push_delivery_store: None, push_dispatcher: None` so the
  commit-path dispatcher hook is a no-op.
- Neither the reclaim loop nor the pending-dispatch scan runs.
- No test or doc claims Lambda push delivery works.

This ADR decides the correct Lambda push architecture, fixes E.20's
atomicity without regressing non-push adopters, and preserves
ADR-011 §8's no-backfill invariant against all stream-delivery
timing races.

## 2. The five logical tables (inventory)

| Logical table | Default name | Purpose | Trait |
|---|---|---|---|
| Tasks | `a2a_tasks` | Current task state (full Task JSON), owner, context, status, updated_at, cancel marker | `A2aTaskStorage`, `A2aCancellationSupervisor` |
| Task events | `a2a_task_events` | Append-only event stream with `event_sequence`; drives SSE / history / replay | `A2aEventStore` |
| Push configs | `a2a_push_configs` | Registered webhook configs per task (URL, auth, token) — the "hooks" | `A2aPushNotificationStorage` |
| Push delivery claims | `a2a_push_deliveries` | Per (tenant, task_id, event_sequence, config_id) delivery state | `A2aPushDeliveryStore` |
| Pending dispatch markers | `a2a_push_pending_dispatches` | E.20 — durable marker that a terminal event still needs push fan-out | `A2aPushDeliveryStore` |

Notes:

- Cancellation is NOT its own table — marker on `a2a_tasks`.
- Atomic task/event commits span `a2a_tasks` + `a2a_task_events`.
- `a2a_push_configs` (the hooks table) is CRUD-accessed via
  `CreateTaskPushNotificationConfig` / `GetTaskPushNotificationConfig`
  / `ListTaskPushNotificationConfigs` / `DeleteTaskPushNotificationConfig`
  RPCs per A2A v1.0 §9. Streaming this table is NOT the dispatch
  trigger.
- `a2a_push_deliveries` rows appear only AFTER `list_configs` has
  succeeded, so streaming it cannot recover pre-claim failures.
- `a2a_push_pending_dispatches` — when its write is atomic with the
  task/event commit AND its lifecycle accounts for late configs — is
  the correct durable intent record.

## 3. Stream source options

| Option | Source | Pros | Cons | Verdict |
|---|---|---|---|---|
| A | `a2a_push_pending_dispatches` | Every record is real work; no parse-and-discard; owner carried on row; backstop enumeration via existing `list_stale_pending_dispatches` | Needs atomic write (fixed by §4.3); needs late-config filter (fixed by §4.5) | **Selected** |
| B | `a2a_task_events` | Always atomic with task | No owner on rows; no cross-owner scan API; time-window backstop backfills late configs | Rejected |
| C | `a2a_tasks` | Task row has owner | Fires on every mutation; no `event_sequence`; filter logic complex | Rejected |
| D | `a2a_push_deliveries` | Fires on claim creation | Rows don't exist yet in the failure modes E.20 is designed for | Rejected as primary |
| E | `a2a_push_configs` | N/A | Stream fires on CRUD, not on terminal events | Rejected |

## 4. Decision

### 4.1 Primary trigger (DynamoDB-backed Lambda)

DynamoDB Stream on `a2a_push_pending_dispatches` with
`StreamViewType::NEW_IMAGE`. Every INSERT is a committed terminal-event
dispatch intent, written atomically with the task/event rows.

### 4.2 Mandatory backstop (all Lambda backends)

EventBridge Scheduler-invoked worker Lambda. Calls
`list_stale_pending_dispatches` (E.20) + `list_reclaimable_claims`
(E.17). DynamoDB Streams have 24h retention and can be stalled by
poison records; the backstop is non-optional.

### 4.3 Atomic marker write — opt-in

The marker moves from the dispatcher's tokio-spawned `run_fanout` into
`A2aAtomicStore::update_task_status_with_events`, but only when the
storage backend explicitly opts in. `A2aAtomicStore` gains:

```rust
/// Does this atomic store also write pending-dispatch markers
/// atomically with terminal status commits? Default: false.
///
/// Turn this on via the backend-specific builder
/// (e.g. `InMemoryA2aStorage::with_push_dispatch_enabled(true)`)
/// when the deployment runs push delivery. The server builder
/// validates consistency: wiring `push_delivery_store` without
/// enabling push_dispatch on the atomic store is a build-time error.
fn push_dispatch_enabled(&self) -> bool { false }
```

When `true`, after writing the task + event rows in the existing
native transaction, the impl iterates the events; for each event where
`event.is_terminal() && matches!(event, StreamEvent::StatusUpdate {..})`,
it writes a row to `a2a_push_pending_dispatches` with
`(tenant, owner, task_id, event_sequence, recorded_at_micros=now)` in
the same transaction. Failure of the marker write rolls the whole
transaction back.

When `false`, the impl is identical to today's behaviour — task +
event rows only. Non-push deployments never touch the
pending_dispatches infrastructure.

Builder validation:

```rust
// A2aServerBuilder::build
if self.push_delivery_store.is_some()
   && !atomic_store.push_dispatch_enabled() {
    return Err(A2aError::Internal(
        "push_delivery_store wired but atomic_store.push_dispatch_enabled() \
         is false. Call .with_push_dispatch_enabled(true) on the backend \
         storage before passing it to .storage()."
    ));
}
```

Non-push deployments are untouched: no flag to set, no table to
provision, no marker writes, no IAM changes. Existing adopters are
unaffected.

### 4.4 `tokio::spawn` is not a correctness primitive under Lambda

The framework treats post-return spawns as opportunistic latency
optimisation only. Correctness is carried by the atomically-written
marker + stream trigger + scheduler backstop.

### 4.5 Late-config prevention — created_at filter on push_configs

Rev 2 claimed ADR-011 §8 was preserved because "markers are deleted
on fan-out, so late configs don't see old markers." This was wrong
about the timing. During the marker-pending window (stream worker
delayed, poisoned, or scheduled backstop picking it up minutes
later), a config registered AFTER commit time is picked up by
`list_configs` inside fan-out and receives the old terminal event.

Rev 3 adds a deterministic filter that closes the window regardless
of timing:

1. Each `a2a_push_configs` row carries a `created_at_micros` column
   (SQL: ALTER TABLE ADD COLUMN; DDB: new attribute; in-memory:
   struct field). Populated by `create_config` as
   `SystemTime::now()`. Default 0 for pre-migration rows (treats
   legacy configs as "registered at the dawn of time" — permissive,
   but only affects rows written before this migration).
2. New trait method
   `A2aPushNotificationStorage::list_configs_eligible_at`:
   ```rust
   async fn list_configs_eligible_at(
       &self,
       tenant: &str,
       task_id: &str,
       eligible_at_micros: i64,
       page_token: Option<&str>,
       page_size: Option<i32>,
   ) -> Result<PushConfigListPage, A2aStorageError>;
   ```
   Returns configs whose `created_at_micros <= eligible_at_micros`.
   Each backend filters natively (SQL `WHERE`, DDB filter expression,
   in-memory `.filter()`).
3. `dispatcher::run_fanout` and `redispatch_pending` call
   `list_configs_eligible_at(..., marker.recorded_at_micros, ...)`
   instead of `list_configs(...)`. A config created after the
   marker's commit time is NEVER eligible for that marker's fan-out.

This preserves ADR-011 §8 as an invariant, not as a timing
observation. A late-created config receives zero historical terminal
events regardless of stream latency, poison records, or scheduler
cadence.

### 4.6 Deleted-task handling — marker delete, not BatchItemFailure

When the stream worker loads a task via `get_task` and gets `Ok(None)`
(task was deleted between marker write and stream delivery), the
correct action is:

1. Delete the marker (matching `redispatch_pending`'s existing
   `Ok(None)` behaviour).
2. Return success for this batch item — no BatchItemFailure.

A deleted task is a permanent signal, not a transient one. Returning
BatchItemFailure would cause Lambda to retry the record until DLQ;
but no amount of retry will resurrect the task. BatchItemFailure is
reserved for:

- Transient `A2aStorageError` from `get_task`, `get_config`, or
  `list_configs_eligible_at`
- Unparseable stream records (malformed NEW_IMAGE)
- Lambda-internal failures during `redispatch_pending`

## 5. Architecture

### 5.1 Request Lambda (commit path)

```
message:send / message:stream / :cancel / force-commit paths
  → A2aAtomicStore::update_task_status_with_events
        ├── update a2a_tasks                     \
        ├── append a2a_task_events                >  one native
        └── insert a2a_push_pending_dispatches   /   transaction
            (ONLY when push_dispatch_enabled() && event is terminal
             StatusUpdate; otherwise omitted)
  → handler returns response to client
  → [opportunistic] dispatcher.dispatch spawned; may run or be
    frozen; correctness does NOT depend on it completing
```

The `A2aAtomicStore::update_task_status_with_events` trait signature
does not change. The opt-in flag lives on the backend storage struct
and is consulted inside the impl.

### 5.2 Stream worker Lambda

```
DynamoDB Stream on a2a_push_pending_dispatches (NEW_IMAGE)
  → Lambda invoked with records
  → for each INSERT record:
      parse pk/sk → (tenant, task_id, event_sequence)
      extract owner + recorded_at_micros from NEW_IMAGE
      reconstruct a PendingDispatch struct
      dispatcher.redispatch_pending(pending):
          get_task(tenant, task_id, owner):
              Ok(Some(task)) → proceed
              Ok(None)       → delete marker; return success (no
                              BatchItemFailure)
              Err(_)         → return BatchItemFailure (transient)
          list_configs_eligible_at(tenant, task_id,
                                   pending.recorded_at_micros, …):
              Ok(configs) → per-config fan-out
              Err(_)      → return BatchItemFailure (transient)
          delete_pending_dispatch after fan-out completes
  → skip MODIFY records (marker row is insert-only under normal flow;
    MODIFY from a scheduler refresh is safe to skip — next tick handles)
  → skip DELETE records (marker already consumed)
  → return BatchItemFailures with SequenceNumbers of transient
    failures only
```

### 5.3 Scheduled worker Lambda (mandatory backstop)

```
EventBridge Scheduler cron → Lambda invoked
  → store.list_stale_pending_dispatches(cutoff, limit)
        → dispatcher.redispatch_pending(p) for each
  → store.list_reclaimable_claims(limit)
        → dispatcher.redispatch_one(c) for each
  → return summary (counts, errors)
```

Both enumerations are backend-native queries added by E.17 + E.20.

### 5.4 Idempotency

Every path in §5.2 and §5.3 ultimately calls `worker.deliver(target,
payload)`, which calls `claim_delivery`. Three cases, all safe:

1. **No prior claim** — `claim_delivery` creates a Pending claim
   (generation=1).
2. **Live non-terminal claim exists** — `ClaimAlreadyHeld`; worker
   reports `ClaimLostOrFinal`; no POST.
3. **Terminal claim exists** — `ClaimAlreadyHeld` (terminal rows
   frozen per ADR-011 §10); no POST.

Duplicate stream records, stream × scheduler races, request-Lambda
opportunistic fan-out colliding with stream-worker fan-out — all safe
by construction. `delete_pending_dispatch` is idempotent.

### 5.5 Late-config invariant

Given:
- T_commit: wall-clock time of the atomic task+event+marker commit.
- T_register: wall-clock time of a new `a2a_push_configs` row.
- `marker.recorded_at_micros == T_commit` (written atomically).
- `config.created_at_micros == T_register`.

Invariant: fan-out for this marker consumes only configs with
`created_at_micros <= recorded_at_micros`, i.e. `T_register <=
T_commit`. Any config with `T_register > T_commit` is EXCLUDED by
`list_configs_eligible_at`, regardless of how much time passed
between the marker write and fan-out execution.

ADR-011 §8 is preserved as an invariant, not as a timing observation.

## 6. Storage additions

### 6.1 On `A2aAtomicStore`

Add one method, default implemented:

```rust
fn push_dispatch_enabled(&self) -> bool { false }
```

No existing impls change — they inherit the default. Backends that
opt in implement this method (typically reading a bool field on the
storage struct).

### 6.2 On `A2aPushNotificationStorage`

Add one method:

```rust
async fn list_configs_eligible_at(
    &self,
    tenant: &str,
    task_id: &str,
    eligible_at_micros: i64,
    page_token: Option<&str>,
    page_size: Option<i32>,
) -> Result<PushConfigListPage, A2aStorageError>;
```

Returns configs whose `created_at_micros <= eligible_at_micros`. The
existing `list_configs(...)` remains unchanged — useful for
operator CRUD endpoints where no eligibility filter is needed.

### 6.3 Schema changes on `a2a_push_configs`

- SQL (SQLite + Postgres): `ALTER TABLE a2a_push_configs ADD COLUMN
  created_at_micros INTEGER NOT NULL DEFAULT 0;`
  Index on `(tenant, task_id, created_at_micros)` for eligibility
  scans on tasks with many configs.
- DynamoDB: new attribute `createdAtMicros` (Number). Existing
  `create_config` writes include it. Queries add a filter expression
  on `createdAtMicros <= :threshold`.
- In-memory: `StoredPushConfig { config: TaskPushNotificationConfig,
  created_at_micros: i64 }`. Linear filter.

### 6.4 No changes to event rows

`a2a_task_events` is NOT enriched. No new columns, no new trait
methods on `A2aEventStore`, no GSI. ADR-009's append-only event
contract is preserved.

### 6.5 No changes to pending-dispatch rows beyond E.20

`a2a_push_pending_dispatches` keeps its E.20 shape. `recorded_at_micros`
is the commit timestamp, set atomically with the task/event rows.

## 7. Lambda builder wiring

```rust
impl LambdaA2aServerBuilder {
    pub fn push_delivery_store(
        mut self,
        store: impl A2aPushDeliveryStore + 'static,
    ) -> Self { ... }

    // .storage() bound extended to require A2aPushDeliveryStore
    // (mirrors main server).
}
```

On build, if `push_delivery_store` is present:
- Construct `PushDeliveryWorker` + `PushDispatcher`.
- Run the same-backend check including the delivery store.
- Run the retry-horizon validation the main server already does.
- **Run the new `push_dispatch_enabled()` consistency check** — if
  the atomic store reports `false`, return a build error.
- Install the dispatcher in the Lambda `AppState.push_dispatcher`.

The commit-hook call sites fire the dispatcher as on the main server;
on Lambda, the dispatcher's main responsibility becomes marker
deletion and opportunistic POST — correctness is carried by the
atomic marker + stream/scheduler.

## 8. New crate surface — `turul-a2a-aws-lambda`

```rust
pub struct LambdaStreamRecoveryHandler { /* dispatcher + push_storage */ }
impl LambdaStreamRecoveryHandler {
    pub async fn handle_stream_event(
        &self,
        event: aws_lambda_events::event::dynamodb::Event,
    ) -> LambdaStreamRecoveryResponse; // BatchItemFailures: transient only
}

pub struct LambdaScheduledRecoveryHandler { /* dispatcher + stores */ }
impl LambdaScheduledRecoveryHandler {
    pub async fn handle_scheduled_event(
        &self,
        event: aws_lambda_events::event::eventbridge::Event,
    ) -> LambdaScheduledRecoveryResponse;
}
```

Both share a private `redispatch_from_marker(pending)` helper that
awaits `dispatcher.redispatch_pending(pending)` and classifies the
result for the caller's response shape.

## 9. Operator responsibilities (IaC)

The framework does NOT create AWS resources. The adopter's IaC
wires:

1. **DynamoDB Streams** on `a2a_push_pending_dispatches`
   (view type: NEW_IMAGE). Required only if push delivery is enabled.
2. **Lambda event-source mapping** from the stream to the stream-
   worker Lambda. DLQ, reserved concurrency, `MaximumRetryAttempts`.
3. **EventBridge Scheduler rule** firing the scheduled-worker
   Lambda. Default recommendation: every 5 minutes. Required
   whenever push delivery is enabled.
4. **TTL** on `a2a_push_pending_dispatches` sufficient to outlive
   worst-case scheduler outage. Default matches
   `push_delivery_ttl_seconds` (7 days).
5. **IAM**: grant `Put/Delete` on `a2a_push_pending_dispatches` to
   the atomic-store role, and `Scan/Query/Delete` to the scheduled-
   worker role. Grant stream-read to the stream-worker event-source
   mapping.

Non-push deployments skip ALL of the above — no marker writes, no
stream, no scheduler.

## 10. Test plan

### 10.1 Atomicity parity (global; per backend)

- `update_task_status_with_events` with `push_dispatch_enabled=true`
  and a terminal `StatusUpdate` event writes an
  `a2a_push_pending_dispatches` row in the same transaction.
- `update_task_status_with_events` with `push_dispatch_enabled=true`
  and a non-terminal status does NOT write a marker.
- `update_task_status_with_events` with `push_dispatch_enabled=true`
  and an artifact-update event does NOT write a marker.
- `update_task_status_with_events` with `push_dispatch_enabled=false`
  NEVER writes a marker, regardless of event terminality. Existing
  non-push deployments unaffected.
- Injected transaction failure rolls back: neither task update nor
  marker is persisted.

### 10.2 Builder consistency

- `push_delivery_store` wired + `push_dispatch_enabled=false` on the
  atomic store → `A2aServer::build()` returns a consistency error.
- `push_delivery_store` wired + `push_dispatch_enabled=true` → ok.
- No `push_delivery_store` + `push_dispatch_enabled=false` → ok.
- No `push_delivery_store` + `push_dispatch_enabled=true` → ok
  (writes markers that nothing consumes; not a misconfiguration, just
  wasteful — allowed for flexibility).

### 10.3 Late-config preservation (§8 invariant)

- Create task; commit terminal event at T1 (marker.recorded_at_micros
  = T1).
- At T2 > T1: create a push config (config.created_at_micros = T2).
- Scheduled worker tick at T3 > T2: `list_stale_pending_dispatches`
  returns the marker; `run_fanout` calls
  `list_configs_eligible_at(..., T1, ...)` → returns empty. No POST
  to the T2 config.
- Assert: wiremock recorded zero POSTs to the late config across
  any timing.

### 10.4 Stream worker — deleted task is NOT a BatchItemFailure

- Seed marker, then delete the task (simulating admin delete).
- Invoke stream worker with the marker record.
- Assert: marker is deleted; BatchItemFailures is empty. No retry
  churn toward DLQ.

### 10.5 Stream worker — transient storage error IS a BatchItemFailure

- Inject a transient `DatabaseError` from `get_task` (wrapper store
  that fails once then succeeds).
- Invoke stream worker with the record.
- Assert: BatchItemFailures contains the SequenceNumber; second
  Lambda retry succeeds.

### 10.6 Stream worker — idempotency

- Duplicate stream record delivered twice; claim fencing makes one
  win. Exactly one POST.

### 10.7 Stream × scheduler race

- Both handlers invoked on the same marker within milliseconds.
  Claim fencing + marker-delete idempotency yield exactly one POST.

### 10.8 Scheduled worker — scan-and-fan-out

- Seed three stale markers recorded before the cutoff; scheduled
  worker processes all three; three POSTs arrive; three markers
  cleared.

### 10.9 Scheduled worker — reclaimable claims

- Existing E.17 parity test shape — unchanged.

### 10.10 Docs assertion

- README Lambda section calls out: EventBridge Scheduler mandatory;
  DDB Stream recommended on DynamoDB; `push_dispatch_enabled=true`
  required on the atomic store when push_delivery_store is wired.
- ADR-008 cross-references ADR-013.

## 11. Follow-up: amend E.20

Rev 3 keeps E.20's table, methods, and tests in place. Concrete
amendments:

- Add `A2aAtomicStore::push_dispatch_enabled()` default method.
- Add per-backend constructor(s) to opt in: e.g.
  `InMemoryA2aStorage::with_push_dispatch_enabled(bool)`.
- Inside each atomic-store impl of `update_task_status_with_events`,
  after writing task + event rows, iterate events and write the
  marker in the same transaction when the flag is on and the event
  is terminal `StatusUpdate`.
- Remove the `record_pending_dispatch` call at the top of
  `dispatcher.rs::run_fanout`. The dispatcher still calls it in the
  redispatch path to refresh `recorded_at` (idempotent upsert).
- Remove `list_configs` calls inside `run_fanout` / redispatch;
  replace with `list_configs_eligible_at(..., pending.recorded_at_micros,
  ...)`.
- Add `created_at_micros` column to `a2a_push_configs` schema on all
  four backends; populate on `create_config`; expose via
  `list_configs_eligible_at`.
- Add a new parity test
  `late_config_registered_after_commit_receives_no_old_events`.
- Preserve existing test
  `pending_dispatch_marker_recovers_persistent_list_configs_outage`.

## 12. Risks and non-goals

**Non-goals:**

- In-Lambda periodic background tasks. `tokio::spawn` post-return is
  opportunistic only.
- Automatic provisioning of AWS resources. Framework ships handler
  code and example IaC snippets only.
- Non-DynamoDB Lambda backends: scheduler-only recovery; scheduler-
  interval latency. Documented, not blocked.
- Retroactive delivery to late configs. Explicitly prevented by §4.5.

**Risks:**

- **Operator forgets to enable `push_dispatch_enabled`.** Builder
  validation catches this at build time — server refuses to start.
  Mitigation: clear error message naming the method to call.
- **Operator forgets to wire EventBridge.** Push recovery silently
  non-functional. Mitigation: cold-start warn, README call-out.
- **Stream worker poison record.** DLQ; backstop sweep independent.
- **Retention window mismatch.** Builder validation rejects
  configurations where `scheduled_sweep_interval >=
  pending_dispatch_ttl / 2`. (Nice-to-have, not a release blocker.)
- **Atomic-store write amplification.** Each terminal commit writes
  three rows instead of two when `push_dispatch_enabled`. Negligible
  for typical workloads; high-terminal-throughput tenants should
  monitor.
- **Pre-migration push_configs have `created_at_micros = 0`.** Treated
  as "registered at the dawn of time" — always eligible. Permissive.
  Operators doing a schema migration should re-stamp these rows via
  a backfill script if they need strict §8 semantics for legacy data.

## 13. Open questions

- Should `list_configs_eligible_at` replace `list_configs` on the
  trait? Rev 3 keeps both — CRUD endpoints need the unfiltered list;
  fan-out needs the filtered list. Some adopter tooling may also
  want the unfiltered list. Two methods, two purposes.
- Should the pre-migration `created_at_micros = 0` default be
  something other than permissive (e.g., "registered at the epoch"
  treated as effectively "never"/"always"/"unknown")? Rev 3 picks
  "always eligible" to avoid breaking existing deployments;
  adopters who need stricter semantics can run a backfill.

## 14. Decision summary

- **Stream source:** `a2a_push_pending_dispatches` (existing, E.20).
- **Trigger topology:** DDB Stream (fast path, DynamoDB only) +
  EventBridge Scheduler (mandatory backstop, all backends).
- **Atomicity:** marker write moves into
  `A2aAtomicStore::update_task_status_with_events`, gated by an
  opt-in `push_dispatch_enabled()` flag on the storage.
- **Late-config prevention:** new `created_at_micros` column on
  `a2a_push_configs`; new trait method
  `list_configs_eligible_at(..., threshold)`; fan-out uses it to
  exclude post-commit registrations.
- **Deleted-task handling:** marker deleted; no BatchItemFailure.
  BatchItemFailure reserved for transient errors and malformed
  records.
- **No new tables.** `a2a_push_pending_dispatches` already exists
  from E.20.
- **No new trait methods on `A2aEventStore`.** Rev 1's
  `find_terminal_event_sequence` is not added.
- **Non-push deployments:** completely unaffected. No flag to set,
  no table required, no marker writes.
- **Release gate:** Phase G (0.1.4) blocked on §7 + §8 + §10 + §11
  landing.

## 15. Agent-team review trace

Rev 3 is the output of three review cycles across a four-agent team
(storage/schema, A2A compliance, devil's advocate, synthesis):

- **Rev 1** was rejected on owner-plumbing, cross-owner scan, and
  §8 backfill grounds.
- **Rev 2** was rejected on: (a) atomic marker made push infra
  load-bearing for non-push deployments; (b) marker-pending window
  still admitted late-config backfill; (c) deleted-task test plan
  mis-classified as BatchItemFailure.
- **Rev 3** (this document) addresses all three: opt-in atomic gate,
  deterministic `created_at_micros` eligibility filter, correct
  deleted-task handling.
