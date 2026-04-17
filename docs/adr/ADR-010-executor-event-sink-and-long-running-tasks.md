# ADR-010: Executor EventSink and Long-Running Task Lifecycle

- **Status:** Proposed (rev 2)
- **Date:** 2026-04-17
- **Supersedes:** ADR-009 §12 "Terminal Task Replay" — terminal subscribe returns `UnsupportedOperationError` per A2A v1.0 §3.1.6 / §9.4.6, no replay carve-out. See §4.3 below.

## Context

D3 (ADR-009) shipped durable event coordination for the **coarse event model** the framework produces: `SUBMITTED` on task creation, `WORKING` on status transition, terminal event on final state, `ArtifactUpdate` when an executor appends an artifact in a single `execute()` call. Every test executor in the suite returns synchronously, and the framework transitions `SUBMITTED → WORKING → COMPLETED` within one request cycle.

Three adopter-facing capabilities are missing:

1. **Long-running tasks**: the A2A proto specifies `return_immediately` in `SendMessageConfiguration` (`a2a.proto:160`). `return_immediately = false` (default) requires the server to block until the task reaches a terminal or interrupted state. `return_immediately = true` returns after task creation with state SUBMITTED or WORKING. The framework today has no mechanism for the second case: `execute()` runs to completion under the request future. If the executor takes five seconds, the request takes five seconds.
2. **Executor-emitted progress events**: an executor that streams tokens, reports mid-flight status ("calling tool X…"), or emits a stream of artifact chunks has no way to push events while `execute()` is running. The only events the framework writes are the ones derived from mutating `&mut Task` on return.
3. **Cancellation that the executor can observe**: `ExecutionContext.cancellation` exists (`executor.rs:28`) and the `:cancel` handler updates task state to `CANCELED`, but no code path trips the token on the running executor. Synchronous executors never see it.

ADR-009 §13 explicitly deferred "finer-grained executor events from within executor execution" to a separate ADR. This is that ADR.

**What this ADR also resolves**: the runtime boundary between framework-owned and executor-owned transitions. Cancellation propagation (ADR-012) and push delivery (ADR-011) both depend on knowing who is allowed to emit what, and when. Pinning that down here lets the later ADRs build on a fixed foundation.

## Decision

### 1. Task Lifecycle Ownership — Split, with a Single Terminal Writer

Lifecycle transitions are split between framework and executor:

**Framework-owned transitions** (always, even when an EventSink is in use):
- `SUBMITTED` on initial task creation.
- `CANCELED` as the result of a `CancelTask` that arrives *before* the executor observes the cancellation token (storage-side race winner — see §5).
- `FAILED` when an executor returns `Err(A2aError)` and the task is still non-terminal.
- Timeout-induced `FAILED` if the server's configured long-running timeout elapses (see §4).

**Executor-owned transitions** (emitted via EventSink):
- `WORKING` once execution actually begins (the framework emits this lazily if the executor does not).
- `INPUT_REQUIRED` / `AUTH_REQUIRED` — interrupted states where the executor yields back to the client.
- `COMPLETED` / `FAILED` / `CANCELED` / `REJECTED` as natural outcomes of the executor's own logic.
- All `ArtifactUpdate` events.

**On REJECTED vs FAILED**: the proto defines both as distinct terminal states (`a2a.proto:189–207`, enum value 7 for REJECTED, 4 for FAILED). They MUST NOT be conflated on the wire. Semantic distinction: `REJECTED` is "executor declines — will not perform this task" (policy refusal, guardrail, malformed-request rejection detected by the executor). `FAILED` is "executor tried and could not" (tool error, timeout, network failure). The EventSink has separate methods for each (§2).

**Single-terminal-writer invariant — enforced as a storage contract** (§3 and §7.1): for a given `(tenant, task_id)`, `A2aAtomicStore::update_task_status_with_events` MUST reject any write attempting to set a terminal state when the task is already in a terminal state, returning `A2aStorageError::TerminalStateAlreadySet { current_state }`. The check and the write occur within the same transaction on each backend. This is not a prose invariant — it is an enforced trait contract with parity tests across all four backends. Concurrent terminal emits resolve to exactly one winner; losers surface an explicit error their caller can distinguish from illegal state-machine transitions.

If the executor emits `COMPLETED` and the framework has already committed `CANCELED` from a cancel request, the executor's emit returns `A2aError::InvalidRequest { message: "EventSink is closed" }` (because the sink detects terminal-already-set via the storage error and closes itself). Conversely, if the executor's `COMPLETED` commits first, a concurrent `CancelTask` returns `TaskNotCancelable` (409). This extends the existing cancel-on-terminal rule to cover the live-execution window without changing wire semantics.

**Split rationale**: giving the executor full lifecycle control would leak framework responsibilities (initial persistence, auth/tenant scoping, request-timeout enforcement) into every executor. Giving the framework full control rules out executor-driven progress, which is the whole point of this ADR.

