# ADR-012: Cancellation Propagation to Executors

- **Status:** Accepted
- **Date:** 2026-04-17 (accepted 2026-04-18, after two rev cycles)
- **Refines:** ADR-010 §5 (cancellation integration sketch)
- **Related:** ADR-009 (durable event coordination), ADR-010 (executor EventSink and long-running lifecycle)

## Context

ADR-010 §5 sketched cross-instance cancellation with a storage-internal `cancel_requested` marker and deferred the concrete design here. The current implementation (`core_cancel_task` in `router.rs:898`) does a direct atomic write of terminal `CANCELED` with no executor involvement — that works for synchronous tasks where the executor has already returned, but fails for three scenarios:

1. **Same-instance long-running executor**. `ctx.cancellation` exists, but nothing in the framework currently trips it on `CancelTask`. The executor never sees the cancel until it completes naturally, at which point the subsequent atomic write race-loses per ADR-010 §7.1.
2. **Cross-instance cancel**. Handler on instance B, executor on instance A. B's in-flight registry is empty; A's local token is never tripped. Same stuck behavior as #1.
3. **Orphaned task cancel**. Task in non-terminal state in storage, no executor running on any instance (crash, rolling deploy, Lambda cold-restart after timeout). No one will ever emit the terminal. Direct atomic write of `CANCELED` is the only path to resolution.

This ADR settles how cancellation propagates from handler to executor across these scenarios, what the handler returns and when, and how cross-instance correctness is guaranteed without depending on cross-instance infrastructure that turul-a2a deployments may or may not have.

## Decision

### 1. Storage-Internal `cancel_requested` Marker

A new boolean marker on the task record, **never** serialized on the wire:

- **Name**: `cancel_requested` (column/attribute). Default false on task creation.
- **Set to `true`**: by `CancelTask` handlers, atomically when the request is accepted (task exists, owner matches, task is non-terminal).
- **Never cleared**: the marker is monotonic per task. On transition to terminal, the marker is irrelevant — no code path reads it on terminal tasks.
- **Wire visibility**: none. Not in `Task` proto message, not in any response, not in any JSON-RPC result. Only readable via new storage trait methods (§10).

**Why a separate marker instead of using task status**: the task stays in its current non-terminal state (`WORKING`, `INPUT_REQUIRED`, etc.) between marker-write and terminal commit. We need to distinguish "cancel requested, executor running" from "terminal CANCELED" because:
- Subscribers attached during this window expect to observe the eventual terminal event, not a pseudo-terminal cancel-requested state.
- The executor deciding whether to observe cancellation needs to read a distinct signal from task state.
- The atomic store's single-terminal-writer contract (ADR-010 §7.1) is about terminal writes — cancel-request is pre-terminal metadata.

**Per-backend shape**:
- **In-memory**: `bool` field on the task record, stored under the same write lock as the task itself.
- **SQLite / PostgreSQL**: `cancel_requested BOOLEAN NOT NULL DEFAULT FALSE` column on `a2a_tasks` table. No new table, no new index (lookups are always by task_id, which is indexed).
- **DynamoDB**: `cancelRequested` BOOL attribute on the task item. Updated via `UpdateItem` with `SET cancelRequested = :true`, conditioned on `attribute_exists(pk)` + `statusState NOT IN (…terminals…)`.

### 2. Handler Behavior — Blocking Up to Grace, Race-Aware

This refines ADR-010 §5. The `CancelTask` handler **waits** up to a configurable grace window for the cancellation to resolve, and returns the final persisted task state. The proto `CancelTask` rpc returns `Task` (`a2a.proto:64`); waiting until the task is terminal (or the grace expires) produces a more useful response than returning a still-non-terminal snapshot that the client then has to poll.

**Sequence**:

1. **Validation**: `get_task(tenant, task_id, owner)` — enforces existence + owner + tenant. Missing → 404 `TaskNotFound`. Wrong owner → also 404 (consistent with existing auth-isolation pattern). Terminal → 400 `TaskNotCancelable` (existing spec-compliant rule, unchanged).
2. **Marker write**: `A2aTaskStorage::set_cancel_requested(tenant, task_id, owner)` — conditional on task being non-terminal. If a concurrent commit pushed the task to terminal between step 1 and step 2, this returns `TaskNotCancelable` and the handler returns 400.
3. **Local wake-up** (same-instance fast path): if `AppState::in_flight[(tenant, task_id)]` exists, call `handle.cancellation.cancel()`. Executor's next check of `ctx.cancellation.is_cancelled()` returns true; executor emits `sink.cancelled(...)` or the framework commits `CANCELED` directly after grace (step 4).
4. **Grace wait with polling**: handler loops with a small internal poll (`cancel_handler_poll_interval`, default 100ms), re-reading `get_task` on each tick. Wait ends on whichever comes first:
   - **Terminal observed**: task state is now terminal (any terminal — `CANCELED`, `COMPLETED`, `FAILED`, `REJECTED`). Handler returns that task snapshot. Note: *any* terminal is accepted as resolution; a task that completes naturally between marker-write and grace-expiry is a legitimate race resolution per ADR-010 §7.1.
   - **Grace deadline**: `cancel_handler_grace` elapsed (default 5 seconds). Proceed to step 5.
5. **Framework-committed fallback**: handler attempts `A2aAtomicStore::update_task_status_with_events(CANCELED, reason = "cancellation grace expired; framework terminal commit")`. Three outcomes per ADR-010 §4.1's race-aware pattern:
   - `Ok(_)`: framework wins. Task is now `CANCELED`. Handler returns that snapshot.
   - `Err(TerminalStateAlreadySet { current_state })`: executor or another path committed first. Handler reads current task via `get_task` and returns that snapshot (may be CANCELED, COMPLETED, FAILED, or REJECTED). Handler does not retry.
   - `Err(other)`: storage error. Handler returns 500 with the error, but the marker remains set so any in-flight executor can still resolve cleanly in the background.

