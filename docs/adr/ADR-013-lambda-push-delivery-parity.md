# ADR-013: Lambda Push Delivery Parity

- **Status:** Proposed (rev 2 — 2026-04-19)
- **Depends on:** ADR-008 (Lambda adapter), ADR-009 (durable event coordination),
  ADR-011 (push notification delivery), E.20 (`a2a_push_pending_dispatches`
  table)
- **Spec reference:** A2A v1.0 §9 "Push Notifications"; ADR-011 §8 "Config
  lifecycle edge cases" (no backfill invariant)

## Revision history

- **rev 1 (rejected, 2026-04-19)** — proposed streaming the
  `a2a_task_events` table and rescinding E.20's
  `a2a_push_pending_dispatches` table. Rejected by agent-team review:
  - Event rows don't carry `owner`; `A2aTaskStorage::get_task` requires
    owner, so the stream worker's "load the task to get owner" step
    was circular.
  - `list_tasks(status=Terminal)` is owner-scoped; the scheduled
    backstop's cross-owner terminal-task scan has no framework API.
  - A time-window event scan can retroactively deliver old terminal
    events to configs registered after commit time, violating
    ADR-011 §8 "no backfill."
  - Schema-enrichment cost (4 backend migrations + 1 DDB GSI + 1 new
    trait method) exceeded the marker-atomicity cost (1 extra write
    inside an existing transaction per backend).
- **rev 2 (this doc)** — keeps `a2a_push_pending_dispatches` and
  fixes its atomicity by moving the marker write from the dispatcher's
  post-commit tokio-spawned task into
  `A2aAtomicStore::update_task_status_with_events`'s existing
  transaction. Stream the pending-dispatch table directly; every
  record is real work. EventBridge Scheduler remains the mandatory
  backstop.

## 1. Context

ADR-011 specified push notification delivery for turul-a2a. Its design
assumes an always-on process: a `PushDeliveryWorker` spawned at
`A2aServer::run`, a per-instance reclaim loop polling
`list_reclaimable_claims` and (since E.20) a pending-dispatch marker
table, and a dispatcher that fires fan-out via `tokio::spawn`. None of
those assumptions survive a Lambda deployment.

AWS Lambda's execution model is request-scoped. Between invocations the
execution environment MAY be frozen indefinitely. `tokio::spawn`ed tasks
created inside an invocation are only guaranteed to run while the
handler is still awaiting — once the handler returns, a spawn may
complete, may be frozen for minutes, or may never run again.
`A2aServer::run`'s sweep loops are never called under Lambda; the
Lambda adapter uses a bespoke `LambdaA2aHandler` that only translates
events to the shared axum router and back.

Today (commit `47d3694`):

- `LambdaA2aServerBuilder` has no `.push_delivery_store(...)` setter.
- The Lambda `AppState` literal hardcodes `push_delivery_store: None,
  push_dispatcher: None` so every commit-path dispatcher hook is a
  no-op.
- Neither the reclaim loop nor the pending-dispatch scan runs.
- No test or doc claims Lambda push delivery works.

This ADR decides the correct Lambda push architecture and answers the
question that E.20 left un-litigated: **how is the "this terminal event
needs push fan-out" intent durably recorded atomically with the task
commit, so a Lambda-driven recovery path can reliably pick it up?**

## 2. The five logical tables (inventory)

Before picking a stream source, enumerate what storage already exists.
The framework's `storage/` module exposes five logical tables; names
below are the default SQL / DynamoDB names.

| Logical table | Default name | Purpose | Trait |
|---|---|---|---|
| Tasks | `a2a_tasks` | Current task state (full Task JSON), owner, context, status, updated_at, cancel marker | `A2aTaskStorage`, `A2aCancellationSupervisor` |
| Task events | `a2a_task_events` | Append-only event stream with `event_sequence`; drives SSE / history / replay | `A2aEventStore` |
| Push configs | `a2a_push_configs` | Registered webhook configs per task (URL, auth, token) — the "hooks" | `A2aPushNotificationStorage` |
| Push delivery claims | `a2a_push_deliveries` | Per (tenant, task_id, event_sequence, config_id) delivery state: claimant, generation, attempt count, succeeded/gave-up/abandoned diagnostics | `A2aPushDeliveryStore` |
| Pending dispatch markers | `a2a_push_pending_dispatches` | E.20 addition — durable marker that a terminal event still needs push fan-out | `A2aPushDeliveryStore` |