### 2. `EventSink` API — Proto-Defined Variants Only

A new struct `EventSink` is added to `ExecutionContext`:

```rust
#[non_exhaustive]
pub struct ExecutionContext {
    // ... existing fields ...
    /// Sink for executor-emitted progress and terminal events.
    /// Calls are durably persisted before returning.
    pub events: EventSink,
}

#[derive(Clone)]
pub struct EventSink {
    // Internal: holds tenant, task_id, owner, atomic_store handle, broker handle.
}
```

Public surface — all methods are async and each writes through `A2aAtomicStore` before returning:

```rust
impl EventSink {
    /// Emit a non-terminal status update (WORKING, INPUT_REQUIRED, AUTH_REQUIRED).
    /// Rejected (returns Err) if `state` is terminal — use complete/fail/cancelled instead.
    pub async fn set_status(&self, state: TaskState, message: Option<Message>) -> Result<u64, A2aError>;

    /// Emit an artifact chunk. `last_chunk = true` signals end of this artifact's stream.
    /// The artifact itself is not a terminal event.
    pub async fn emit_artifact(&self, artifact: Artifact, append: bool, last_chunk: bool) -> Result<u64, A2aError>;

    /// Emit terminal COMPLETED. Subsequent emits on this sink return EventSinkClosed.
    pub async fn complete(&self, final_message: Option<Message>) -> Result<u64, A2aError>;

    /// Emit terminal FAILED with an error message. Use when the executor tried to
    /// perform the task and could not (tool error, network failure, exhausted retries).
    pub async fn fail(&self, error: A2aError) -> Result<u64, A2aError>;

    /// Emit terminal CANCELED. Normally the framework calls this; executors call it when
    /// they observe cancellation via ctx.cancellation.is_cancelled() and choose to acknowledge.
    pub async fn cancelled(&self, reason: Option<String>) -> Result<u64, A2aError>;

    /// Emit terminal REJECTED. Use when the executor declines to perform the task —
    /// policy refusal, guardrail trip, malformed task detected by executor logic.
    /// Semantically distinct from `fail()`: REJECTED means "will not do this", not
    /// "tried and could not". Maps to proto TASK_STATE_REJECTED (a2a.proto enum value 7).
    pub async fn reject(&self, reason: Option<String>) -> Result<u64, A2aError>;

    /// Emit interrupted state INPUT_REQUIRED (convenience — same as set_status with that state).
    pub async fn require_input(&self, prompt: Option<Message>) -> Result<u64, A2aError>;

    /// Emit interrupted state AUTH_REQUIRED.
    pub async fn require_auth(&self, challenge: Option<Message>) -> Result<u64, A2aError>;

    /// Returns true if this sink has already emitted a terminal event.
    pub fn is_closed(&self) -> bool;
}
```

**Proto-defined variants only.** There is no escape hatch for custom event types, no `emit_custom(serde_json::Value)` method. This preserves spec compliance: every event that leaves `EventSink` maps to a proto `StreamResponse` variant (`StreamResponse` in the proto). The four terminal emits (`complete`, `fail`, `cancelled`, `reject`) map to the four proto terminal states (`COMPLETED`, `FAILED`, `CANCELED`, `REJECTED`). The compliance agent mechanically verifies this.

**Returns**: each method returns the assigned `event_sequence: u64` on success — useful for executors that want to correlate later operations with emitted events, and for tests.

**Errors**: calls on a closed sink return `A2aError::InvalidRequest { message: "EventSink is closed" }`. Calls that violate the state machine (e.g. `set_status(COMPLETED, …)`) return the same error. Backend errors surface as `A2aError::Internal`.

### 3. Ordering and Durability

**Every `EventSink` call writes through `A2aAtomicStore` before returning.** No in-memory buffering, no broker-first, no "maybe we'll flush later." The contract: when `sink.emit_artifact(…).await` returns `Ok(seq)`, that event is persisted, sequence number `seq` is assigned, and replay from `Last-Event-ID < seq` will include it.

**Terminal emits participate in the single-terminal-writer contract from §7.1.** The four terminal sink methods (`complete`, `fail`, `cancelled`, `reject`) go through `A2aAtomicStore::update_task_status_with_events`, which MUST return `TerminalStateAlreadySet` if the task is already terminal. The sink translates this error into `A2aError::InvalidRequest { message: "EventSink is closed: terminal already set" }` and sets `is_closed() = true`. No two terminal events can ever appear in the durable store for the same task.

**Broker notification is fire-and-forget after durable write.** The flow inside each sink method:

```
1. call atomic_store.update_task_status_with_events(...) or similar
2. store assigns monotonic event_sequence per task
3. on transaction commit, broker.notify(task_id)  // wake-up only, no payload
4. return Ok(sequence)
```

This matches ADR-009 §4 and §11 exactly — the broker never carries event data, and the store is always the source of truth.

**Monotonic sequence per task, same as ADR-009**. The sequence counter is already per-`(tenant, task_id)` and per-event in the atomic store. No changes to the counter semantics.

