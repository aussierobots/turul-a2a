# ADR-013: Lambda Push Delivery Parity

- **Status:** Proposed (rev 4 — 2026-04-19)
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
- **rev 3 (rejected)** — same atomic-marker strategy plus opt-in
  gate, but still had two defects:
  - **Wall-clock eligibility is not causal.** Rev 3's
    `created_at_micros <= recorded_at_micros` filter used
    `SystemTime::now()` on each write. In multi-instance / multi-
    Lambda deployments, clocks skew; a config create racing a
    terminal commit can be timestamped before the marker even
    though it was serialized AFTER the event in storage. ADR-011
    §8 needs a causal invariant, not a clock-based one.
  - **Builder was too permissive.** Rev 3 allowed
    `push_dispatch_enabled=true` without `push_delivery_store` wired
    ("writes unused markers, allowed for flexibility"). That
    reintroduces a smaller version of the load-bearing optional-
    infra problem: terminal commits depend on the pending-dispatch
    table even though nothing consumes the markers.
- **rev 4 (this doc)** — corrections:
  - **Causal eligibility via event sequence.** `a2a_tasks` gains
    `latest_event_sequence`, maintained atomically by every commit.
    `a2a_push_configs` gains `registered_after_event_sequence`
    instead of rev 3's `created_at_micros`. `create_config` uses
    a CAS against `a2a_tasks.latest_event_sequence` so a concurrent
    event commit forces a re-read. Fan-out eligibility is
    `config.registered_after_event_sequence < marker.event_sequence`
    — causally serialized by the atomic store, no clocks involved.
  - **Build error for orphaned markers.**
    `push_dispatch_enabled=true` without `push_delivery_store` is a
    build error. A distinctly-named future opt-in
    (`external_push_dispatch_consumer_enabled` or similar) can be
    added if an actual external-consumer use case surfaces; rev 4
    does not pre-build for a speculative one.
  - **Opt-in atomic gate from rev 3 preserved** — still the right
    fix for the non-push load-bearing problem.
  - **Deleted-task handling from rev 3 preserved** — still correct.

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

Builder validation — BOTH directions are errors:

```rust
// A2aServerBuilder::build — consistency check
match (self.push_delivery_store.is_some(),
       atomic_store.push_dispatch_enabled()) {
    (true, true)   => { /* ok — push delivery fully wired */ }
    (false, false) => { /* ok — non-push deployment */ }
    (true, false)  => return Err(A2aError::Internal(
        "push_delivery_store wired but atomic_store.push_dispatch_enabled() \
         is false. Call .with_push_dispatch_enabled(true) on the backend \
         storage before passing it to .storage()."
    )),
    (false, true)  => return Err(A2aError::Internal(
        "atomic_store.push_dispatch_enabled() is true but no \
         push_delivery_store is wired. Pending-dispatch markers would be \
         written with no consumer, imposing load-bearing infra for no \
         benefit. If you need to populate markers for an external \
         consumer, open an issue for a distinctly-named opt-in — for now, \
         this configuration is rejected."
    )),
}
```

Non-push deployments are untouched: no flag to set, no table to
provision, no marker writes, no IAM changes. Existing adopters are
unaffected. Adopters who enable push get both the flag AND the
store — never one without the other.

### 4.4 `tokio::spawn` is not a correctness primitive under Lambda

The framework treats post-return spawns as opportunistic latency
optimisation only. Correctness is carried by the atomically-written
marker + stream trigger + scheduler backstop.

### 4.5 Late-config prevention — causal event-sequence floor

Rev 2 assumed marker-delete timing was sufficient. Rev 3 used a
wall-clock `created_at_micros` comparison. Both are wrong because
they rely on real-time ordering in a multi-writer environment.
Clocks skew; `SystemTime::now()` on two writers can disagree by
seconds; a config create that is serialized AFTER an event commit
in storage can still have an earlier `created_at_micros` value
than the event's `recorded_at_micros`. ADR-011 §8's "registered
before the event" is a CAUSAL relation, not a wall-clock one.

