# ADR-013: Lambda Push Delivery Parity

- **Status:** Proposed
- **Date:** 2026-04-19
- **Depends on:** ADR-008 (Lambda adapter), ADR-009 (durable event coordination),
  ADR-011 (push notification delivery)
- **Supersedes (partially):** E.20 — the dedicated `a2a_push_pending_dispatches`
  table is rescinded in favor of streaming the existing events table; see
  Decision §3 and Follow-ups §11.

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
complete, may be frozen for minutes, or may never run again. `A2aServer
::run`'s sweep loops are never called under Lambda; the Lambda adapter
uses a bespoke `LambdaA2aHandler` that only translates events to the
shared axum router and back.

Today (commit `47d3694`):

- `LambdaA2aServerBuilder` has no `.push_delivery_store(...)` setter.
- The Lambda `AppState` literal hardcodes `push_delivery_store: None,
  push_dispatcher: None` so every commit-path dispatcher hook is a
  no-op.
- Neither the reclaim loop nor the pending-dispatch scan runs.
- No test or doc claims Lambda push delivery works.

This ADR decides the correct Lambda push architecture, picks a
DynamoDB-specific stream trigger, retains a scheduler-driven backstop,
and answers the question that E.20 left un-litigated: **which existing
storage artifact is the durable "this terminal event needs push fan-out"
record?**

## 2. The five logical tables

Before picking a stream source, enumerate what storage already exists.
The framework's `storage/` module exposes five logical tables; names
below are the default SQL/DynamoDB names.

| Logical table | Default name | Purpose | Trait |
|---|---|---|---|
| Tasks | `a2a_tasks` | Current task state (full Task JSON), owner, context, status, updated_at, cancel marker | `A2aTaskStorage`, `A2aCancellationSupervisor` |
| Task events | `a2a_task_events` | Append-only event stream with `event_sequence`; drives SSE / history / replay | `A2aEventStore` |
| Push configs | `a2a_push_configs` | Registered webhook configs per task (URL, auth, token) | `A2aPushNotificationStorage` |
| Push delivery claims | `a2a_push_deliveries` | Per (tenant, task_id, event_sequence, config_id) delivery state: claimant, generation, attempt count, succeeded/gave-up/abandoned diagnostics | `A2aPushDeliveryStore` |
| Pending dispatch markers | `a2a_push_pending_dispatches` | E.20 addition — durable marker that a terminal event still needs push fan-out | `A2aPushDeliveryStore` |

Notes:

- **Cancellation is NOT its own table.** It is a marker on `a2a_tasks`,
  read through `A2aCancellationSupervisor::supervisor_get_cancel_requested`.
- **Atomic task/event commits span `a2a_tasks` + `a2a_task_events`.**
  `A2aAtomicStore::update_task_status_with_events` writes both rows
  transactionally (SQL: single tx; DynamoDB: `TransactWriteItems`; in-
  memory: combined lock).
- The push-config **hooks** table (`a2a_push_configs`) stores
  webhook registrations. It is not a dispatch trigger — new config
  CRUD doesn't mean "deliver now."
- Per-recipient delivery execution state lives in `a2a_push_deliveries`,
  keyed per (event × config). Rows appear only after `claim_delivery`
  runs — which happens after `list_configs` succeeds.

## 3. The deciding question, and why E.20 answered it with the wrong table

The Lambda recovery worker (stream-triggered OR scheduler-triggered)
must be able to answer:

> Given the stream record (or the tick) at my disposal, what terminal
> events committed to storage that have NOT yet had their push
> delivery completed (or at least attempted to create claim rows)?

Five sources could carry this signal durably. Ranked:

### A. Stream `a2a_push_pending_dispatches` (E.20's design)

A dedicated marker table, one row per (tenant, task_id, event_sequence).
The stream worker consumes INSERTs; the backstop scans stale rows.

**Requirement for correctness:** the marker MUST be written atomically
with the terminal event commit. Otherwise there is a loss window
between commit return and marker insert during which a crash silently
drops the notification.

**E.20's implementation FAILS this requirement.** In
`crates/turul-a2a/src/push/dispatcher.rs` (commit `47d3694`), the call
order is:

1. `event_sink::commit_status` → `atomic_store.update_task_status_with_events`
   returns `Ok((task, seqs))` — task + event committed durably.
2. Sink calls `dispatcher.dispatch(tenant, owner, task, vec![(seq, ev)])`.
3. `dispatcher.dispatch` immediately spawns a tokio task:
   ```rust
   tokio::spawn(async move {
       self_clone.run_fanout(tenant, owner, task_id, task, terminal_seqs).await;
   });
   ```