**Multiple subscribers see identical ordering**. This is already guaranteed by ADR-009 §2 and §3 (replay from durable store, single source of truth). Executor emits do not change this — they just add more events to the same ordered stream.

**No write-through → no surprise.** An executor that calls `sink.emit_artifact()` and then panics has still emitted that artifact durably. An executor that mutates its own internal state and then crashes before calling the sink has emitted nothing. The sink call IS the commit boundary.

### 4. Long-Running Execution Model

Three entry points, three behaviors — all driven by the `return_immediately` flag from the proto and the transport:

#### 4.1 Blocking `SendMessage` (`return_immediately = false`, default)

Wire: `POST /message:send` with no `returnImmediately` field, or `false`.

Behavior:
1. Framework creates task (SUBMITTED via `A2aAtomicStore::create_task_with_events`).
2. Framework spawns the executor on a tracked task handle, passes an `EventSink`.
3. Request future awaits a `DashMap<task_id, oneshot::Receiver<TerminalState>>` completion signal registered at spawn time.
4. Signal fires when **any** of: (a) executor emits a terminal or interrupted event through its sink, (b) executor returns `Err` and framework writes terminal `FAILED`, (c) framework writes terminal `CANCELED` due to a cancel request, (d) configured long-running timeout elapses and framework writes `FAILED`.
5. Framework returns the final task snapshot via `get_task(tenant, task_id, owner)`.

**Timeout — two-deadline cancellation policy**. Deployment-configurable via:
- `A2aServer::builder().blocking_task_timeout(Duration)` — soft deadline, default 30 seconds.
- `A2aServer::builder().timeout_abort_grace(Duration)` — grace between soft and hard deadline, default 5 seconds.

Sequence on timeout:

1. **Soft deadline elapses**: framework trips `handle.cancellation.cancel()` (same token passed into `ctx.cancellation`). The in-flight handle is **kept**, not removed. The blocking-send's completion oneshot has not fired yet.
2. **Cooperative window**: executor observes cancellation, calls `sink.cancelled(Some("blocking timeout — executor cancelled"))` or similar, terminal commits, `JoinHandle` resolves, registry entry removed, completion oneshot fires.
3. **Hard deadline elapses** (soft + grace, default 35s): if the executor has not yet terminated by then, framework calls `handle.spawned.abort()` on the `JoinHandle`, waits briefly for the drop-guard to fire, then commits `A2aAtomicStore::update_task_status_with_events(FAILED, reason = "blocking task timed out and did not respond to cancellation")`. Registry entry is removed when the aborted task's `JoinHandle` resolves.
4. **Return**: blocking-send returns the final task snapshot. At this point, regardless of path, the executor is no longer running.

**No zombies by design**. Every timeout path terminates the executor task either cooperatively (preferred) or by `JoinHandle::abort()` (fallback). In-flight entries persist until the `JoinHandle` actually exits, not merely until a terminal event commits — this bounds registry memory to "count of genuinely alive executor tasks."

**Why cooperation first**. `JoinHandle::abort()` is a force-unwind at the next `.await` point — fine for most async code but hostile to executors holding external resources (open file handles, in-progress HTTP requests with dangling connections, uncommitted DB transactions). The cooperative window gives well-behaved executors a chance to clean up. Misbehaving or unresponsive executors still get killed at the hard deadline; they don't get to starve the server.

**Configurable expectations**. Deployments running long LLM or agent-tool chains should set `blocking_task_timeout` deliberately (minutes, not 30s). Deployments running only short synchronous tasks can reduce it. The defaults are conservative, not prescriptive.

**Interrupted states** (`INPUT_REQUIRED`, `AUTH_REQUIRED`) count as completion for blocking sends: the request returns with the task in that state. The executor has yielded back to the client.

Interrupted states (`INPUT_REQUIRED`, `AUTH_REQUIRED`) count as completion for blocking sends: the request returns with the task in that state. The executor has yielded back to the client.

#### 4.2 Non-Blocking `SendMessage` (`return_immediately = true`)

Wire: `POST /message:send` with `configuration.returnImmediately = true`.

Behavior:
1. Framework creates task (SUBMITTED).
2. Framework spawns the executor on a tracked handle, passes an `EventSink`.
3. Framework immediately returns the task in SUBMITTED state.
4. Executor continues in the background. Subsequent `GetTask` / streaming subscriptions observe the executor's progress via durable store.

This is how polling and push-notification consumers are expected to observe completion.

#### 4.3 Streaming `SendStreamingMessage` (and `SubscribeToTask`)

Wire: `POST /message:stream`.

Behavior:
1. Framework creates task (SUBMITTED).
2. Framework spawns the executor on a tracked handle, passes an `EventSink`.
3. Framework returns a streaming response immediately. First event on the stream is the Task object (spec §3.1.6), then live durable events.
4. Subscriber observes `WORKING`, executor-emitted progress, and terminal event in order.
5. Stream closes cleanly when the terminal event is delivered.