Notes:

- **Cancellation is NOT its own table.** It is a marker on `a2a_tasks`,
  read through `A2aCancellationSupervisor::supervisor_get_cancel_requested`.
- **Atomic task/event commits span `a2a_tasks` + `a2a_task_events`.**
  `A2aAtomicStore::update_task_status_with_events` writes both rows
  transactionally (SQL: single tx; DynamoDB: `TransactWriteItems`;
  in-memory: combined lock).
- `a2a_push_configs` holds webhook **registrations**. Streaming it
  would fire on config CRUD, not on terminal events — wrong trigger
  semantics. Not a dispatch source.
- `a2a_push_deliveries` holds per-recipient execution state. Rows
  appear only after `claim_delivery` has run, which is after
  `list_configs` succeeded. Streaming it cannot recover the pre-claim
  gap. Not a viable primary trigger.
- `a2a_push_pending_dispatches` is purpose-built for "this terminal
  event still needs fan-out." When its write is atomic with the
  task/event commit, it is the correct durable intent record.

## 3. Stream source options: a decision matrix

Five candidate stream sources were evaluated by the agent team. Their
assessments:

| Option | Stream source | Pros | Cons | Verdict |
|---|---|---|---|---|
| A | `a2a_push_pending_dispatches` (E.20) | Every stream record is real work. No parse-and-discard. Semantic purity preserved: markers = intent, events = history. Late-created configs cannot receive old terminal events (marker is deleted after first successful fan-out — ADR-011 §8 enforced by construction). | Requires marker write to be atomic with commit. rev 1 claimed this wasn't possible without coupling; agent team showed it IS possible with one additional write inside the existing transaction per backend. | **Selected (with atomicity fix).** |
| B | `a2a_task_events` | Events are already atomic with task commit (ADR-009). No new table. | Event rows don't carry `owner`; stream worker would need a secondary `get_task` read with no owner in scope. Scheduled backstop needs cross-owner terminal-task enumeration — no framework API. Event rows would need enriching with owner + committed_at + is_terminal + a GSI. Time-window event scan violates ADR-011 §8 (retroactive delivery to late configs). | Rejected. |
| C | `a2a_tasks` | Task row has owner. | Fires on every task mutation, not just terminal transitions. No `event_sequence` on the stream record — needs auxiliary read to `a2a_task_events`. Noisy; filter logic complex. | Rejected. |
| D | `a2a_push_deliveries` | Fires on claim row creation. | Claim rows only exist AFTER `list_configs` succeeded — this stream cannot recover the pre-claim gap that E.20 was designed to cover. | Rejected as primary. |
| E | `a2a_push_configs` | None relevant. | CRUD changes to hooks, unrelated to terminal events. | Rejected. |

### 3.1 Why atomicity of the marker write is cheap per backend