4. Inside `run_fanout`, the FIRST await is `record_pending_dispatch`.
5. The outer await returns to the HTTP handler long before step 4 runs.

Between step 1 and step 4 there is a real loss window. On Lambda the
window is even worse: if the handler returns before the spawned task
runs even once, the Lambda environment may freeze and the marker may
never be written.

Making the marker write atomic would require either:
1. Extending `A2aAtomicStore::update_task_status_with_events` to also
   write the marker when the event is terminal (couples atomic_store
   to push semantics — bad separation), OR
2. Wrapping commit + marker in a single backend-native transaction per
   commit path (DDB `TransactWriteItems`, SQL tx, in-memory locks).
   Workable but requires every commit caller to know whether the
   event is terminal.

Both impose non-trivial design debt for a feature we already get for
free below.

### B. Stream `a2a_task_events` (the events table)

The events table is already the durable record. Every terminal
`StatusUpdate` is committed atomically with the task row via
`A2aAtomicStore`; the commit either succeeds and the row is visible,
or fails and no row appears.

A DynamoDB stream on the events table with `NEW_IMAGE` projection fires
once per committed event. The stream worker parses the record,
filters on `event_json.statusUpdate.status.state ∈ {COMPLETED, FAILED,
CANCELED, REJECTED}`, extracts `(tenant, task_id, event_sequence)`,
loads the task to fetch the owner, then runs the fan-out.

**Atomicity is free:** the event row IS the terminal-commit record.
No new loss window.

**Backstop enumeration:** `list_tasks(filter.status=Terminal)` returns
terminal tasks per tenant; for each, `get_events_after(tenant, task_id,
0)` locates the terminal event's sequence; we delegate to the same
fan-out. Idempotency is guaranteed by the existing claim fencing
(`ClaimAlreadyHeld` short-circuits re-delivery of a row already in a
terminal claim status).

Cost: one `get_task` call per stream record; backstop scan is O(terminal
tasks per tenant), bounded by the event-table retention window
(`push_failed_delivery_retention` / `task_ttl_seconds`).

### C. Stream `a2a_tasks`

Fires on every task mutation, not just terminal. Detecting "this mutation
is a terminal transition" requires comparing `OLD_IMAGE.status.state`
to `NEW_IMAGE.status.state` — noisy, and the stream record doesn't
carry `event_sequence` so the worker would need an extra read to find
the correct event. **Not preferred.**

### D. Stream `a2a_push_deliveries`

Fires on claim row creation. But claim rows only exist AFTER
`list_configs` has succeeded — this stream cannot recover the pre-claim
gap we specifically care about. **Not a viable primary trigger.**

### E. Stream `a2a_push_configs`

CRUD changes to hooks. Irrelevant for dispatch. **Not viable.**

## 4. Decision

1. **Primary trigger (DynamoDB-backed Lambda):** DynamoDB Stream on
   `a2a_task_events` with `StreamViewType::NEW_IMAGE`. The stream worker
   Lambda filters for terminal `StatusUpdate` events, loads owner via
   `get_task`, and runs the fan-out. **No new table.**
2. **Backstop (all Lambda backends):** EventBridge Scheduler-invoked
   worker Lambda. Enumerates terminal tasks via `list_tasks(status=
   Terminal)`, walks events, runs the same fan-out. Also sweeps
   `list_reclaimable_claims` for stuck claim rows (existing ADR-011 §10.5
   path).
3. **Atomicity posture:** No new durable-intent record. The events table
   is the durable intent record, atomically committed with the task
   row via the existing `A2aAtomicStore` contract.
4. **Request-Lambda role:** commit durable state only; do NOT attempt
   the HTTP POST inline. Any in-process fan-out is opportunistic best-
   effort and MUST NOT be relied on for correctness.
5. **`tokio::spawn` is not a correctness primitive under Lambda.** The
   framework treats post-return spawns as opportunistic latency
   optimisation only.
6. **E.20's `a2a_push_pending_dispatches` table and its three trait
   methods are rescinded** (see §11). The events-table stream makes them
   redundant, and their current non-atomic write semantics would be
   a latent loss window anyway.

## 5. Architecture

### 5.1 Request Lambda (commit path)

```
message:send / message:stream / :cancel / force-commit paths
  → A2aAtomicStore.update_task_status_with_events (task + event atomic)
  → handler returns response to client
  → [opportunistic] dispatcher.dispatch spawned; may run or be frozen;
    correctness does NOT depend on it completing
```

### 5.2 Stream worker Lambda