`SubscribeToTask` on a still-running task works identically — it attaches to the same durable event stream. Replay-then-live-handoff from ADR-009 §4 already covers this.

`SubscribeToTask` on a **terminal** task returns `UnsupportedOperationError` (HTTP 400, JSON-RPC −32004). This aligns with A2A v1.0 §3.1.6 / §9.4.6: *"SubscribeToTask is used to reconnect to the event stream of a task that is not in a terminal state. If the task is already in a terminal state, the server MUST return UnsupportedOperationError."* This is also the current implementation per CLAUDE.md.

**ADR-010 explicitly supersedes ADR-009 §12 "Terminal Task Replay"**. That section proposed that terminal tasks could be subscribed to for event replay if events remained within TTL. This was never implemented, and is out of spec with A2A v1.0. It is withdrawn. `Last-Event-ID` replay applies only to streams that were active while the task was non-terminal — if a client disconnects after terminal event delivery and then reconnects with `Last-Event-ID ≥ terminal_sequence`, the server returns `UnsupportedOperationError`. Clients that need to know the terminal state after disconnection MUST either (a) track the terminal locally before disconnecting, or (b) call `GetTask` for a snapshot (which remains valid on terminal tasks). This is a history API, not a stream API.

**In-flight implication**: the new spawn-and-track path MUST check task state before accepting a `SubscribeToTask` request. Non-terminal → attach to stream. Terminal → reject with `UnsupportedOperationError`. The check runs against task storage (`get_task`), not the in-flight registry (which is only populated while the executor is alive — absence from the registry does not imply terminal).

#### 4.4 Tracked Executor Handle

A new internal `AppState::in_flight: DashMap<(tenant, task_id), InFlightHandle>` is added. `InFlightHandle` holds:

- `cancellation: CancellationToken` — the one passed into `ExecutionContext`. Tripped by the cancel handler or by timeout.
- `completion: tokio::sync::oneshot::Sender<TerminalState>` — fired by a supervisor task when the executor's `JoinHandle` resolves (cooperative exit or abort).
- `spawned: JoinHandle<()>` — the executor's background task handle.

**Entry lifetime**: entries persist from `spawn` until the `JoinHandle` actually resolves (task exits — cooperatively, via panic, or via `abort()`). They do NOT get removed merely because a terminal event has committed to storage. This is the critical fix relative to earlier drafts: the invariant "entry present ⇒ executor task is alive" must hold so that timeout can still reach into the handle to call `abort()`.

**Supervisor task per spawn**: for each executor spawn, the framework also spawns a supervisor that `.await`s the `JoinHandle`. When the handle resolves, the supervisor: (1) inspects task state — if non-terminal, commits framework `FAILED` with reason `"executor exited without terminal event"`, (2) fires the completion oneshot, (3) removes the registry entry. This guarantees cleanup on every exit path without requiring the executor to be well-behaved.

**On terminal event commit** (any source): the completion oneshot is not fired yet — it fires when the `JoinHandle` resolves, which for cooperative executors happens naturally shortly after their final sink call returns. For aborted executors, the `JoinHandle` resolves after the abort unwinds. Blocking sends await the oneshot; non-blocking and streaming sends observe completion via the durable event stream (which fires the moment the terminal commits, not when the executor exits).

**Memory bound**: entries are removed when the `JoinHandle` resolves. Upper bound is "count of concurrently alive executor tasks", not "count of tasks in non-terminal state in storage." Under healthy operation with the two-deadline timeout policy (§4.1), no entry survives longer than `blocking_task_timeout + timeout_abort_grace` after its natural completion point.

### 5. Cancellation Integration (ADR-012 Preview)

Cancellation is touched here because it depends on the same runtime boundary (`AppState::in_flight`). Full cancellation design lives in ADR-012, but ADR-010 commits to:

**`CancelTask` handler behavior**:
1. Handler verifies the task exists, is non-terminal, and owner matches.
2. Handler looks up `AppState::in_flight[(tenant, task_id)]`. If present, calls `handle.cancellation.cancel()`.
3. Handler writes a `cancel_requested = true` marker to task storage (storage-internal metadata only — never surfaces on wire). This is how cross-instance executors discover the cancel request.
4. Handler returns the current task snapshot per proto — `CancelTask` rpc returns `Task` (`a2a.proto:64`), not empty. At return time the task is typically still non-terminal; the client observes the eventual `CANCELED` via a subsequent `GetTask` or an active stream. The actual `CANCELED` terminal event is written either by:
   - The executor observing `ctx.cancellation.is_cancelled()`, calling `sink.cancelled(Some(reason))`, and committing the terminal event. OR
   - A short grace window expires and the framework commits `CANCELED` directly via `A2aAtomicStore::update_task_status_with_events` with condition `state != terminal`.