**Steps 4 and 5 outlive the request**: once step 3 has armed the abort (§3), the grace wait and the fallback commit run on a detached task that the handler awaits. A request future dropped mid-grace (client disconnect, transport deadline) therefore still ends in a terminal: otherwise the armed abort would stop the executor and nothing would commit, stranding the task WORKING. A storage read failure during the grace poll is logged (`turul_a2a::cancel_grace_poll_error`) and the wait continues to the fallback commit, which surfaces any persistent storage error to the caller.

**Request metadata**: `CancelTaskRequest.metadata` (`a2a.proto`: "a flexible key-value map for passing additional context or parameters") has no behaviour defined by the A2A v1.0.1 spec (§3.1.5 lists only the task id as the operative input). The framework accepts it on HTTP+JSON (request body), JSON-RPC (`params.metadata`) and gRPC alike, does not interpret it, and does not forward it to the executor; a request with metadata cancels exactly as one without.

**Why blocking**: clients expect `CancelTask` to reflect the cancel's effect in its response. A non-blocking handler returning a still-`WORKING` task forces every adopter to write poll-until-terminal logic for a basic cancel. Blocking up to 5 seconds is well within normal HTTP request timeouts and gives a 99th-percentile UX that's consistent with the proto's `return_immediately = false` default for `SendMessage`.

**Configuration**:
- `A2aServer::builder().cancel_handler_grace(Duration)` — how long the handler waits before committing CANCELED itself. Default 5s. Shared default with ADR-010's `cancellation_grace` but distinct config field.
- `A2aServer::builder().cancel_abort_grace(Duration)` — how long an executor may keep running after its token is tripped for a cancel before it is aborted (§3, "Abort after grace"). Default 5s. Independent of `cancel_handler_grace`: a deployment can give executors longer to clean up than the handler waits, or abort them sooner.
- `A2aServer::builder().cancel_handler_poll_interval(Duration)` — how often the handler re-reads task state within the grace wait. Default 100ms. Lower values reduce latency at slight storage-load cost.

### 3. In-Flight Supervisor Integration

The `InFlightHandle` from ADR-010 §4.4 already holds a `cancellation: CancellationToken`. ADR-012 adds a second way to trip this token beyond the same-instance handler path: cross-instance marker observation by the supervisor.

**Same-instance path** (ADR-010 §5, unchanged): handler on instance A calls `AppState::in_flight[(tenant, task_id)].cancellation.cancel()` directly after writing the marker. Executor on A observes the token within its next cooperation point (loop tick, `.await` between steps, etc.). No storage re-read needed for the token trip — it's in-process.

**Cross-instance path**: handler on instance B writes the marker via `set_cancel_requested`. The supervisor on instance A (where the executor is actually running) observes the marker via one of two mechanisms:

1. **Broker wake-up** (optional, fast path): if the local broker's notification for this task arrives (e.g., another event on the same task triggers it, or an optional cross-instance pub/sub propagates the cancel-request), the supervisor performs an ad-hoc marker check via `A2aCancellationSupervisor::supervisor_get_cancel_requested(tenant, task_id)`. If true, trips `handle.cancellation`.

2. **Poll fallback** (always available): supervisor batch-polls all in-flight task IDs in its local registry at `cross_instance_cancel_poll_interval` (default 1 second) via `A2aCancellationSupervisor::supervisor_list_cancel_requested(tenant, task_ids: &[String]) -> Vec<String>`. One query per tick returns the IDs with marker set; supervisor trips each matching in-flight handle's token.

The poll is per-tenant: each tick issues one query per tenant with active in-flight tasks. Storage load is O(tenants_with_alive_tasks) per polling interval, not O(alive_tasks).