```
DynamoDB Stream on a2a_task_events (NEW_IMAGE)
  → Lambda invoked with records
  → for each record:
      parse event_json
      if !is_terminal_status_update: skip
      extract (tenant, task_id, event_sequence)
      load task via A2aTaskStorage.get_task(..., owner=<owner-from-task>)
      if task gone: mark batch-item-failure OR silently skip per policy
      else: dispatcher.run_fanout(...) — reuses existing fan-out logic
  → return BatchItemFailures with the SequenceNumbers of records that failed
```

### 5.3 Scheduled worker Lambda (backstop, required for all Lambda deployments)

```
EventBridge Scheduler cron → Lambda invoked
  → list_reclaimable_claims(limit) → dispatcher.redispatch_one per row
  → list_tasks(filter.status=Terminal, page_size=N) per tenant iteration
      → for each task:
          get_events_after(tenant, task_id, 0)
          → find terminal StatusUpdate event
          → dispatcher.run_fanout(...)
          → claim fencing short-circuits already-delivered events
          → dispatcher idempotent by design
  → return summary (events scanned, dispatched, errors)
```

### 5.4 Idempotency

Every path in §5.2 and §5.3 ultimately calls `worker.deliver(target,
payload)`, which calls `claim_delivery`. Three cases:

1. **No prior claim** — `claim_delivery` creates a Pending claim (generation=1).
2. **Live non-terminal claim exists** — `ClaimAlreadyHeld`; worker
   reports `ClaimLostOrFinal`; no POST.
3. **Terminal claim exists** — `ClaimAlreadyHeld` (terminal never
   re-claimable per ADR-011 §10); no POST, no side effects.

Duplicate stream records, stream × scheduler races, request-Lambda
opportunistic fan-out colliding with stream worker — all safe by
construction.

## 6. Storage additions (trait-level, no new tables)

Stream worker shipping requires only one backend-neutral trait
addition:

```rust
/// Find the terminal StatusUpdate event in a task's event stream,
/// if any. Used by the scheduled backstop to locate the event
/// sequence to fan out from. Returns None for non-terminal tasks.
async fn find_terminal_event_sequence(
    &self,
    tenant: &str,
    task_id: &str,
) -> Result<Option<u64>, A2aStorageError>;
```

This is added to `A2aEventStore` (not `A2aPushDeliveryStore` — it's an
event-store query). Every backend implements it by walking its existing
event rows; no schema change.

No new tables. No new columns. No new atomic-store contracts.

## 7. Lambda builder wiring

```rust
impl LambdaA2aServerBuilder {
    // NEW
    pub fn push_delivery_store(
        mut self,
        store: impl A2aPushDeliveryStore + 'static,
    ) -> Self { ... }

    // Extend existing .storage() bound to include A2aPushDeliveryStore.
}
```

On build:
- If `push_delivery_store` is present, construct `PushDeliveryWorker` +
  `PushDispatcher` exactly like the main server builder.
- Same-backend check extended to the delivery store.
- Retry-horizon validation mirrors the main server builder.
- The dispatcher is installed in the Lambda's `AppState.push_dispatcher`
  so the commit-hook call sites in `event_sink` / `router` fire it.

The dispatcher's behaviour under Lambda:
- `dispatch` continues to spawn fan-out. Correctness does not depend
  on the spawn completing — the stream worker + scheduler will recover
  any unfired delivery. This keeps the commit-path surface identical
  to the main server.

## 8. New crate surface — `turul-a2a-aws-lambda`

```rust
pub struct LambdaStreamRecoveryHandler { /* dispatcher + task_storage */ }
impl LambdaStreamRecoveryHandler {
    pub async fn handle_stream_event(
        &self,
        event: aws_lambda_events::event::dynamodb::Event,
    ) -> LambdaStreamRecoveryResponse; // BatchItemFailures on partial failure
}

pub struct LambdaScheduledRecoveryHandler { /* dispatcher + task_storage + claim store */ }
impl LambdaScheduledRecoveryHandler {
    pub async fn handle_scheduled_event(
        &self,
        event: aws_lambda_events::event::eventbridge::Event,
    ) -> LambdaScheduledRecoveryResponse;
}
```

Both handlers share a private `redispatch_terminal_event(tenant, owner,
task_id, sequence)` helper that invokes `dispatcher.run_fanout` — made
pub(crate) for this purpose.

## 9. Operator responsibilities (IaC)

The framework does NOT create AWS resources. The adopter's
CloudFormation / CDK / Terraform wires:

1. **DynamoDB Streams** on the `a2a_task_events` table (view type:
   NEW_IMAGE).
2. **Lambda event-source mapping** from the stream to the stream
   worker Lambda. Include a DLQ and a reserved concurrency ceiling.