**`cancel_requested` field scope**: storage-internal only. It is NOT a proto-defined field, NOT serialized on wire, NOT returned in any API response. It exists solely so a cross-instance supervisor polling task storage can discover a cancel request that was accepted on a different instance. ADR-012 will define the exact storage shape (column/attribute name, index requirements).

**Storage-backed fallback for cross-instance cancellation**:
- If the handler runs on instance B but the executor runs on instance A, `AppState::in_flight` on B is empty. B still writes a pending-cancel marker to task storage (new `cancel_requested: bool` field or status-side metadata — concrete shape in ADR-012).
- The executor on A observes cancellation via two paths: (i) the live token if same-instance, (ii) a periodic check of the task's `cancel_requested` flag (polled by the executor-lifecycle supervisor inside the tracked handle, not the executor code itself).

**Grace window**: configurable, default 5 seconds. During the window, the framework waits for the executor to emit its own terminal. If the window expires, the framework commits `CANCELED` directly. Either way, the single-terminal-writer invariant (§1) ensures one and only one terminal event reaches the store.

**Cancel vs complete race**: the atomic store serializes terminal writes. Whichever commit reaches the store first wins. The loser's emit returns an error to the executor (which should treat it as "already terminal, done") and the loser's wire response returns the actual final state (which may be either).

### 6. Push Delivery Dependency (ADR-011 Preview)

ADR-011 (push notifications) will consume events from the **durable event store**, not from executor callbacks directly.

Why: push delivery needs to survive: (a) the originating instance dying mid-task, (b) the executor emitting ten events in rapid succession, (c) delivery retries for webhook 5xx errors. None of those work if delivery is wired to a per-execute() async hook.

ADR-011 will define a delivery worker that subscribes to the same `A2aEventStore` (+ broker wake-ups) that SSE subscribers use. An executor that emits events via `EventSink` gets those events delivered to webhooks automatically because they're in the durable store — no separate "also notify push" call needed. The same applies to framework-owned transitions (e.g., framework-committed `CANCELED`).

This ADR commits to one thing: **`EventSink` emits and framework-owned transitions are the only sources of events**, and both write through the same atomic store. ADR-011 builds on that — it does not need a new event channel.

### 7.1 Storage Contract — `A2aAtomicStore` Terminal-Write Semantics

ADR-010 adds an **explicit contract clause** to the existing `A2aAtomicStore::update_task_status_with_events` trait method. No new method is added; no method signature changes. What changes:

1. **New error variant** `A2aStorageError::TerminalStateAlreadySet { task_id: String, current_state: String }`. This is additive to the existing error enum.

2. **Contract clause** on the method's docstring (normative): *"If the task is already in a terminal state when this method is called, the implementation MUST return `TerminalStateAlreadySet` without persisting any events or mutating task state. The check and the write MUST occur within the same transaction (or equivalent atomic boundary on the backend), such that concurrent terminal-write attempts resolve to exactly one winner."*

3. **Per-backend implementation**:
   - **In-memory**: lock check + write in one critical section.
   - **SQLite / PostgreSQL**: `UPDATE a2a_tasks ... WHERE task_id = $1 AND tenant = $2 AND status_state NOT IN ('TASK_STATE_COMPLETED', 'TASK_STATE_FAILED', 'TASK_STATE_CANCELED', 'TASK_STATE_REJECTED')`. Row count == 0 → terminal already set → return error, roll back event inserts.
   - **DynamoDB**: `TransactWriteItems` with `ConditionExpression` on the task item: `attribute_not_exists(statusState) OR statusState IN (:submitted, :working, :input_required, :auth_required)`. Condition failure → return `TerminalStateAlreadySet`.

4. **Distinguished from state-machine violations**: existing state-machine errors (e.g. `SUBMITTED → INPUT_REQUIRED` where INPUT_REQUIRED is not a valid next-state) remain `InvalidStateTransition` errors. `TerminalStateAlreadySet` is specifically the "you lost the race" signal, not "you tried an illegal transition."

5. **Parity tests** gate acceptance: each backend ships a parity test that spawns N concurrent callers racing to write different terminal states. Assertion: exactly one caller returns `Ok`, N−1 callers return `TerminalStateAlreadySet`, the event store contains exactly one terminal event, and the task's persisted state matches the winning caller's write. See §9 test #12.

6. **EventSink integration**: when `EventSink::complete/fail/cancelled/reject` receives `TerminalStateAlreadySet` from the atomic store, it translates to `A2aError::InvalidRequest { message: "EventSink is closed: terminal already set" }` to the executor and marks `is_closed() = true`. The executor can choose to bail out quietly or continue processing with the understanding that further emits will fail.

### 7.2 Trait Evolution — Additive, No Breaking Change

`AgentExecutor::execute` signature does **not** change:

```rust
async fn execute(
    &self,
    task: &mut Task,
    message: &Message,
    ctx: &ExecutionContext,  // now has ctx.events
) -> Result<(), A2aError>;
```