**Path interaction**: both paths can trip the token. `CancellationToken::cancel()` is idempotent; multiple trips are harmless. If same-instance sets marker AND trips local token AND cross-instance poll observes marker on the same instance (rare: shouldn't happen — the registry is per-instance), the second trip is a no-op.

**Abort after grace — an executor does not outlive the cancel**: both paths trip the token through one owner, `InFlightHandle::cancel_then_abort_after(grace)` with `grace = cancel_abort_grace`. It trips the token and arms a one-shot abort that fires `grace` later. When it fires, the executor is aborted (`InFlightHandle::abort`, the same abort the blocking-send hard deadline uses, ADR-010 §4.1) unless it has by then committed a terminal state through its own sink — an executor that acknowledged the cancel is cooperating, and its post-terminal cleanup (ADR-010 §4.4) is left to finish. An interrupted commit (INPUT_REQUIRED / AUTH_REQUIRED) does not count: an executor that raised an interrupt, resumed WORKING and then ignores a cancel has not acknowledged it. The handle records the executor's own terminal commit in a flag of its own, distinct from the `yielded` signal, which fires on interrupts too. Arming is idempotent per handle: the poller re-observes the marker on every tick, and the handler and poller on one instance may both trip; only the first call arms.

- **Timing**: on the instance that runs both the handler and the executor, the abort fires `cancel_abort_grace` after the handler trips the token; with equal defaults that is the deadline of the framework CANCELED commit (§2 step 5). Cross-instance, it fires `cancel_abort_grace` after the owning instance's poller observes the marker.
- **The abort writes nothing**: the terminal stays with the handler's fallback commit (§2 step 5) or whichever writer wins the CAS. The supervisor does not treat an aborted executor as failed (ADR-010 §4.4).
- **Reach**: the abort drops the executor's future. Work the executor detached from its own future (`tokio::spawn` inside `execute`) is not reachable by it; an executor that spawns detached work owns its cancellation.
- **Why**: before this, the framework committed CANCELED and left an executor that ignored the token running, with its side effects (outbound calls, I/O, compute) continuing behind a task that reports CANCELED. Its writes fail at the CAS, so the wire stayed consistent, but the task's state contradicted what the process was doing. The blocking-send timeout already aborted at its hard deadline; cancel now follows the same rule.

**Supervisor panic safety — `SupervisorSentinel` drop-guard**: the supervisor task cannot be allowed to leak registry entries or leave executors un-aborted if it panics. The supervisor is wrapped around a sentinel that owns the cleanup responsibilities and runs them on all exit paths (normal, error, panic unwind) via `Drop`:

```rust
struct SupervisorSentinel {
    registry: Arc<InFlightRegistry>,
    key: (String, String),             // (tenant, task_id)
    spawned: Option<JoinHandle<()>>,   // the executor
    yielded_fired: Arc<AtomicBool>,
    // ... minimal state needed for cleanup ...
}

impl Drop for SupervisorSentinel {
    fn drop(&mut self) {
        // Runs on normal exit AND panic unwind — tokio task panics
        // unwind the task, which drops locals, which fires this.
        if let Some(handle) = self.spawned.take() {
            if !handle.is_finished() {
                handle.abort();  // guarantees executor cleanup
            }
        }
        self.registry.remove(&self.key);  // guarantees registry entry cleanup
        // `yielded` is NOT fired from the drop path — if the supervisor
        // panicked before a terminal committed, blocking send stays waiting
        // until its own timeout fires. Firing from here would require the
        // sentinel to hold the yielded sender, which we don't trust a
        // panicked context to do correctly.
    }
}
```

The supervisor task body runs "above" the sentinel. If the body completes normally (executor exited, state inspected, yielded fired), the sentinel's drop still runs and does cleanup — which is idempotent because the handle is already finished and the registry entry may already have been removed. If the body panics, the sentinel's drop is the floor — the executor gets aborted, the registry entry is removed, and the panic is caught by tokio's default (task aborts, does not propagate to the server).

**Observability**: supervisor panics MUST emit an ERROR-level structured tracing event at target `turul_a2a::supervisor_panic` with `tenant` and `task_id` fields. Panics indicate framework bugs and should surface loudly — they are not normal operation. The framework exports no metric for this; deployments that want a counter derive it from the tracing event. See R5 for the full policy.

This pattern guarantees: *a supervisor panic leaks nothing, aborts the executor, and makes noise.*

### 4. Cross-Instance Correctness Without Infrastructure Dependency

Following ADR-009's pattern: **storage is the source of truth; broker is the optimization**. turul-a2a deployments that run on one instance need no cross-instance infrastructure. Deployments that run across multiple instances and share storage (the only supported multi-instance pattern per ADR-009 §8) get correctness from storage polling without needing Redis, SNS, EventBridge, or any other pub/sub layer. Those are optional latency improvements, not correctness requirements.

**Worst-case cross-instance latency** under default config:
- Marker write: ≤50ms (single storage write)
- Supervisor poll cadence: ≤1000ms
- Token trip → executor observation: ≤50ms (cooperation tick)
- Executor emits terminal + commit: ≤200ms typical
- **Total**: ≤1.3s typical, ≤cancel_handler_grace (5s default) hard cap

If the executor is slow to observe or the storage is under load, the handler's grace window backstop kicks in — framework commits CANCELED, executor's late `sink.cancelled(...)` loses the race cleanly.

**With optional pub/sub layer** (e.g., deployment wires up cross-instance broker): total latency drops to ≤200ms typical. Not required, but supported via the existing broker plug-in point.

### 5. Cancel on Non-Existent In-Flight (Orphaned Tasks)

Scenario: task persisted in storage as `WORKING`, no executor running on any instance (crash, rolling deploy, Lambda cold-start after timeout). Client sends `CancelTask`.

**Handler behavior**:
1. Validation succeeds (task exists, non-terminal, owner matches).
2. Marker write succeeds.
3. Local in-flight lookup fails (empty on this instance).
4. Handler enters grace wait — no executor will ever observe the marker.
5. Grace expires.
6. Framework-committed fallback writes `CANCELED` via atomic store. Succeeds (no race, no other writer).
7. Handler returns the CANCELED task.

**Cross-instance consideration**: if instance C has the executor running, C's supervisor poll observes the marker within its poll cadence, trips the token, executor emits terminal. In this case the orphan assumption was wrong — but the behavior is still correct: either C's cooperative terminal commits first (handler's grace-end CANCELED commit race-loses, reads C's terminal), or C is slow and handler's CANCELED commits first (C's subsequent `sink.cancelled(...)` returns `EventSink is closed: terminal already set` and C's supervisor cleans up). No deadlock, no stuck state. Orphan detection is implicit in the grace-expiry + race-aware commit.

**No special "orphan" handling code**: the same grace-with-fallback path handles both genuine orphans and slow executors. This is load-bearing for the design's simplicity.

Orphans left by a dead process, which no client may ever cancel, are resolved separately by the executor lease (ADR-029, opt-in, safe with several instances), which commits `FAILED` rather than `CANCELED`.

### 6. Race Handling

Beyond the ADR-010 §7.1 single-terminal-writer contract:

- **Cancel-vs-cancel (same task)**: second handler's marker write is a no-op (marker already true). Both handlers proceed into grace wait. Whichever grace expires first tries to commit `CANCELED`. The winner commits; the loser gets `TerminalStateAlreadySet { current_state: "TASK_STATE_CANCELED" }` and reads the persisted state (which is CANCELED), returning it. Both clients observe a successful cancel — idempotent at the wire.

- **Cancel-vs-complete**: ADR-010 §7.1 covers this. Whoever commits the terminal first wins; the other emits `EventSink is closed: terminal already set`. Handler's race-aware fallback reads the persisted state.

- **Cancel-on-terminal**: pre-existing rule, unchanged. 400 `TaskNotCancelable` with `google.rpc.ErrorInfo` per ADR-004.

- **Cancel-on-transitioning-to-terminal** (narrow window): handler's step 1 reads non-terminal; by step 2's `set_cancel_requested`, the task has transitioned to terminal. The conditional-write in `set_cancel_requested` fails with a state-based error; handler converts to `TaskNotCancelable` (400). No marker is set on a terminal task.

### 7. Owner Enforcement

All cancellation operations enforce owner match:

- `set_cancel_requested` takes `owner` and the backend implementation checks the owner on the target task record (via `WHERE owner = $1` on SQL backends or a `ConditionExpression` on DynamoDB). A wrong owner surfaces as `TaskNotFound` to the caller — same anti-enumeration pattern used by `get_task`, `update_task`, and `delete_task`.
- The supervisor's `supervisor_list_cancel_requested` batch query does NOT re-check owner. Correctness comes from the fact that the supervisor only polls task IDs already in its own in-flight registry, and those entries were populated at spawn time from owner-validated requests. The query scope is `(tenant, task_id IN supervisor_set)`; authorization was enforced upstream.

Authorization model stays identical to current behavior: the handler authenticates, validates ownership at marker-write time, and operates. Cross-instance propagation rides the existing owner-scoped storage contract.

### 8. Terminal Event for Framework-Committed CANCELED

When the framework commits `CANCELED` via the fallback path (§2.5), the event written to the durable event store is a standard `TaskStatusUpdateEvent` with:

- `state = "TASK_STATE_CANCELED"`
- `message = None`. Framework-committed terminals do NOT synthesize an agent-authored message. Attributing a framework string to `ROLE_AGENT` would conflate framework telemetry with executor output for downstream consumers reading `task.history` or SSE streams — they'd see a message that claims to be from the agent but wasn't produced by the agent.
- **Storage-internal diagnostic flag** `framework_terminal = true` on the event record. Not part of the proto, not serialized to the wire, not returned by any handler. Used for server-side logging, metrics, and test assertions that want to distinguish framework-committed from executor-acknowledged cancels. Backend implementations store this as an additional column/attribute alongside the event data; absence on read defaults to false (backward-compatible with existing events).

For executor-acknowledged `CANCELED` (via `sink.cancelled(reason)`), the message carries the reason the executor provided — that is the executor's own output, correctly attributed to `ROLE_AGENT`.

**Client distinction on wire**: a client that genuinely needs to know whether the cancel was cooperative vs framework-forced can tell from the event stream — executor-acknowledged cancels have a non-empty message, framework-committed do not. This is descriptive, not prescriptive; clients generally don't need to care and the canonical contract is just "the task is CANCELED."

### 9. Cancellation-Grace-Timer vs Blocking-Timeout-Grace

ADR-010 §4.1 defines `timeout_abort_grace` for blocking-send timeouts. ADR-012 §2 defines `cancel_handler_grace` for CancelTask handlers. They serve different events:

| Config | Event | Default | Purpose |
|---|---|---|---|
| `blocking_task_timeout` | Blocking SendMessage timeout | 30s | How long a blocking send waits before giving up on the executor |
| `timeout_abort_grace` | Time between soft and hard timeout | 5s | Cooperative window after cancellation trip during timeout |
| `cancel_handler_grace` | CancelTask handler wait | 5s | How long the handler waits for cancellation to resolve before committing CANCELED |
| `cancel_abort_grace` | Abort deadline after a cancel trips the token | 5s | How long the executor gets before it is aborted (§3) |
| `cross_instance_cancel_poll_interval` | Supervisor polling | 1s | How often supervisor checks marker for cross-instance cancels |
| `cancel_handler_poll_interval` | Handler internal loop | 100ms | How often the handler re-reads task state while waiting |

Separate config fields so deployments can tune independently. Defaults share 5s where the spirit of the wait is the same ("give the executor 5 seconds to be reasonable").

### 10. Trait Evolution — Two Traits, Split by Authorization Scope

The cancel-marker storage is split across **two traits** by the authorization scope of their methods. Backends implement both. Splitting prevents handler code from accidentally calling owner-unscoped reads — a real footgun if everything lived on `A2aTaskStorage`.

**`A2aTaskStorage` (public, handler-callable)** gains one additive owner-scoped method:

```rust
#[async_trait]
pub trait A2aTaskStorage: Send + Sync {
    // ... existing methods unchanged ...

    /// Set the cancel-requested marker on a non-terminal task.
    /// Enforces owner match: wrong owner returns `TaskNotFound` (anti-enumeration).
    /// Returns `TaskNotCancelable` if the task is terminal, `TaskNotFound`
    /// if the task doesn't exist or owner doesn't match.
    /// The marker is idempotent — setting it on a task where it's already
    /// true is a successful no-op.
    async fn set_cancel_requested(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
    ) -> Result<(), A2aStorageError>;
}
```

**`A2aCancellationSupervisor` (new trait, framework-internal)** holds the unscoped read methods. Handler code cannot reach these; the supervisor reaches them because its in-flight registry entries are already owner-validated at spawn time (ADR-010 §4.4):

```rust
/// Supervisor-only cancel-marker reads. NOT for request handlers.
///
/// Authorization invariant: callers MUST operate only on task_ids that
/// are already in their own in-flight registry. Those entries were
/// owner-validated at spawn time. These methods do NOT re-check owner
/// because doing so would require the supervisor to carry per-task
/// owner strings into every poll tick — a pure overhead for a guarantee
/// that is already enforced upstream.
///
/// External backend implementers implement this trait alongside
/// `A2aTaskStorage` + `A2aEventStore` + `A2aAtomicStore`. Handler code
/// does not hold `Arc<dyn A2aCancellationSupervisor>` — only
/// `InFlightRegistry` and its supervisor task hold it.
#[async_trait]
pub trait A2aCancellationSupervisor: Send + Sync {
    /// Read the cancel-requested marker for a single task.
    /// Returns false if the marker is not set OR the task is already
    /// terminal (supervisor treats these identically — no cancellation
    /// needed). No owner check; caller guarantees authorization.
    async fn supervisor_get_cancel_requested(
        &self,
        tenant: &str,
        task_id: &str,
    ) -> Result<bool, A2aStorageError>;

    /// Batch-read: returns the subset of the given task IDs that have the
    /// cancel-requested marker set AND are not terminal. Scoped to tenant
    /// for efficient indexing. Used by the in-flight supervisor's poll tick.
    /// No owner check; caller guarantees authorization.
    async fn supervisor_list_cancel_requested(
        &self,
        tenant: &str,
        task_ids: &[String],
    ) -> Result<Vec<String>, A2aStorageError>;

    /// Set (or, with `None`, clear) the executor-lease expiry on the
    /// named tasks of one tenant. Registry-scoped: the caller passes the
    /// ids of executors running in its own process, or the tasks the
    /// reaper just resolved (ADR-029).
    async fn supervisor_renew_executor_leases(
        &self,
        tenant: &str,
        task_ids: &[String],
        expires_at_micros: Option<i64>,
    ) -> Result<(), A2aStorageError>;

    /// Every SUBMITTED / WORKING task, across all tenants and owners,
    /// whose executor lease expired before `now_micros`. Store-wide: its
    /// only caller is the lease reaper (ADR-029), which acts for the
    /// framework, not for a requester.
    async fn supervisor_list_expired_executor_leases(
        &self,
        now_micros: i64,
    ) -> Result<Vec<ExecutorHeldTask>, A2aStorageError>;
}
```

The registry-scoping authorization invariant above binds the two cancel-marker reads and the lease renewal. `supervisor_list_expired_executor_leases` is the one store-wide read; it carries no requester identity at all and is reachable only from the lease reaper (ADR-029 §1).

**AppState wiring**: `AppState::cancellation_supervisor: Arc<dyn A2aCancellationSupervisor>` is a separate field from `task_storage`. In practice, the same backend type satisfies both traits; the builder wires a single `Arc` into both fields. Cross-type safety: `AppState` does not expose `cancellation_supervisor` to handler code — it is used only by the `InFlightRegistry` constructor and the supervisor task body.

**Per-backend implementation notes**:
- **In-memory**: new `cancel_requested` field on the task record, checked under the same write lock.
- **SQLite / PostgreSQL**: new `cancel_requested BOOLEAN NOT NULL DEFAULT FALSE` column. Migrations are additive and safe to run on existing databases. Batch method: `SELECT task_id FROM a2a_tasks WHERE tenant = $1 AND task_id = ANY($2) AND cancel_requested = true AND status_state NOT IN (…terminals…)`.
- **DynamoDB**: new `cancelRequested` BOOL attribute. Batch method: `BatchGetItem` with projection on `cancelRequested`, `statusState`, filtered in application code. For large in-flight registries, split across multiple BatchGetItem calls (100 items per call limit).
- **Supervisor efficiency**: batch query cost is O(in-flight-task-count) per poll tick, not O(total-tasks). Under healthy operation with typical alive-task counts, this is negligible.

### 11. Required Tests Before GREEN

These gate ADR-012 acceptance. Timing assertions run on tokio's paused clock, so a bound such as "before the grace window" states which code path returned, not a wall-clock race.

1. **Same-instance cancel trips executor token**
   Executor started by a non-blocking send waits on `ctx.cancellation`, then acknowledges with `sink.cancelled(Some(reason))`. `CancelTask` on the same instance. Assert: the handler returns the CANCELED task carrying the executor's reason (not the framework's message-less commit), before `cancel_handler_grace` elapses; exactly one terminal event persists.