Rev 4 uses the per-task monotonic event sequence — which IS
causally ordered by the atomic store — as the registration floor:

1. **New column on `a2a_tasks`**: `latest_event_sequence` (BIGINT /
   Number / u64). Maintained atomically by every
   `A2aAtomicStore::update_task_status_with_events` and
   `update_task_with_events` as `MAX(previous, new_event_sequences)`.
   Zero until the first event commits. Any commit on a given task
   advances this monotonically.
2. **New column on `a2a_push_configs`**:
   `registered_after_event_sequence` (BIGINT / Number / u64).
   Replaces rev 3's `created_at_micros`.
3. **`create_config` uses a compare-and-swap** against
   `a2a_tasks.latest_event_sequence`:
   - Read `latest_event_sequence = seq_read` for this
     `(tenant, task_id)`.
   - Attempt to write the config with
     `registered_after_event_sequence = seq_read`, conditional on
     `a2a_tasks.latest_event_sequence` STILL equalling `seq_read`.
   - If the condition fails (a concurrent atomic commit advanced
     `latest_event_sequence`), re-read and retry.
   - Per-backend mechanism:
     - **DynamoDB**: `TransactWriteItems` with a `ConditionCheck` on
       `a2a_tasks[task_id].latestEventSequence = :seq_read` and a
       `Put` of the config row.
     - **SQLite / PostgreSQL**: a single SERIALIZABLE transaction
       reading `latest_event_sequence` and inserting the config row.
       Conflicting concurrent `update_task_status_with_events` tx
       causes a serialization failure; outer code retries.
     - **In-memory**: take `tasks` + `push_configs` write guards
       together; read `latest_event_sequence`, insert config; no
       races possible.
4. **New trait method**:
   ```rust
   async fn list_configs_eligible_at_event(
       &self,
       tenant: &str,
       task_id: &str,
       event_sequence: u64,
       page_token: Option<&str>,
       page_size: Option<i32>,
   ) -> Result<PushConfigListPage, A2aStorageError>;
   ```
   Returns configs where
   `registered_after_event_sequence < event_sequence`. Strictly
   less-than: a config registered AT sequence N is not eligible
   for event sequence N (the event was already in-flight when the
   config was written).
5. **Dispatcher path**: `run_fanout` and `redispatch_pending` call
   `list_configs_eligible_at_event(tenant, task_id, marker.event_sequence, …)`
   instead of `list_configs(...)`.

This preserves ADR-011 §8 as a causal invariant: a config registered
AFTER an event was atomically committed (storage-serialization
order, NOT wall-clock order) is never eligible for that event.
Clock skew is irrelevant; `latest_event_sequence` is maintained by
the same atomic transaction that assigns event sequences.

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
  `list_configs_eligible_at_event`
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
      extract owner + event_sequence + recorded_at_micros from NEW_IMAGE
      reconstruct a PendingDispatch struct
      dispatcher.redispatch_pending(pending):
          get_task(tenant, task_id, owner):
              Ok(Some(task)) → proceed
              Ok(None)       → delete marker; return success (no
                              BatchItemFailure)
              Err(_)         → return BatchItemFailure (transient)
          list_configs_eligible_at_event(tenant, task_id,
                                         pending.event_sequence, …):
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

Let `S_commit` be the event_sequence assigned to a terminal event
by the atomic store. Let `S_register` be the
`registered_after_event_sequence` recorded on a push config at its
create-time CAS.

**Invariant:** fan-out for a marker with event_sequence `S_commit`
consumes only configs with `S_register < S_commit`. Any config
with `S_register >= S_commit` is EXCLUDED by
`list_configs_eligible_at_event`.

This is a *causal* invariant:

- `latest_event_sequence` is maintained by the same atomic
  transaction that assigns event sequences, so concurrent writers
  always see a consistent monotonic counter per task.
- `create_config`'s CAS forces a retry whenever a concurrent
  atomic commit would advance `latest_event_sequence` between the
  CAS read and the config write.