The rev 1 assumption that atomic marker writes would "couple atomic_store
to push semantics" was over-stated. In every backend the atomic-store
impl already knows about events (it's writing them); adding a third
row write inside the same native transaction is a one-line extension.
Cost per backend:

| Backend | How the marker join lands |
|---|---|
| DynamoDB | `TransactWriteItems` already combines task update + event puts. Add one more `Put` item for the pending-dispatch row when any incoming event satisfies `StreamEvent::is_terminal() && matches!(.., StatusUpdate { .. })`. Limit is 100 items per transaction; one extra is free. |
| SQLite / Postgres | `sqlx::Transaction` already wraps the task update + event inserts. Add one more `INSERT INTO a2a_push_pending_dispatches ... ON CONFLICT DO UPDATE` in the same tx. |
| In-memory | `update_task_status_with_events` already takes the `tasks` and `events` write guards. Extend to also take the `pending_dispatches` write guard and insert when the event is terminal. All three guards in consistent lock order. |

The decision flag (`is_terminal_status_update_event`) is already computable
by the caller's existing `StreamEvent` — no new field or tracking
required. `StreamEvent::is_terminal()` exists today (see
`crates/turul-a2a/src/streaming/mod.rs`).

### 3.2 Why streaming the pending-dispatch table beats streaming the events table

Beyond the atomicity / §8 arguments, two operational reasons favour A
over B:

- **Every stream record is real work.** On a busy deployment the
  events table churns with non-terminal `StatusUpdate` events, artifact
  updates, and submitted/working transitions. Streaming that table
  wastes Lambda invocation budget on parse-and-discard for records
  that produce no action. The pending-dispatch table has exactly one
  row per terminal-event fan-out; every stream record triggers fan-out.
- **Backstop enumeration is natively queryable.** `list_stale_pending_dispatches(older_than, limit)` (from E.20) already gives the exact
  enumeration the scheduled worker needs: markers past the freshness
  threshold. No cross-owner task scan, no time-window event GSI.

## 4. Decision

1. **Primary trigger (DynamoDB-backed Lambda):** DynamoDB Stream on
   `a2a_push_pending_dispatches` with `StreamViewType::NEW_IMAGE`. Every
   INSERT is a committed terminal-event dispatch intent. The stream
   worker Lambda reads `(tenant, owner, task_id, event_sequence)` from
   the row and drives the existing fan-out.
2. **Mandatory backstop (all Lambda backends):** EventBridge Scheduler-
   invoked worker Lambda. Calls `list_stale_pending_dispatches` (from
   E.20) + `list_reclaimable_claims` (from E.17) and delegates each
   row to the dispatcher. Required because DynamoDB Streams have 24h
   retention and can be stalled by poison records.
3. **Atomicity fix (global, not Lambda-only):** the marker write
   moves from the dispatcher's tokio-spawned `run_fanout` into
   `A2aAtomicStore::update_task_status_with_events`. The marker is
   written inside the same native transaction as the task update +
   event puts. Applies to ALL deployments (main server, Lambda) —
   not a Lambda-specific change.
4. **Request-Lambda role:** commit durable state only; do NOT block
   the response on HTTP POSTs. The existing opportunistic
   `tokio::spawn`ed fan-out remains for the main-server path (where
   it reliably runs) and is a free fast-path for Lambda whenever the
   execution environment stays alive long enough (best-effort, NOT
   load-bearing).
5. **`tokio::spawn` is not a correctness primitive under Lambda.**
   The framework treats post-return spawns as opportunistic latency
   optimisation only. Correctness is carried by the atomically-
   written marker + stream trigger + scheduler backstop.
6. **ADR-011 §8 "no backfill" is preserved by construction.** Markers
   are deleted at the end of `run_fanout`. A config created after a
   terminal event is committed therefore has no marker to fire
   against — the claim path short-circuits cleanly and no old event
   is retroactively POSTed. No `config_created_at` timestamp or
   registration-floor sequence is required.

## 5. Architecture

### 5.1 Request Lambda (commit path)

```
message:send / message:stream / :cancel / force-commit paths
  → A2aAtomicStore::update_task_status_with_events
        ├── update a2a_tasks                     \
        ├── append a2a_task_events                >  one native
        └── insert a2a_push_pending_dispatches   /   transaction
            (when StreamEvent::is_terminal() && StatusUpdate)
  → handler returns response to client
  → [opportunistic] dispatcher.dispatch spawned; may run or be frozen;
    correctness does NOT depend on it completing
```

The `A2aAtomicStore::update_task_status_with_events` trait surface
does not change. The caller still passes `(tenant, task_id, owner,
new_status, events)`. Inside each backend impl, after writing the
task + event rows, the impl iterates `events`; for each event where
`event.is_terminal() && matches!(event, StreamEvent::StatusUpdate { .. })`
it also writes a row to `a2a_push_pending_dispatches` with
`(tenant, owner, task_id, event_sequence, recorded_at=now_micros)`.
Failure of the marker write inside the transaction rolls the whole
transaction back — the task commit and the marker commit are either
both visible or both absent.

### 5.2 Stream worker Lambda

```
DynamoDB Stream on a2a_push_pending_dispatches (NEW_IMAGE)
  → Lambda invoked with records
  → for each record:
      parse pk/sk → (tenant, task_id, event_sequence)
      extract owner from the NEW_IMAGE
      reconstruct a PendingDispatch struct
      dispatcher.redispatch_pending(pending) — existing method
        → loads task via get_task (owner-scoped; owner available)
        → list_configs with bounded retry
        → per-config claim + POST
        → delete_pending_dispatch after fan-out completes
  → return BatchItemFailures with SequenceNumbers of any failed records
```

Every stream record is real work. No parse-and-discard. Owner is
carried on the row (E.20 stores it; the DynamoDB backend has an
`owner` attribute alongside `tenant`, `taskId`, `eventSequence`,
`recordedAtMicros`).

### 5.3 Scheduled worker Lambda (mandatory backstop)

```
EventBridge Scheduler cron → Lambda invoked
  → store.list_stale_pending_dispatches(cutoff, limit)
        → dispatcher.redispatch_pending(p) for each
  → store.list_reclaimable_claims(limit)
        → dispatcher.redispatch_one(c) for each
  → return summary (counts, errors)
```

Both enumerations are backend-native queries added by E.17 + E.20 —
no cross-owner task scan, no time-window GSI.

### 5.4 Idempotency

Every path in §5.2 and §5.3 ultimately calls `worker.deliver(target,
payload)`, which calls `claim_delivery`. Three cases, all safe:

1. **No prior claim** — `claim_delivery` creates a Pending claim
   (generation=1).
2. **Live non-terminal claim exists** — `ClaimAlreadyHeld`; worker
   reports `ClaimLostOrFinal`; no POST.
3. **Terminal claim exists** — `ClaimAlreadyHeld` (terminal rows are
   frozen per ADR-011 §10); no POST.

Duplicate stream records, stream × scheduler races, request-Lambda
opportunistic fan-out colliding with stream-worker fan-out — all safe
by construction. `delete_pending_dispatch` is idempotent
(ON CONFLICT DO NOTHING on SQL; unconditional DeleteItem on DDB;
`HashMap::remove` in-memory).

### 5.5 Late-config handling (ADR-011 §8 preserved)

Workflow: task completes at T1 → marker written atomically → stream
worker fires at T2 (shortly after) → fan-out queries
`a2a_push_configs`, finds zero configs registered → marker deleted
(no work to do). Operator registers a config at T3. No marker exists
for the old terminal event — claim path produces no claim row — no
POST is issued. Late-created configs receive only events committed
after their registration.

This matches ADR-011 §8 exactly:

> Config added after events exist (backfill): no backfill. Configs
> capture events commit-timestamped after registration. This avoids
> weird catch-up storms when a client registers many configs against
> long-running tasks.

## 6. Storage additions

**rev 1 proposed** adding `A2aEventStore::find_terminal_event_sequence`
and enriching `a2a_task_events` with owner/committed_at/is_terminal
attributes. **rev 2 requires neither.**

- No new trait methods on `A2aEventStore`.
- No schema changes on `a2a_task_events`.
- No DynamoDB GSI.
- No new trait methods on `A2aPushDeliveryStore` beyond what E.20
  already added (`record_pending_dispatch`, `delete_pending_dispatch`,
  `list_stale_pending_dispatches`).
- One behavioural change per backend inside
  `A2aAtomicStore::update_task_status_with_events` to include the
  marker in the existing transaction.

The existing `record_pending_dispatch` trait method on
`A2aPushDeliveryStore` remains — it's still useful for redispatch
paths that refresh `recorded_at` — but its call from
`dispatcher.rs::run_fanout` at dispatch time moves into the atomic
store. Downstream callers (`run_fanout` on redispatch,
`redispatch_pending`, tests) keep using it.

## 7. Lambda builder wiring

```rust
impl LambdaA2aServerBuilder {
    // NEW
    pub fn push_delivery_store(
        mut self,
        store: impl A2aPushDeliveryStore + 'static,
    ) -> Self { ... }

    // Extend existing .storage() bound to include A2aPushDeliveryStore
    // (mirrors the main server).
}
```

On build, if `push_delivery_store` is present: construct
`PushDeliveryWorker` + `PushDispatcher`, run the same-backend check
including the delivery store, run the retry-horizon validation that
the main server already does, and install the dispatcher in the
Lambda `AppState.push_dispatcher`. After this, the commit-hook call
sites in `event_sink` / `router` fire the dispatcher — but because
the atomic store now writes the marker, the dispatcher's main job on
Lambda becomes marker deletion and opportunistic POST; correctness
does not depend on the dispatcher completing before handler return.

## 8. New crate surface — `turul-a2a-aws-lambda`

```rust
pub struct LambdaStreamRecoveryHandler { /* dispatcher + push_delivery_store */ }
impl LambdaStreamRecoveryHandler {
    pub async fn handle_stream_event(
        &self,
        event: aws_lambda_events::event::dynamodb::Event,
    ) -> LambdaStreamRecoveryResponse; // BatchItemFailures on partial failure
}

pub struct LambdaScheduledRecoveryHandler { /* dispatcher + push_delivery_store */ }
impl LambdaScheduledRecoveryHandler {
    pub async fn handle_scheduled_event(
        &self,
        event: aws_lambda_events::event::eventbridge::Event,
    ) -> LambdaScheduledRecoveryResponse;
}
```

Both handlers share a private `redispatch_from_marker(pending)` helper
that unconditionally awaits `dispatcher.redispatch_pending(pending)`.
No new trait surface; consumes the existing dispatcher API.

## 9. Operator responsibilities (IaC)

The framework does NOT create AWS resources. The adopter's
CloudFormation / CDK / Terraform wires:

1. **DynamoDB Streams** on the `a2a_push_pending_dispatches` table
   (view type: NEW_IMAGE).
2. **Lambda event-source mapping** from the stream to the stream-
   worker Lambda. Include a DLQ and a reserved concurrency ceiling.
   Configure `BisectBatchOnFunctionError: true` and
   `MaximumRetryAttempts` so poison records route to the DLQ.
3. **EventBridge Scheduler rule** firing the scheduled-worker Lambda
   at the operator's chosen cadence. Default recommendation: every
   5 minutes.
4. **Retention / TTL** on `a2a_push_pending_dispatches` sufficient to
   outlive the worst-case scheduler outage window. Default TTL
   matches `push_delivery_ttl_seconds` (7 days).

The framework emits a `tracing::warn!` on Lambda cold-start when
`push_delivery_store` is wired, stating these resources are required.

## 10. Test plan

**Atomicity tests (global, not Lambda-only):**

- Parity: `update_task_status_with_events` with a terminal StatusUpdate
  event writes a row to `a2a_push_pending_dispatches` in the same
  transaction as the task + event rows. Injected transaction failure
  leaves neither row persisted (atomic rollback).
- Parity: `update_task_status_with_events` with a non-terminal status
  event does NOT write a pending_dispatch row.
- Parity: `update_task_status_with_events` with an artifact-update
  event does NOT write a pending_dispatch row.
- Parity: `update_task_with_events` (artifact-only) does NOT write a
  pending_dispatch row regardless.

**Late-config test (ADR-011 §8 preserved):**

- Task completes, marker written, fan-out runs with zero configs,
  marker deleted. Config created afterwards. Scheduled worker tick
  runs — list_stale_pending_dispatches returns empty; no POST to the
  new config. Asserts ADR-011 §8 compliance at the integration level.

**Builder tests:**

- `push_delivery_store` setter wires the dispatcher; absent setter
  leaves `push_dispatcher: None`.
- Mismatched backend rejected; horizon violation rejected (mirror
  main-server builder tests).

**Stream worker tests:**

- Synthetic DDB stream event with mixed records (valid marker
  inserts, MODIFY records, a DELETE record). Only INSERTs trigger
  redispatch.
- Partial-batch failure: one record targets a task that's been
  deleted between marker write and stream delivery; handler returns
  BatchItemFailures with only that SequenceNumber.
- Duplicate stream record: same record delivered twice; exactly one
  POST; second observes `ClaimAlreadyHeld`.
- Stream × scheduler race: both trigger on the same marker within
  milliseconds; claim fencing makes one win; exactly one POST.

**Scheduled worker tests:**

- Seed three stale pending-dispatch markers (recorded_at before
  cutoff) — scheduled worker processes all three, markers cleared.
- Seed stuck claim rows — scheduled worker redispatches via
  `redispatch_one` (existing E.17 path).

**Docs assertion:**

- README Lambda section warns that push recovery requires EventBridge
  + (optionally) DDB Stream wiring. ADR-008 cross-references ADR-013.

## 11. Follow-up: amend E.20 (do not revert)

rev 1 proposed reverting E.20. **rev 2 keeps E.20's table and methods
in place and instead amends the atomicity contract.** Concrete changes:

- In each `A2aAtomicStore::update_task_status_with_events` impl, after
  writing the task + event rows in the existing transaction, iterate
  the events; for each `event` where `event.is_terminal() &&
  matches!(event, StreamEvent::StatusUpdate { .. })`, write a
  pending-dispatch row `(tenant, owner, task_id, event_sequence,
  recorded_at_micros=now)` in the same transaction.
- Remove the `record_pending_dispatch` call at the top of
  `dispatcher.rs::run_fanout`. (The redispatch path still calls it to
  refresh `recorded_at` when a scheduled sweep runs —
  `record_pending_dispatch` is idempotent upsert per E.20.)
- Keep `delete_pending_dispatch` at the end of `run_fanout` — marker
  deletion is still the dispatcher's responsibility after fan-out
  completes.
- Keep the existing test
  `pending_dispatch_marker_recovers_persistent_list_configs_outage`
  — its assertions still hold.
- Add the new atomicity parity test described in §10.

## 12. Risks and non-goals

**Non-goals:**

- In-Lambda periodic background tasks. `tokio::spawn` post-return is
  opportunistic only.
- Automatic provisioning of AWS resources. That's IaC territory; the
  framework ships handler code and example IaC snippets only.
- Non-DynamoDB Lambda backends (Postgres-on-Lambda etc.): scheduler-
  only recovery, scheduler-interval latency. Documented, not blocked.

**Risks:**

- **Operator forgets to wire EventBridge.** Push recovery is silently
  non-functional. Mitigation: cold-start warn, README call-out, worked
  example in `examples/`.
- **Stream worker poison record.** Bad marker row or missing task
  could block a shard. Mitigation: DLQ configured by operator;
  backstop sweep runs independently of stream health and processes
  the same markers via `list_stale_pending_dispatches`.
- **Retention window mismatch.** If the pending-dispatch table TTL is
  shorter than the scheduler cadence, the backstop misses stale
  markers. Mitigation: builder validation rejects configurations
  where `scheduled_sweep_interval >= pending_dispatch_ttl / 2`. (Future
  work — not a release blocker.)
- **Atomic-store write amplification.** Each terminal commit now does
  three atomic writes (task + event + marker) instead of two. Cost:
  one additional DDB write unit per terminal commit. For most
  deployments this is negligible; high-throughput tenants with very
  frequent terminals should monitor.

## 13. Open questions

- Should `a2a_push_pending_dispatches` be written unconditionally on
  commit, or gated on "at least one config registered for this task"?
  Gating would avoid pointless markers when a task never had a hook.
  Arguments against gating: (a) extra read on the commit path,
  (b) race where a config is added between "check configs exist" and
  "fan-out runs"; the scheduler would not recover this. **rev 2
  recommends unconditional marker write.** `run_fanout` deletes
  markers when it finds zero configs, so the lifecycle cost is short.
- Should the stream worker call `redispatch_pending` in parallel for
  records in the same batch, or serially? Start serial; concurrency
  controls live on the Lambda side (reserved concurrency, max batch
  window).

## 14. Decision summary

- **Stream source:** `a2a_push_pending_dispatches` (existing, from
  E.20), with NEW_IMAGE projection.
- **Trigger topology:** DynamoDB Stream → stream worker Lambda (fast
  path) + EventBridge-scheduled worker Lambda (mandatory backstop).
- **New tables:** none.
- **New trait methods on `A2aPushDeliveryStore`:** none.
- **New trait methods on `A2aEventStore`:** none.
- **Behavioural change:** `A2aAtomicStore::update_task_status_with_events`
  writes the marker in the same transaction as task + event rows when
  the event is a terminal `StatusUpdate`. Trait signature unchanged.
- **Preserved invariants:** ADR-009 atomic-commit contract, ADR-011 §8
  "no backfill" (by construction).
- **Release gate:** Phase G (0.1.4) is blocked on this ADR being
  accepted and §7 + §8 + §10 + §11 implemented.

## 15. Agent-team review trace (rev 2)

rev 2 was produced after a four-agent team audit on 2026-04-19:

- **Storage/schema agent** mapped the cost of event-row enrichment
  (rev 1's path) — quantified it as 4 backend migrations + 1 DDB GSI
  + 1 new trait method + schema pollution of an append-only history
  table. Concluded enrichment is feasible but expensive.
- **A2A compliance agent** confirmed push configs are first-class
  A2A v1.0 (`crates/turul-a2a-proto/proto/a2a.proto:89–139`,
  A2A v1.0 §9, ADR-011 §6) and flagged ADR-011 §8 "no backfill" as a
  framework-enforced policy. Rev 1's event-scan-on-schedule would
  violate §8 by retroactively delivering terminal events to
  late-created configs.
- **Devil's-advocate agent** argued for keeping E.20's marker table
  and fixing atomicity instead. Decisive point: per-backend atomicity
  cost (one additional write inside an existing transaction) is
  smaller than rev 1's enrichment cost (schema × 4 + GSI + trait
  method) AND preserves the §8 invariant by construction.
- **Synthesis (this document):** rev 2 accepts the devil's advocate's
  recommendation. §11 changes from "revert E.20" to "amend E.20 to
  move marker write into atomic store." No new trait surface. No
  schema changes. No GSI.