`ExecutionContext` gains a new `events: EventSink` field. `ExecutionContext` is already `#[non_exhaustive]`, so this is additive under semver rules for our pre-1.0 crate — no breaking change to existing executors.

**Migration path for existing executors**:
- Synchronous "mutate &mut task then return" executors continue to work unchanged. On return, the framework inspects `task` for state changes and emits corresponding events (same as today). These executors never touch `ctx.events`.
- Executors that want executor-driven progress use `ctx.events` and may return immediately with a non-terminal task state.

**Detection rule** (for the framework): after `execute()` returns `Ok(())`:
- If `ctx.events.is_closed()` is true (a terminal was emitted), treat that terminal as the outcome.
- If `task.state()` is terminal and `ctx.events` was never used, emit the corresponding event via the sink internally (preserving legacy behavior exactly).
- If `task.state()` is non-terminal and `ctx.events` was never used AND the task is not in a legitimate interrupted state, the framework emits `FAILED` with reason "executor returned without reaching terminal state and without emitting events." This protects against silent hangs.

### 8. Scope Limits

**Not in this ADR** (deferred to future work):
- Token-by-token streaming inside a single artifact chunk. `emit_artifact` already supports the chunk model from ADR-006.
- Batched sink emits (e.g., "emit 100 artifact chunks in one transaction"). Each `emit_*` call is one transaction. Callers that need batching can use `A2aAtomicStore` directly in advanced cases, but that's outside the executor API.
- Custom event types outside the proto's `StreamResponse` variants. Proto-only, non-negotiable for spec compliance.
- Back-pressure from slow subscribers. Events commit to the store regardless of subscriber speed; slow subscribers catch up via replay (same as ADR-009 §8 policy).

### 9. Required Tests Before GREEN

These tests gate ADR-010 acceptance and phase-2 completion. All must be green on default features before moving to ADR-011.

1. **Long-running executor completes after delay**
   Executor calls `sink.set_status(WORKING).await`, sleeps 500ms, calls `sink.emit_artifact(…).await`, sleeps 500ms, calls `sink.complete(None).await`. Framework returns terminal task via blocking send. Assert: wall-clock elapsed ≥ 1s, final state is COMPLETED, three events in durable store.

2. **Client polling reaches terminal**
   Non-blocking send (`returnImmediately = true`) with the same executor. Client polls `get_task` in a loop with 100ms delay. Assert: at least one observation is in a non-terminal state (SUBMITTED or WORKING), and polling eventually returns COMPLETED within 3s.

3. **Streaming receives executor progress in order**
   Executor emits `WORKING`, `ArtifactUpdate#1`, `ArtifactUpdate#2`, `COMPLETED`. SSE client receives Task snapshot + those four events in exactly that order, with strictly increasing sequence numbers.

4. **Subscribe replay includes executor-emitted events**
   Executor emits 3 events via sink. Task completes. New client calls `SubscribeToTask` (without `Last-Event-ID`). Assert: replay delivers Task snapshot + all 4 durable events (including the 3 sink-emitted ones) + stream closes cleanly.

5. **Cancellation token reaches running executor**
   Executor loops checking `ctx.cancellation.is_cancelled()`, with an explicit tick every 50ms. `:cancel` is called on the task. Assert: executor observes cancellation within 200ms, calls `sink.cancelled(Some("user cancelled"))`, and the task's terminal event is `CANCELED` with that reason in the message.

6. **Cancel vs complete race is deterministic**
   Executor that emits `COMPLETED` immediately on startup. Client sends send + :cancel back-to-back. Assert: exactly one terminal event persists, either `COMPLETED` or `CANCELED`. Wire response to `:cancel` is either 200 (if cancel wins) or 409 TaskNotCancelable (if complete wins). The observed terminal matches the winning side.

7. **Multiple subscribers receive identical ordered events**
   Two SSE subscribers attach before executor starts. Executor emits 5 events. Assert: both subscribers receive identical event sequence (same sequence numbers, same payloads, same order).

8. **Cross-instance executor progress**
   Executor on instance A emits progress via sink. Subscriber on instance B sees all events (via shared durable store). Matches ADR-009 cross-instance verification but now with sink-emitted events.

9. **Framework-default fallback still works**
   Legacy executor that mutates `&mut task` and returns without touching `ctx.events`. Framework emits appropriate events based on task state change. All existing SSE/streaming tests continue to pass unchanged.

10. **Failed executor emits FAILED terminal**
    Executor returns `Err(A2aError::Internal { … })` without touching sink. Framework writes terminal `FAILED` with the error message. Assert: terminal event is `FAILED` and wire response to blocking send contains the task in FAILED state.