2. **Cross-instance cancel via storage marker**
   Two `AppState`s sharing storage. Executor on instance A (cooperative), with A's `run_cross_instance_cancel_poller` running. Instance B, whose in-flight registry is empty, runs the full `CancelTask` handler. Assert: B returns the CANCELED task carrying A's executor reason — A's poller observed B's marker, tripped A's token, A committed the terminal, and B's grace-wait poll observed it — before B's grace elapses; exactly one terminal event persists.

3. **Cancel on orphaned task (no in-flight)**
   Create task in WORKING state directly in storage (simulating crashed executor). No executor running. Client calls `CancelTask`. Assert: handler writes marker, enters grace, no executor responds, grace expires, framework commits CANCELED via atomic store, handler returns CANCELED task. Wall-clock approximately `cancel_handler_grace`.

4. **Cancel-vs-complete race**
   Two deterministic sub-cases:
   - **Cancel wins**: the executor ignores the token and is held on a test channel past the grace window. The framework commits CANCELED (no message) and the executor is aborted at the grace deadline (§3): its future is dropped (drop probe) and it never reaches `complete()`. Exactly one terminal (CANCELED) persists.
   - **Cancel wins, late write refused**: cross-instance, with instance A's `cancel_handler_grace` longer than B's. B commits CANCELED at its own deadline while A's abort has not yet fired; the test then opens the channel and A's executor's `complete()` returns `EventSink is closed`. Exactly one terminal (CANCELED) persists.
   - **Complete wins**: the executor answers the token trip with `complete()`, so COMPLETED lands during the handler's grace wait. The handler returns the COMPLETED task before its grace elapses and never writes CANCELED; the only terminal event is COMPLETED.