- After the retry, `S_register` equals the NEW
  `latest_event_sequence`, which is `>= S_commit`. Eligibility
  filter rejects.

Clock skew, stream delay, scheduler lateness, and inter-instance
timing drift are all irrelevant. The invariant depends only on
storage serialization order.

ADR-011 §8 is preserved deterministically.

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
async fn list_configs_eligible_at_event(
    &self,
    tenant: &str,
    task_id: &str,
    event_sequence: u64,
    page_token: Option<&str>,
    page_size: Option<i32>,
) -> Result<PushConfigListPage, A2aStorageError>;
```

Returns configs whose `registered_after_event_sequence <
event_sequence`. Strict less-than: a config recorded AT sequence N
is not eligible for event sequence N — the event was already in
flight when the config CAS succeeded. The existing
`list_configs(...)` remains unchanged for operator CRUD endpoints.

### 6.3 Schema changes

**`a2a_tasks` gains `latest_event_sequence`:**

- SQL: `ALTER TABLE a2a_tasks ADD COLUMN latest_event_sequence
  BIGINT NOT NULL DEFAULT 0;`
- DynamoDB: new attribute `latestEventSequence` (Number).
- In-memory: new field on `StoredTask`.

**The column write is unconditional in every backend, regardless
of the `push_dispatch_enabled` flag.** It's updated atomically by
every `A2aAtomicStore::update_task_status_with_events` and
`update_task_with_events` to the max of its previous value and the
newly-assigned event sequences, inside the same native transaction
that writes the event rows. Why unconditional: a deployment that
has push disabled today can enable it later — and configs created
AFTER enable would need to compare against a latest_event_sequence
that reflects ALL events committed since the task existed, not just
the ones committed after push was enabled. Universally maintaining
the counter is the simplest guarantee of that invariant, and the
cost is one additional BIGINT write per commit — negligible vs.
the existing task + event writes. ONLY the marker write on
`a2a_push_pending_dispatches` is gated by `push_dispatch_enabled`.

**`a2a_push_configs` gains `registered_after_event_sequence`:**

- SQL: `ALTER TABLE a2a_push_configs ADD COLUMN
  registered_after_event_sequence BIGINT NOT NULL DEFAULT 0;`
  Index on `(tenant, task_id, registered_after_event_sequence)`
  for fast eligibility scans.
- DynamoDB: new attribute `registeredAfterEventSequence` (Number).
- In-memory: new field on `StoredPushConfig`.

**Pre-migration rows** (tasks and configs existing before this
ADR lands) have both new columns defaulted to 0. For `a2a_tasks`,
the default is harmless: the first post-migration commit updates
`latest_event_sequence` to the correct value (max of previous 0
and the new event's sequence equals the new sequence; subsequent
commits extend correctly). For `a2a_push_configs`, the default
means legacy configs have `registered_after_event_sequence = 0`
— eligible for any event with sequence > 0 (i.e., all real
events). This is permissive by design: legacy configs predate the
causal floor and the framework does not retroactively invent a
registration sequence for them. Operators who need strict causal
semantics on legacy configs should run a one-off backfill script
that sets `registered_after_event_sequence = latest_event_sequence`
per `(tenant, task_id)` at migration time.

### 6.4 `create_config` CAS

Per-backend pseudocode for the create path:

```rust
loop {
    let seq_read = task_storage.get_latest_event_sequence(tenant, task_id).await?;
    let result = push_storage.create_config_with_cas(
        tenant, task_id, config.clone(),
        /* registered_after = */ seq_read,
        /* expected_latest_event_sequence = */ seq_read,
    ).await;
    match result {
        Ok(cfg) => return Ok(cfg),
        Err(A2aStorageError::ConditionalCheckFailed { .. }) => continue,  // retry
        Err(other) => return Err(other),
    }
}
```

The CAS primitive lives inside the backend — the handler code
(`core_create_push_config` in `router.rs`) does not see the loop.
Backends wrap `create_config` around the retry as an internal
detail.

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

- `push_delivery_store` wired + `push_dispatch_enabled=false` on
  the atomic store → `A2aServer::build()` returns a consistency
  error naming `with_push_dispatch_enabled`.
- `push_delivery_store` wired + `push_dispatch_enabled=true` → ok.
- No `push_delivery_store` + `push_dispatch_enabled=false` → ok.
- No `push_delivery_store` + `push_dispatch_enabled=true` →
  `A2aServer::build()` returns an explicit build error
  ("pending-dispatch markers would be written with no consumer").
  Test pins the exact error reason; a future
  `external_push_dispatch_consumer_enabled` opt-in would be a
  distinct trait/method, not a relaxation of this rule.

### 10.3 Late-config preservation (§8 invariant)

Causal, sequence-based, no clocks:

- Commit two non-terminal events on task T, reaching
  `latest_event_sequence = 5`.
- Register config `C1`. CAS reads 5; config stored with
  `registered_after_event_sequence = 5`.
- Commit terminal event, assigned `event_sequence = 6`; marker
  written with `event_sequence = 6`.
- Register config `C2` AFTER the terminal commit. CAS reads 6;
  config stored with `registered_after_event_sequence = 6`.
- Fan-out for the marker calls
  `list_configs_eligible_at_event(..., 6, ...)`:
  - `C1`: `5 < 6` → **eligible** (POST fires).
  - `C2`: `6 < 6` → **not eligible** (no POST).

Assert: wiremock records exactly one POST (to C1), zero to C2.

### 10.4 CAS race on `create_config`

Simulate a concurrent terminal commit during `create_config`:

- Start `create_config` for a task at `latest_event_sequence = 5`
  (CAS read returns 5).
- Inject a concurrent `update_task_status_with_events` that lands
  a terminal event, advancing `latest_event_sequence` to 6.
- Resume the `create_config` CAS; it must detect the mismatch,
  re-read `latest_event_sequence = 6`, and store the config with
  `registered_after_event_sequence = 6`.
- Fan-out for the terminal event (sequence 6) filters out the new
  config (`6 < 6` false). No POST.

Parity across all four backends.

### 10.5 Stream worker — deleted task is NOT a BatchItemFailure

- Seed marker, then delete the task (simulating admin delete).
- Invoke stream worker with the marker record.
- Assert: marker is deleted; BatchItemFailures is empty. No retry
  churn toward DLQ.

### 10.6 Stream worker — transient storage error IS a BatchItemFailure

- Inject a transient `DatabaseError` from `get_task` (wrapper store
  that fails once then succeeds).
- Invoke stream worker with the record.
- Assert: BatchItemFailures contains the SequenceNumber; second
  Lambda retry succeeds.

### 10.7 Stream worker — idempotency

- Duplicate stream record delivered twice; claim fencing makes one
  win. Exactly one POST.

### 10.8 Stream × scheduler race

- Both handlers invoked on the same marker within milliseconds.
  Claim fencing + marker-delete idempotency yield exactly one POST.

### 10.9 Scheduled worker — scan-and-fan-out

- Seed three stale markers recorded before the cutoff; scheduled
  worker processes all three; three POSTs arrive; three markers
  cleared.

### 10.10 Scheduled worker — reclaimable claims

- Existing E.17 parity test shape — unchanged.

### 10.11 Docs assertion

- README Lambda section calls out: EventBridge Scheduler mandatory;
  DDB Stream recommended on DynamoDB; `push_dispatch_enabled=true`
  required on the atomic store when push_delivery_store is wired.
- ADR-008 cross-references ADR-013.

## 11. Follow-up: amend E.20

Rev 4 keeps E.20's table, methods, and tests in place. Concrete
amendments across four coordinated changes:

**(a) Opt-in atomic gate:**
- Add `A2aAtomicStore::push_dispatch_enabled()` default method
  returning false.
- Add per-backend constructor(s) to opt in: e.g.
  `InMemoryA2aStorage::with_push_dispatch_enabled(bool)`.
- `A2aServerBuilder::build` validates both directions (§4.3): wiring
  `push_delivery_store` requires the flag on; the flag on without
  a delivery store is a build error.

**(b) Atomic marker in the commit tx:**
- Inside each atomic-store impl of `update_task_status_with_events`,
  after writing task + event rows, iterate events and write the
  marker in the same native transaction when the flag is on and
  the event is terminal `StatusUpdate`.
- Remove `record_pending_dispatch` call at the top of
  `dispatcher.rs::run_fanout`. The dispatcher still calls it in
  the redispatch path to refresh `recorded_at` (idempotent upsert).

**(c) Causal eligibility via event sequence:**
- Add `latest_event_sequence` column / attribute / field to
  `a2a_tasks` in all four backends; maintain atomically in every
  `update_task_status_with_events` and `update_task_with_events`
  as `MAX(previous, new_event_sequences)`.
- Replace rev 3's `created_at_micros` on `a2a_push_configs` with
  `registered_after_event_sequence` (BIGINT / Number / u64).
- `A2aPushNotificationStorage::create_config` internally performs
  a CAS against `a2a_tasks.latest_event_sequence` (retry loop;
  no handler changes).
- Add trait method `list_configs_eligible_at_event(tenant,
  task_id, event_sequence, ...)`. Fan-out and redispatch call
  this in place of `list_configs`.

**(d) Stream worker correctness:**
- `LambdaStreamRecoveryHandler` treats `get_task` Ok(None) as a
  permanent signal: delete the marker, return success, NO
  BatchItemFailure.
- Transient storage errors (`Err(_)`) and unparseable records DO
  surface as BatchItemFailures.

**Tests amended/added:**

- `late_config_registered_after_commit_receives_no_old_events`
  (sequence-based, rev 4 §10.3).
- `create_config_cas_retries_on_concurrent_event_commit`
  (rev 4 §10.4).
- `builder_rejects_push_dispatch_without_consumer` (rev 4 §10.2).
- `builder_rejects_push_consumer_without_dispatch_enabled`
  (rev 4 §10.2).
- Existing
  `pending_dispatch_marker_recovers_persistent_list_configs_outage`
  preserved.

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
- **Stream worker poison record.** DLQ; backstop sweep runs
  independently.
- **Retention window mismatch.** If operator sets a TTL on
  `a2a_push_pending_dispatches` shorter than the scheduler cadence,
  the backstop can miss stale markers. Builder validation SHOULD
  reject configurations where `scheduled_sweep_interval >=
  pending_dispatch_ttl / 2`. (Nice-to-have, not a release blocker.)
- **Commit write amplification.** Every commit writes one
  additional BIGINT column on `a2a_tasks` (`latest_event_sequence`),
  unconditionally, whether or not push is enabled. This is
  necessary for the causal floor to remain valid if push is enabled
  later. Cost: one integer update per commit tx — negligible.
  Push-enabled commits additionally write one marker row per
  terminal StatusUpdate.
- **Legacy configs are permissive.** Pre-migration
  `a2a_push_configs` rows have `registered_after_event_sequence = 0`
  and are eligible for any real event. This is documented migration
  behaviour, not a framework guarantee. Operators who need strict
  §8 semantics on legacy data run a one-off backfill as described
  in §6.3.
- **CAS retry storm.** Very-high-frequency event commits on a
  single task could starve `create_config` against
  `a2a_tasks.latest_event_sequence`. Bounded in practice — the
  config create is typically issued against a SUBMITTED or WORKING
  task whose commit rate is limited to handler throughput. Document
  the retry budget (recommended 5 attempts with 10ms/50ms/...ms
  backoff) and surface a `CreateConfigCasTimeout` error if
  exhausted.

## 13. Open questions

- Should `list_configs_eligible_at_event` replace `list_configs`
  on the trait? Rev 4 keeps both — CRUD endpoints (`ListTask
  PushNotificationConfigs`) need the unfiltered list; fan-out
  needs the event-sequence-filtered list. Two methods, two
  purposes.
- Should the pre-migration `registered_after_event_sequence = 0`
  default be treated as "always eligible" (rev 4 choice) or
  "never eligible"? Rev 4 picks permissive to avoid breaking
  existing deployments; adopters who need strict causal semantics
  on legacy configs run the backfill script in §6.3.
- Should the CAS retry budget on `create_config` be surfaced to
  the adopter via `RuntimeConfig`? Rev 4 hardcodes a bounded
  default; if CAS-starvation proves real in production workloads,
  promote to configurable.
- Should a separate `external_push_dispatch_consumer_enabled` flag
  ship if real external-consumer use cases emerge? Rev 4 does NOT
  ship it; the strict builder rejects the orphan-marker case.
  Revisit if an adopter files a concrete need.

## 14. Decision summary

- **Stream source:** `a2a_push_pending_dispatches` (existing, E.20).
- **Trigger topology:** DDB Stream (fast path, DynamoDB only) +
  EventBridge Scheduler (mandatory backstop, all backends).
- **Atomicity:** marker write moves into
  `A2aAtomicStore::update_task_status_with_events`, gated by an
  opt-in `push_dispatch_enabled()` flag on the storage.
- **Builder is strict:** both `push_delivery_store wired && flag
  off` and `flag on && no delivery store` are build errors. No
  orphaned configurations.
- **Late-config prevention:** CAUSAL via event sequence.
  `a2a_tasks.latest_event_sequence` maintained atomically;
  `a2a_push_configs.registered_after_event_sequence` written by a
  CAS on `create_config`; trait method
  `list_configs_eligible_at_event(..., event_sequence)`. Clock
  skew irrelevant.
- **Deleted-task handling:** marker deleted; no BatchItemFailure.
  BatchItemFailure reserved for transient errors and malformed
  records.
- **No new tables.** `a2a_push_pending_dispatches` already exists
  from E.20.
- **No new trait methods on `A2aEventStore`.** Rev 1's
  `find_terminal_event_sequence` is not added.
- **Schema additions:** `a2a_tasks.latest_event_sequence` and
  `a2a_push_configs.registered_after_event_sequence`. Both integer
  columns, DEFAULT 0 on migration. No GSI required.
- **Non-push deployments:** no marker writes, no pending_dispatch
  table required, no push_dispatch flag needed. `a2a_tasks.
  latest_event_sequence` IS maintained on every commit regardless of
  push state, because configs created AFTER push is later enabled
  need a valid causal floor on tasks that already have events. Cost
  is one additional BIGINT column update per atomic commit —
  negligible. Only the pending-dispatch MARKER write is gated by
  the opt-in flag.
- **Release gate:** Phase G (0.1.4) blocked on §7 + §8 + §10 + §11
  landing.

## 15. Agent-team review trace

Rev 4 is the output of four review cycles across a four-agent team
(storage/schema, A2A compliance, devil's advocate, synthesis):

- **Rev 1** was rejected on owner-plumbing, cross-owner scan, and
  §8 backfill grounds.
- **Rev 2** was rejected on: (a) atomic marker made push infra
  load-bearing for non-push deployments; (b) marker-pending window
  still admitted late-config backfill; (c) deleted-task test plan
  mis-classified as BatchItemFailure.
- **Rev 3** was rejected on: (a) wall-clock `created_at_micros`
  eligibility is not causal — multi-writer clock skew breaks §8
  across instance boundaries; (b) builder's "flag on, no consumer"
  case reintroduced a load-bearing-infra leak.
- **Rev 4** (this document) switches the eligibility check to the
  causal `latest_event_sequence` / `registered_after_event_sequence`
  pair maintained by the atomic store itself, and tightens the
  builder to reject the orphan-marker configuration. All three
  prior-rev defects are resolved; the core decision (stream
  pending-dispatch, scheduler backstop) is unchanged since rev 2.