11. **Blocking timeout trips cancellation, then hard-aborts if needed**
    Two sub-cases:
    - **Cooperative**: executor loops checking `ctx.cancellation.is_cancelled()` with a 50ms tick. Blocking send configured with soft=500ms, grace=1000ms. Assert: request returns within 700ms, terminal event is `CANCELED` (executor acknowledged), `JoinHandle` has resolved, registry entry is gone.
    - **Abort fallback**: executor ignores cancellation and sleeps forever. Same timeout config. Assert: request returns within soft+grace+200ms (≤1700ms), terminal event is `FAILED` with reason mentioning timeout, `JoinHandle::abort()` was invoked, supervisor fires completion oneshot after unwind, registry entry is gone. Critically: after the test returns, poll tokio for the executor task existence — must be absent.

12. **Single-terminal-writer parity across backends**
    For each of in-memory, SQLite, PostgreSQL, DynamoDB: spawn 10 concurrent callers each attempting a different terminal write (`COMPLETED`, `FAILED`, `CANCELED`, `REJECTED`) on the same `(tenant, task_id)` that starts in WORKING. Assert: exactly one returns `Ok`, the other 9 return `A2aStorageError::TerminalStateAlreadySet` (not a generic error, not `InvalidStateTransition`). The event store contains exactly one terminal event. The persisted task state matches the winning caller's write.

13. **Subscribe on terminal task returns UnsupportedOperationError**
    Create task, complete it (any path). Client calls `SubscribeToTask` on the terminal task id. Assert: response is `UnsupportedOperationError` (HTTP 400, JSON-RPC −32004), `google.rpc.ErrorInfo.reason = "UNSUPPORTED_OPERATION"`, body references subscribe on terminal task. Verify **no** events are replayed. This test validates the §4.3 supersession of ADR-009 §12.

14. **Executor REJECTED via EventSink::reject**
    Executor calls `ctx.events.reject(Some("policy: not permitted")).await?`. Assert: terminal event state is `TASK_STATE_REJECTED` (wire name), persisted task status state is REJECTED, message contains the reason, blocking-send wire response has the task in REJECTED state. Verify this is distinct from FAILED on the wire (proto enum integer value 7, not 4).

15. **EventSink methods reject writes after terminal**
    Executor calls `sink.complete(None).await?`, then `sink.emit_artifact(...).await`. Assert: the second call returns `A2aError::InvalidRequest { message: "EventSink is closed: terminal already set" }`, no new event persists, `sink.is_closed()` returns true.

### 10. What Changes in Existing Code