5. **Cancel-vs-cancel idempotency**
   Two simultaneous `CancelTask` handlers on the same task (possibly on different instances). Executor emits CANCELED naturally at T=100ms. Assert: both handlers return the same CANCELED task. No duplicate terminal events in the event store. Second marker write is a no-op.

6. **Cancel on terminal task returns TaskNotCancelable**
   For each terminal state (COMPLETED, FAILED, CANCELED, REJECTED — A2A v1.0.1 §3.1.5 "already completed, failed, or canceled"), on HTTP (400), JSON-RPC (−32002) and gRPC (`FAILED_PRECONDITION`), each with `google.rpc.ErrorInfo.reason = "TASK_NOT_CANCELABLE"`. A second cancel after a successful one is included. The handler never attempts the marker write (asserted through a storage wrapper counting `set_cancel_requested` calls).

7. **Cancel by wrong owner returns TaskNotFound**
   Owner A creates task. Owner B's `CancelTask` returns 404 `TaskNotFound` (not 403 — consistent with anti-enumeration auth pattern). No marker write is attempted and the supervisor read of A's live task stays false.

8. **Batch `supervisor_list_cancel_requested` parity**
   For each of in-memory, SQLite, PostgreSQL, DynamoDB: set cancel-requested on a subset of N=20 tasks across 2 tenants. Call `supervisor_list_cancel_requested(tenant_A, all_20_ids)` — returns exactly the tenant_A subset with marker set. Terminal tasks with marker set are excluded. Storage parity test.