3. **EventBridge Scheduler rule** firing the scheduled worker Lambda
   at the operator's chosen cadence. Default recommendation: every
   5 minutes.
4. **Retention policy** on the events table sufficient to cover worst-
   case scheduler outage windows.

The framework emits a `tracing::warn!` on Lambda cold-start stating
these resources are required — a loud signal if an operator forgets
to wire them.

## 10. Test plan

- **Builder:** `push_delivery_store` setter wires the dispatcher;
  absent setter leaves `push_dispatcher: None`.
- **Stream worker unit:** synthetic DDB stream event with mixed
  records (terminal StatusUpdate, non-terminal StatusUpdate, artifact
  update, non-event row). Only the terminal triggers a fan-out.
- **Stream worker partial batch:** one of three records targets a
  deleted task; assert BatchItemFailures contains only that record's
  SequenceNumber.
- **Duplicate stream record:** same record delivered twice; assert
  exactly one POST; second observes ClaimAlreadyHeld.
- **Stream × scheduler race:** both handlers invoked on the same
  terminal event within milliseconds; claim fencing makes one win,
  the other no-ops; exactly one POST.
- **Scheduled worker enumerates stuck events:** seed three terminal
  tasks, delete all their claim rows (simulating complete stream
  outage); scheduled worker runs; three POSTs arrive.
- **Scheduled worker enumerates reclaimable claims:** existing E.17
  test shape applies unchanged.
- **Docs assertion:** README Lambda section warns that push recovery
  requires EventBridge + (optionally) DDB Stream wiring.

## 11. Follow-up: revert E.20

The `a2a_push_pending_dispatches` table added in commit `47d3694` is
rescinded. Concrete changes:

- Remove `record_pending_dispatch`, `delete_pending_dispatch`,
  `list_stale_pending_dispatches` from `A2aPushDeliveryStore`.
- Remove the corresponding table / hashmap / attribute from all four
  backend impls.
- Remove the pending-dispatch arm of the reclaim sweep in
  `A2aServer::run`.
- Remove the `record_pending_dispatch` calls at the top of `run_fanout`.
- Revert
  `pending_dispatch_marker_recovers_persistent_list_configs_outage` —
  replace with an equivalent test that:
  - Uses a stream-worker simulation (pass a synthetic stream record
    directly to `LambdaStreamRecoveryHandler`).
  - Asserts the fan-out fires.
- Drop the parity test for `list_stale_pending_dispatches`.

The original motivation for E.20 — "persistent config-list failure
must not drop the terminal event" — is preserved by the events-table
stream (fires on commit, regardless of list_configs) and the
scheduler backstop (walks events independently).

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
- **Scheduled worker scan cost at scale.** `list_tasks(status=Terminal)`
  grows with retention. Mitigation: operators can tune
  `task_ttl_seconds`; scheduler default cadence is 5 min, not aggressive.
  Backend-specific indexes (a GSI on DDB) are a future optimisation if
  large-tenant deployments hit the wall.
- **Stream worker poison record.** Bad event_json or missing task could
  block a shard. Mitigation: DLQ configured by operator; backstop
  sweep runs independently of stream health.
- **Retention window mismatch.** If the events table TTL is shorter
  than the scheduler cadence, the backstop misses terminal events.
  Mitigation: builder validation rejects configurations where
  `scheduled_sweep_interval >= task_ttl_seconds / 2`.

## 13. Open questions

- Should the backstop scheduled worker use `list_tasks(status=
  Terminal)` (O(terminal tasks per tenant) per tick) or a more efficient
  secondary-index scan? For 0.1.x, list_tasks is sufficient; a DDB GSI
  is an optimisation we can defer.
- Should the framework ship an `examples/lambda-push-worker/` reference
  crate with CDK snippets, or just a README section? Lean toward a
  reference crate — operators copy-paste more reliably than they read.
- Should `A2aEventStore::find_terminal_event_sequence` instead return
  the full event (including context_id, etc.) so the worker doesn't
  re-parse? Keep it minimal for now; callers who need the full event
  can follow up with `get_events_after`.

## 14. Decision summary

- **Stream source:** `a2a_task_events` (existing), NEW_IMAGE projection.
- **Trigger topology:** DDB Stream worker Lambda (fast path) +
  EventBridge-scheduled worker Lambda (mandatory backstop).
- **New tables:** none.
- **New trait methods:** one
  (`A2aEventStore::find_terminal_event_sequence`).
- **Rescinded:** E.20's `a2a_push_pending_dispatches` table and its
  three `A2aPushDeliveryStore` methods.
- **Release gate:** Phase G blocked on this ADR being accepted and
  §11 + §7 + §8 + §10 implemented.