**New**:
- `crates/turul-a2a/src/executor.rs`: `EventSink` struct with the 9 public methods from §2, including `reject`.
- `crates/turul-a2a/src/server/in_flight.rs`: `InFlightRegistry`, `InFlightHandle`, spawn-and-track logic, supervisor task, two-deadline timeout enforcement.
- `crates/turul-a2a/src/storage/error.rs`: new `A2aStorageError::TerminalStateAlreadySet { task_id, current_state }` variant (additive to existing enum).
- Storage parity tests for concurrent terminal writes (§9 test #12) — one file per backend.
- Router-level: subscribe-on-terminal rejection path (§4.3).
- Tests: per §9 list.

**Modified**:
- `ExecutionContext`: add `events: EventSink` field. Factory constructors (`anonymous`, and the production one used by the router) populate it.
- `A2aServer::Builder`: add `blocking_task_timeout` (default 30s), `timeout_abort_grace` (default 5s), `cancellation_grace` (default 5s for user-initiated cancel), all optional.
- `router.rs` handlers for `message:send`, `message:stream`, `tasks:cancel`, `tasks:subscribe`: use `InFlightRegistry`; subscribe checks task state for terminal and rejects per §4.3.
- `A2aAtomicStore::update_task_status_with_events` — **contract clause added to docstring** (normative). No signature change. All four backend implementations gain: conditional-write/lock-check to reject terminal-to-terminal writes with `TerminalStateAlreadySet`. See §7.1 for per-backend detail.
- `EventSink::complete/fail/cancelled/reject` — translate `TerminalStateAlreadySet` from the atomic store into `A2aError::InvalidRequest { message: "EventSink is closed: terminal already set" }` and mark the sink closed.

**Unchanged**:
- `AgentExecutor` trait signature.
- `A2aAtomicStore`, `A2aEventStore`, `A2aTaskStorage` method signatures — only docstring contracts and error variants change.
- All existing SSE / streaming tests (beyond the one that tested terminal-replay, which is updated per §4.3).
- Wire format — `EventSink` emits events that map 1:1 to the existing `StreamEvent` variants.

**ADR-009 deltas**:
- §12 "Terminal Task Replay" is superseded by ADR-010 §4.3. Document this in ADR-009 with a deprecation note pointing here, then re-mark ADR-009 as amended.

### 11. Risks and Mitigations

**R1 — Executor crashes mid-execution with non-terminal state**. Mitigation: the tracked handle's drop-guard catches panics via `AbortOnDrop` semantics and writes `FAILED` with "executor panicked" reason. The registry entry is always cleaned up on executor task exit, regardless of reason.

**R2 — Sink's atomic-store call fails** (DB down, constraint violation). Mitigation: the error surfaces from `sink.emit_*().await` directly. Executors that ignore the error are implementer bugs, but the framework still functions — the event is NOT in the store, so subscribers don't see it, and eventually the executor completes or times out. No silent half-commit.

**R3 — `DashMap` contention under high concurrency**. Mitigation: per-entry locks are short (insert on spawn, remove on terminal). Reads from `:cancel` are O(1). If profiling shows contention, switch to a sharded map. Not a launch blocker.

**R4 — Timeout and executor cleanup**. Earlier drafts allowed timeout to leave executors running as detached background tasks. That is rejected in this revision. The two-deadline policy (§4.1) guarantees: (a) executors always get a cooperative cancellation signal on timeout, (b) unresponsive executors are force-aborted via `JoinHandle::abort()`, (c) in-flight registry entries persist until the `JoinHandle` resolves, not just until a terminal event commits. Result: no zombies, no detached work, no resource leak. Tests #11a and #11b verify both paths.

**R5 — Pre-existing test executors break**. They don't. The detection rule in §7.2 preserves exact legacy behavior. The compliance agent will verify this during review of the phase-2 PR.

**R6 — Single-terminal-writer contract not enforced uniformly across backends**. Mitigated by making the contract explicit in `A2aAtomicStore::update_task_status_with_events`'s docstring (§7.1), by naming the specific backend mechanism (conditional `UPDATE` / condition expression / lock-under-write), and by gating acceptance on parity tests (§9 test #12). If a future backend implementer skips the conditional-write, the parity tests fail — not a prose TODO that can be forgotten.

**R7 — ADR-009 §12 implementation gap not surfaced earlier**. ADR-009 §12 proposed terminal-replay; CLAUDE.md and the current code reject terminal subscriptions per spec. The gap was that no one reconciled the two when ADR-009 shipped. ADR-010 closes the gap by explicit supersession and requires an amending entry on ADR-009 (see §10 "ADR-009 deltas"). Going forward, when an ADR proposes a behavior that the implementation then doesn't adopt, the ADR must be revised to reflect reality rather than documenting aspiration.

## Consequences

**Positive**:
- A2A proto's `return_immediately` semantics are correctly implemented for the first time.
- Long-running agents, streaming executors, and real cancellation all become usable.
- REJECTED becomes a first-class terminal on the sink, distinct from FAILED — spec-compliant and semantically correct.
- Timeouts no longer produce zombie executors — every path cleans up the spawned task.
- Single-terminal-writer is enforced as a storage contract with parity tests, not a prose invariant.
- Terminal subscribe aligns with A2A v1.0 §3.1.6 / §9.4.6; the ADR-009 §12 aspiration is withdrawn cleanly.
- The same durable event store that already exists is the single source of truth for every adopter-observable signal — no new event channels, no coherence worries.
- ADR-011 (push) and ADR-012 (cancel) can build on a fixed runtime model without re-litigating ownership.

**Negative**:
- New public API surface (`EventSink`) to maintain. Mitigated by `#[non_exhaustive]` on `ExecutionContext` and by proto-defined variants only on sink methods.
- Slightly more state in `AppState` (the in-flight registry with supervisor tasks). Bounded by live executor count; hard ceiling enforced by `blocking_task_timeout + timeout_abort_grace`.
- One more moving piece for adopters to learn. Mitigated by the fact that legacy executors work unchanged — the sink is only relevant for executors that want new behavior.
- A new `A2aStorageError` variant and a new contract clause on `A2aAtomicStore` change what existing backend implementations must do on terminal-to-terminal writes. Migration path: each of the four bundled backends gets the conditional-write implementation as part of phase 2.

**Neutral**:
- Two-deadline timeout policy (soft + abort grace) is new but optional. Defaults are conservative; deployments can override. Deployments running long agent chains should set them deliberately — the ADR documents this expectation.

## Follow-Up ADRs

- **ADR-011 — Push notification delivery**: builds on the durable event stream defined here. Delivery worker subscribes to the same events `EventSink` emits.
- **ADR-012 — Cancellation propagation**: deepens the `:cancel` handler / in-flight registry contract sketched in §5. Covers cross-instance, grace-window tuning, and the storage-backed cancel marker shape.

## Alternatives Considered

- **Executor-owned full lifecycle**: rejected. Would push auth/tenant scoping, initial persistence, and timeout into every executor implementation.
- **Separate trait `StreamingAgentExecutor`**: rejected. Two traits means two routing paths, two registries, two testing matrices. The additive-field approach keeps one trait with a clearly-optional capability.
- **Sink as a return value** (`execute` returns `Stream<Item = Event>`): rejected. Forces executors to model their work as a stream even when they don't want to, and makes it awkward to mix "mutate `&mut task`" with "emit events." The channel-as-handle shape is more idiomatic Rust.
- **Buffered sink with flush on execute() return**: rejected. Violates the durability guarantee of §3 and creates an awkward edge where an executor that crashes mid-execute loses emitted events.