9. **Marker write is conditional on non-terminal**
   Task in COMPLETED state. `set_cancel_requested` → returns `A2aStorageError::TaskNotCancelable` (or equivalent terminal error). Marker remains false in storage.

10. **Blocking send interaction**
    Cooperative executor reports its task id once running. Blocking send in flight (10s timeout). `CancelTask` on the same task. Assert: both calls return the CANCELED task carrying the executor's reason; the handler returns before its grace and the send before its blocking timeout; exactly one terminal event persists. No deadlock, no cross-talk between the yielded signal and the handler's grace wait.

11. **Streaming subscriber sees terminal CANCELED**
    Subscriber attached to a WORKING task; the test reads its opening Task snapshot before cancelling. Assert: the next and last event is the `TaskStatusUpdateEvent` with `state = TASK_STATE_CANCELED`, after which the SSE stream closes. In the durable store the CANCELED event's sequence is strictly greater than every earlier event, and reading the events after the pre-cancel sequence returns exactly that terminal.

12. **Cross-instance poll load under N in-flight tasks**
    N=100 in-flight tasks across two tenants on one instance, a subset with the marker set. The real poller runs three ticks on the paused clock against a supervisor that counts calls. Assert: exactly one `supervisor_list_cancel_requested` query per tenant per tick (six in total), no per-task `supervisor_get_cancel_requested` reads, and exactly the marked tasks' tokens tripped.

13. **Supervisor panic cleanup via SupervisorSentinel**
    The production supervisor body has no panic site of its own, so the test runs a supervisor-shaped task — `SupervisorSentinel` held, then `panic!` — over a real WORKING task and a real in-flight handle whose executor never returns. Assert:
    - The ERROR tracing event at `turul_a2a::supervisor_panic` carries task_id + tenant (asserted in `runtime_substrate_tests.rs` with a capturing subscriber).
    - The in-flight registry entry for this task is removed.
    - The executor was aborted: its future is dropped (awaited through a drop probe, not read from `is_finished()` immediately after `abort()`).
    - The server remains healthy — a subsequent unrelated `GetTask` succeeds.
    - `GetTask` on the task returns a WORKING snapshot (nothing committed a terminal). A subsequent `CancelTask` takes the §5 orphan-cancel path and resolves it to CANCELED with no message.

14. **Supervisor-unscoped reads are not on `A2aTaskStorage`**
    Compile-time test: handler code path attempts to call `supervisor_get_cancel_requested` via `Arc<dyn A2aTaskStorage>`. Assert: code does not compile (method is on `A2aCancellationSupervisor`, not `A2aTaskStorage`). Verified by a `compile_fail` doctest on the trait.

15. **Cancel acceptance and lookup on every transport**
    On HTTP, JSON-RPC and gRPC: an unknown task id returns `TaskNotFound` (404 / −32001 / `NOT_FOUND`); a request carrying `metadata` cancels exactly as one without (§2, request metadata). Through the handler: INPUT_REQUIRED and AUTH_REQUIRED tasks are cancelable (interrupted states are not terminal).

16. **Executor that ignores the token is aborted at the grace deadline**
    Same instance, paused clock. The executor ignores `ctx.cancellation` and sleeps in a loop. Assert: the handler returns the framework's CANCELED (no message) at the handler grace deadline; the executor is aborted at the abort grace deadline; the executor's future is dropped (drop probe); the in-flight registry entry is removed; the only terminal event is CANCELED (no FAILED from the abort).

17. **Executor that acknowledged is not aborted**
    The executor answers the token with `sink.cancelled(reason)`, then keeps running post-terminal cleanup past the grace deadline and records that the cleanup finished. Assert: the handler returns the executor's CANCELED before grace; the cleanup runs to completion (not aborted).

18. **Cross-instance: the owning instance aborts**
    Two `AppState`s sharing storage, A's poller running. A's executor ignores the token; B runs `CancelTask`. Assert: B returns the framework's CANCELED at its grace deadline; A's executor future is dropped within `cross_instance_cancel_poll_interval + cancel_handler_grace` of the marker write; A's registry entry is removed; one terminal event.

19. **End to end over HTTP**
    A real server (`bind` on port 0, short `cancel_handler_grace`) runs an executor that ignores the token. A non-blocking `SendMessage`, then `POST /tasks/{id}:cancel`. Assert: 200 with the CANCELED task, and the executor's future is dropped shortly after the grace deadline.

20. **Interrupted then resumed is still aborted**
    The executor raises AUTH_REQUIRED, resumes WORKING, then ignores a cancel. Assert: it is aborted at the grace deadline (an interrupt commit is not an acknowledgement).

21. **Dropped cancel request still commits**
    The `CancelTask` request future is dropped during the grace wait. Assert: the task still ends with exactly one CANCELED terminal and the executor is aborted.

22. **Abort deadline is its own setting**
    `cancel_abort_grace` longer than `cancel_handler_grace`: the handler returns CANCELED at its grace and the executor keeps running until the abort deadline, then is aborted. `cancel_abort_grace` shorter: the executor is aborted before the handler's CANCELED commit, which still lands as the only terminal. The cross-instance poller uses `cancel_abort_grace` too.

### 12. What Changes in Existing Code

**New**:
- `A2aStorageError` gains no new variants — existing `TaskNotCancelable` and `TaskNotFound` cover the cancel paths. `TerminalStateAlreadySet` from ADR-010 §7.1 remains the single-terminal-writer signal.
- `crates/turul-a2a/src/storage/traits.rs`:
  - One new method on `A2aTaskStorage`: `set_cancel_requested` (owner-scoped, handler-callable).
  - New trait `A2aCancellationSupervisor` with two methods: `supervisor_get_cancel_requested`, `supervisor_list_cancel_requested` (owner-unscoped, supervisor-internal).
- `crates/turul-a2a/src/storage/{memory,sqlite,postgres,dynamodb}.rs`: each backend implements BOTH traits. Single struct, two `impl` blocks.
- `crates/turul-a2a/src/storage/parity_tests.rs`: parity tests covering all three methods + the supervisor-only-scope invariant.
- `crates/turul-a2a/src/server/in_flight.rs` (added by ADR-010): cross-instance poll loop in the supervisor; `SupervisorSentinel` drop-guard (§3).
- `crates/turul-a2a/src/server/builder.rs`: new config fields `cancel_handler_grace`, `cancel_handler_poll_interval`, `cross_instance_cancel_poll_interval`.
- `crates/turul-a2a/src/server/state.rs`: new `cancellation_supervisor: Arc<dyn A2aCancellationSupervisor>` field on `AppState`, wired by the builder from the same backend instance that satisfies `A2aTaskStorage`. Handler code paths do NOT see this field on `AppState`'s public surface.
- Supervisor-panic observability is the ERROR tracing event at `turul_a2a::supervisor_panic` (§3); no metric is registered.

**Modified**:
- `crates/turul-a2a/src/router.rs`: `core_cancel_task` rewritten for the sequence in §2. Validation, marker write, local wake-up, grace wait with poll, race-aware fallback. The current direct-atomic-write is replaced.
- `A2aAtomicStore::create_task_with_events` and `update_task_status_with_events`: must also handle the new `cancel_requested` column on write (default false on create, preserved on update). Docstrings updated. Signatures unchanged.

**Unchanged**:
- `AgentExecutor` trait signature.
- `ExecutionContext.cancellation: CancellationToken` — same type, same semantics. Only difference: now it actually gets tripped by something other than server shutdown.
- Wire format — cancel-request is never serialized. The `CancelTask` response is still `Task` per proto.
- `A2aEventStore` trait — no changes.
- All existing `core_cancel_task` tests. The refactored implementation must continue to pass them plus the new ones in §11.

### 13. Risks and Mitigations

**R1 — Cross-instance polling load scales with in-flight count**. Mitigated by batch query (one per tenant per tick), not one query per task. Default 1s interval is conservative; deployments with low cancel traffic can raise to 5s or 10s. Storage backends with push notification (e.g., DynamoDB Streams) could short-circuit the poll entirely in a future optimization.

**R2 — Handler blocking up to 5s ties up connection pool**. Bounded by `cancel_handler_grace`. Deployments that see heavy concurrent cancel load can tune it down (e.g., 1s). The trade-off is **not** "occasionally returns non-terminal state" — the designed fallback (§2.5) always commits a terminal (framework-authored `CANCELED`, or whatever wins the race) before the handler returns. The trade-off is: **lower grace means more framework-forced CANCELED terminals and fewer executor-acknowledged terminals.** Slow-cooperative executors that need more than `grace` time to emit their own `sink.cancelled(reason)` get race-lost at the atomic store (§2.5(b)) — their late emit returns `EventSink is closed`. Clients always see a terminal task; the only visible difference is that framework-committed `CANCELED` carries `message = None` (§8) while executor-acknowledged `CANCELED` may carry a message explaining the cancel reason. The grace is per-handler, not global, so tuning it doesn't starve other request types. Since the abort-after-grace rule (§3), a slow-cooperative executor that has not committed its own terminal by the deadline is also aborted, so an executor that needs longer to clean up before acknowledging needs a larger `cancel_handler_grace`; cleanup after its acknowledgement is not cut short.

**R3 — Storage migration for SQLite/PostgreSQL**. Adding a `cancel_requested` column is additive and compatible with existing data (DEFAULT FALSE). Migrations SHOULD ship with the crate version that adds the column. DynamoDB needs no migration (attributes are schemaless).

**R4 — Cancel-vs-complete near boundary is non-deterministic**. Expected behavior. Both are legitimate outcomes; clients that care about distinguishing can inspect the terminal message. ADR documents this explicitly; tests cover both directions (test #4).

**R5 — Supervisor task panic must not leak registry entries or zombie executors**. The orphan-cancel path in §5 resolves *storage state* only — it does not stop a running executor or free a leaked `InFlightHandle`. Leaving panicked supervisors silently orphaned would undermine ADR-010's cleanup guarantees. Mitigated as follows:

1. **`SupervisorSentinel` drop-guard** (§3): the supervisor task body is wrapped around a sentinel whose `Drop` impl runs on all exit paths — normal completion, error, AND panic unwind. The sentinel aborts the executor's `JoinHandle` (if still running) and removes the in-flight registry entry. This is the floor: a panicked supervisor still cleans up via unwind semantics.

2. **Panic is a framework invariant violation, surfaced loudly**: supervisor panics emit an ERROR-level tracing event at `turul_a2a::supervisor_panic` with task_id + tenant fields. Deployments SHOULD alert on that event. The process does not crash (tokio's default catches panics at the task boundary) but the panic is not hidden.

3. **Storage state handling after supervisor panic**: the sentinel does NOT fire `yielded` from its drop path — a panicked context is untrusted for that delicate signal. Blocking sends still waiting on `yielded` fall through to their own `blocking_task_timeout`-based fallback (ADR-010 §4.1 timeout path), which ALSO runs via a sentinel so that the blocking send's own cleanup is panic-safe. If the task is non-terminal at the time of supervisor panic, subsequent `CancelTask` or `GetTask` calls see the non-terminal state; the §5 orphan-cancel path can finalize it.

4. **What this is NOT a replacement for**: a supervisor panic is a framework bug. The sentinel ensures no leak, but we must still find and fix the underlying cause. Acceptable as a safety net; not acceptable as a normal code path.

Taken together: panicked supervisors do not leak registry entries, do not leave zombie executors, do not silently stall blocking sends, and do not hide the bug that caused the panic. The explicit invariants are: *(a) cleanup always runs, (b) the panic is never silent, (c) other tasks are unaffected*.

**R6 — Marker set on task that subsequently terminates naturally before handler grace expires**. Handler's grace-expiry CANCELED attempt gets `TerminalStateAlreadySet { current_state: "TASK_STATE_COMPLETED" }` (or other). Handler returns the actual persisted state. Client sees the task completed despite having called cancel. This is correct behavior — the cancel request arrived but the task was already going to terminate; whichever happened first at the store wins. Same pattern as ADR-010 §7.1.

## Consequences

**Positive**:
- `ctx.cancellation` actually gets tripped by `CancelTask` — the token existed in the trait surface for versions but never served its purpose. Now it does.
- Long-running executors get cancellation propagation that matches adopter expectations.
- Cross-instance cancel works via storage polling — no infrastructure dependency.
- Handler returns the final terminal task state (or the race-won equivalent), not a non-terminal snapshot requiring client-side follow-up polling. Better UX at the cost of handler blocking.
- Orphaned-task cancel resolves cleanly via the same grace-with-fallback path — no special "orphan" code path.
- Cancellation semantics are nailed down before ADR-011, which needs to know when CANCELED events are emitted and by whom.

**Negative**:
- New storage column / attribute on all four backends (additive migration for SQL-based backends).
- Handler blocks up to 5 seconds in the worst case. Configurable; sane default.
- Cross-instance polling adds a new source of periodic storage load (one batch query per tenant per second by default).
- Storage trait surface grows: `A2aTaskStorage` gains one method; a new `A2aCancellationSupervisor` trait adds two more. Backend implementers now implement five `A2aTaskStorage` methods (was four) plus two `A2aCancellationSupervisor` methods. The split is a feature, not a cost — it prevents handler code from accidentally bypassing owner checks.
- Supervisor invariant surface is larger: `SupervisorSentinel` drop-guard requires careful state management to avoid double-free and to be safe in panic-unwind contexts. Mitigated by keeping sentinel state minimal (§3).

**Neutral**:
- The existing `core_cancel_task` implementation is replaced. The wire contract (response type, error codes, status codes) is preserved exactly — per ADR-004 and ADR-010 compliance.

## Follow-Up

- **ADR-011 (push notification delivery)** now has a nailed-down cancellation semantic to build on. Push delivery consumes the durable event stream, which includes both executor-emitted and framework-committed terminal CANCELED events. ADR-011 treats all terminal events identically regardless of origin.
- **Optional future ADR** — if supervisor polling load becomes a bottleneck at scale, an ADR for pluggable cross-instance notification (DynamoDB Streams, Redis pub/sub, SNS) can replace the poll with a push. Out of scope for 0.1.x; premature at current scale.

## Alternatives Considered

- **Mutate `status_state` directly to "CANCEL_REQUESTED"**: rejected. Not a proto-defined state; would leak on wire via `Task.status.state`. Must use a distinct storage-internal field.
- **Cross-instance via broker-only**: rejected. Requires infrastructure turul-a2a doesn't own (Redis, etc.). Storage-polling fallback is non-negotiable for correctness.
- **Event-based cancellation** (write a "cancel_requested" event to the durable event store): rejected. Events are per-task-sequence; a cancel request is a task-level mutation, not a stream event. Also, events are subscribed-to; the executor wouldn't be subscribing to its own task's events.
- **Non-blocking handler returning immediately with non-terminal snapshot** (ADR-010 §5's original proposal): rejected in favor of blocking-up-to-grace. Clients get better UX; 5s cap is bounded; race-aware fallback handles edge cases cleanly.
- **Handler commits CANCELED immediately without waiting**: rejected. Would ignore `ctx.cancellation` entirely — executor never gets a chance to acknowledge. Also defeats the grace design in ADR-010 §4.1 (for blocking-send timeout). Consistency matters: cancel and timeout both use cooperative-then-forced pattern.

## Revision log

- 2026-10-22 — HTTP status for `TaskNotCancelable` updated 409 → 400 to follow the A2A v1.0.1 error-mapping table (see ADR-004). No behavioural change to the cancellation contract.
- 2026-10-22 — Amended to what ships and what the tests prove. Supervisor-panic observability is the ERROR tracing event at `turul_a2a::supervisor_panic`; the `framework.supervisor_panics` metric was never implemented and is removed (§3, §12, R5, test #13). `CancelTaskRequest.metadata` recorded as accepted and not interpreted on every transport (§2). Required tests restated as implemented: #1, #2 (instance B runs the full handler over A's live executor), #4 (both sub-cases forced), #6 (every terminal state, all transports, no marker write), #7, #10, #11 (store sequence check; no resume), #12 (bounded correctness test of one batch query per tenant per tick, replacing the benchmark wording), #13 (tracing event, awaited abort, full follow-up), #14 (doctest), new #15 (TaskNotFound and metadata on every transport, interrupted states cancelable).
- 2026-10-25 — Abort after grace (§3): a cancel's token trip arms an abort of the executor `cancel_handler_grace` later, owned by `InFlightHandle::cancel_then_abort_after` on both the same-instance and the cross-instance path; an executor that committed its own terminal state is not aborted (an interrupt commit does not count, so the handle tracks the executor's terminal commit separately from `yielded`). The grace wait and fallback commit run detached from the request so a dropped request cannot strand the task after the abort, and a grace-poll read failure no longer ends the cancel early (§2). §10 gains `supervisor_list_executor_held_tasks`, the one store-wide supervisor read, for ADR-028. Previously an executor that ignored the token kept running behind a CANCELED task; the blocking-send timeout already aborted. `run_cross_instance_cancel_poller` takes the abort grace. §2 configuration, §9 table, R2 and test #4 (cancel wins: the executor is now aborted and never reaches `complete()`; the late-write refusal moves to a cross-instance sub-case) updated; tests #16–#21 added. §5 points restart orphans at ADR-028.
- 2026-10-25 — Abort deadline split from the handler wait: `cancel_abort_grace` (default 5s) sets when a cancelled executor is aborted, on both the handler and the cross-instance poller paths; `cancel_handler_grace` is only the handler's wait (§2, §3, §9; test #22). §5 and §10 point to ADR-029's executor lease, which replaces ADR-028's startup reconciliation; the supervisor trait loses `supervisor_list_executor_held_tasks` and gains the two lease methods.
