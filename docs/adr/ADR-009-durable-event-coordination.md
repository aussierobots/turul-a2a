# ADR-009: Durable Event Coordination for Multi-Instance Streaming

- **Status:** Accepted (§12 amended by ADR-010, 2026-04-17)
- **Date:** 2026-04-11
- **Amendments:** §12 "Terminal Task Replay" is superseded by ADR-010 §4.3. The proposed terminal-replay carve-out was never implemented and is out of spec with A2A v1.0 §3.1.6 / §9.4.6. Terminal SubscribeToTask returns `UnsupportedOperationError`. The rest of ADR-009 (durable event store, atomic writes, replay for non-terminal tasks, broker-as-wake-up, parity requirements) remains authoritative.

## Context

Multi-instance request/response correctness is verified (distributed tests with shared storage). However, cross-instance streaming/subscription remains unsolved. The current in-process `TaskEventBroker` (`tokio::sync::broadcast`) is a local optimization for attached clients on the same instance — not a coordination mechanism.

This affects any multi-instance deployment: load-balanced binaries, ECS/Fargate, Kubernetes, Lambda, rolling deploys. The problem is the same regardless of deployment target.

**The gap:** If a subscriber connects to instance A and the task's executor runs on instance B, instance A's broker has no events to deliver. The subscriber sees nothing.

## Decision

### 1. Source of Truth: Durable Event Store

A new **durable event store** is the source of truth for streaming events. The in-process broker becomes a local delivery optimization — it accelerates delivery to clients attached to the same instance, but the event store is what guarantees correctness.

**Stored per event:**
- `tenant` — tenant scope (consistent with all other storage traits)
- `task_id` — which task this event belongs to
- `event_sequence` — monotonically increasing integer per task within tenant (assigned by the store)
- `event_type` — `status_update` or `artifact_update`
- `event_data` — serialized `StreamEvent` JSON
- `created_at` — timestamp
- `ttl` — expiration (for cleanup)

**Key shape:**
- Primary key: `(tenant, task_id, event_sequence)`
- Events are append-only within a task
- The store assigns `event_sequence` atomically (no caller-assigned IDs)
- Tenant is part of the key — events cannot collide across tenants even if task IDs overlap

**Owner:** Owner is NOT part of the event key. Owner authorization is checked at the handler level before subscribing or replaying events (same pattern as task storage — `get_task` verifies owner, then the handler accesses events for the verified task). The event store does not re-check owner on every read.

**Retention:** Events are retained for a configurable TTL (default: 24 hours). After TTL, events are eligible for cleanup. Terminal task events are retained at least until the task itself is cleaned up.

### 2. Event Identity and Ordering

Each event gets a **monotonic sequence number per task**, assigned by the event store at write time. This is the event ID used for `Last-Event-ID` replay.

**Ordering guarantees:**
- Events within a single task are strictly ordered by `event_sequence`
- Cross-task ordering is not guaranteed (tasks are independent streams)
- The store MUST NOT reorder events within a task
- Gaps in sequence numbers are allowed (e.g., after failed writes) but MUST NOT cause replay to skip events

**SSE event ID format:** `{task_id}:{event_sequence}` — parseable by both client and server for replay.

### 3. Replay Semantics

**On subscribe/reconnect:**
1. Client sends `Last-Event-ID: {tenant}:{task_id}:{sequence}` header (or equivalent for initial subscribe)
2. Server extracts tenant from the request path (same as all other operations)
3. Server queries event store: `get_events_after(tenant, task_id, sequence)`
4. Server streams historical events first, then switches to live delivery
5. If `sequence` is 0 or absent, all stored events for the task within that tenant are replayed

**If the requested replay point has expired (TTL):**
- Server returns the earliest available events, not an error
- Server MAY include a synthetic `event: replay_gap` SSE comment indicating missed events
- Client should treat this as best-effort catch-up, not a protocol error

**Event identity:** Each event is identified by `(tenant, task_id, event_sequence)`. SSE event ID format: `{task_id}:{sequence}` (tenant is implicit from the request path, not repeated in the event ID).

**Idempotency:** Clients SHOULD handle potential duplicate delivery during the transition from historical replay to live events. Clients deduplicate by tracking the last seen sequence per task.

### 4. Live Delivery vs Correctness

**Architecture layers:**

```
Producer (executor)
  → atomic_store.update_task_with_events() (correctness-critical)
  → event_broker.notify(task_id) (wake-up signal, fire-and-forget)
  → optionally notify cross-instance bus (optimization)

Subscriber (SSE client)
  → on connect: replay from event_store.get_events_after() (correctness-critical)
  → subscribe to broker wake-up notifications (optimization)
  → on wake-up: re-query event_store for new events (correctness-critical)
  → emit SSE events with id: from store sequence (correctness-critical)
  → on terminal event: close stream cleanly
```

**Correctness-critical path:** atomic store write + durable store replay. This alone is sufficient for correct streaming — everything else is latency optimization. The SSE response is always built from store data, never from broker payloads.

**Optional bus/fanout layer:** SNS, EventBridge, Redis Pub/Sub, or DynamoDB Streams can provide low-latency cross-instance notification ("wake up, new event for task X"). Without this, subscribers on other instances poll the durable store at a configurable interval (e.g., 1-5 seconds). Polling is correct but higher latency.

**Local broker role:** A wake-up signal only. When producer and subscriber are on the same instance, the broker notifies the subscriber that new data is available in the store, avoiding polling latency. The subscriber always re-reads from the store — the broker does NOT carry event data or sequence numbers. This is NOT the source of truth — just a notification mechanism.

### 5. Trait Boundary: `A2aEventStore`

A new trait, separate from `A2aTaskStorage`. Events are a different concern from task CRUD — they are append-only, sequence-ordered, and have different query patterns.

```rust
#[async_trait]
pub trait A2aEventStore: Send + Sync {
    fn backend_name(&self) -> &'static str;

    /// Append an event for a task. Returns the assigned sequence number.
    /// The store assigns the sequence atomically.
    async fn append_event(
        &self,
        tenant: &str,
        task_id: &str,
        event: StreamEvent,
    ) -> Result<u64, A2aStorageError>;

    /// Get all events for a task after a given sequence number.
    /// Returns events in sequence order. Tenant-scoped.
    async fn get_events_after(
        &self,
        tenant: &str,
        task_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, StreamEvent)>, A2aStorageError>;

    /// Get the latest event sequence number for a task.
    /// Returns 0 if no events exist. Tenant-scoped.
    async fn latest_sequence(
        &self,
        tenant: &str,
        task_id: &str,
    ) -> Result<u64, A2aStorageError>;

    /// Delete events older than TTL. Returns count of deleted events.
    async fn cleanup_expired(&self) -> Result<u64, A2aStorageError>;
}
```

This trait is backend-neutral. Implementations for DynamoDB, PostgreSQL, SQLite, and in-memory follow the same parity test pattern as `A2aTaskStorage`.

### 6. Backend Strategy

**DynamoDB:**
- Table: `a2a_task_events`
- PK: `pk` (String) — `{tenant}#{taskId}` (consistent with `a2a_tasks` key shape)
- SK: `eventSequence` (Number)
- Attributes: `tenant`, `taskId`, `eventType`, `eventData` (JSON string), `createdAt`, `ttl` (DynamoDB TTL)
- Query: `pk = :pk AND eventSequence > :seq` with `ScanIndexForward = true`
- Per-task sequence allocation: counter item at `pk = {tenant}#{taskId}`, `sk = 0` (or `#counter`). `UpdateItem` with `ADD nextSequence :one` atomically increments and returns the new value.
- **Atomic task+event commit:** `TransactWriteItems` combining: (1) counter increment, (2) event PutItem, (3) task update — all in one DynamoDB transaction. This satisfies `A2aAtomicStore`'s consistency requirement.

**PostgreSQL:**
- Table: `a2a_task_events`
- Columns: `tenant TEXT, task_id TEXT, event_sequence BIGINT, event_type TEXT, event_data JSONB, created_at TIMESTAMPTZ DEFAULT NOW()`
- Primary key: `(tenant, task_id, event_sequence)`
- **Per-task sequence allocation:** `SELECT COALESCE(MAX(event_sequence), 0) + 1 FROM a2a_task_events WHERE tenant = $1 AND task_id = $2 FOR UPDATE` within the same transaction that inserts the event and updates the task. The `FOR UPDATE` on the MAX query provides row-level locking against concurrent writers to the same `(tenant, task_id)`. No separate counter table needed — the events table itself is the sequence source.
- Query: `WHERE tenant = $1 AND task_id = $2 AND event_sequence > $3 ORDER BY event_sequence`
- The sequence is a real persisted value on each event row, not a derived view. Stable across retention/deletion — deleting old events does not renumber remaining events.
- **Why not BIGSERIAL / ROW_NUMBER():** A global BIGSERIAL is not per-task monotonic. ROW_NUMBER() is query-time derivation, not a persisted sequence — it changes when events are deleted and cannot serve as a stable `Last-Event-ID` cursor.

**SQLite:**
- Same schema as PostgreSQL
- Per-task sequence: `SELECT COALESCE(MAX(event_sequence), 0) + 1` within the same transaction. Safe under SQLite's single-writer model (only one write transaction at a time).
- Suitable for single-instance and testing

**In-memory:**
- `HashMap<String, Vec<(u64, StreamEvent)>>` with atomic counter per task
- For testing and parity verification only

### 7. HTTP and JSON-RPC Streaming Behavior

**`POST /message:stream`:**
1. Create task, write initial SUBMITTED event to durable store
2. Subscribe: replay from sequence 0 + live delivery
3. Executor writes status/artifact events to durable store as task progresses
4. Each event is also published to local broker (fast path for same-instance subscribers)
5. Stream terminates when terminal event is delivered

**`GET /tasks/{id}:subscribe`:**
1. Verify task exists and is non-terminal
2. Accept optional `Last-Event-ID` header for reconnection
3. Replay from durable store starting after the given sequence
4. Switch to live delivery (local broker or poll)
5. Stream terminates on terminal event

**JSON-RPC `SendStreamingMessage` / `SubscribeToTask`:**
- Same event model, delivered via the JSON-RPC streaming response

**Both use the same event store.** No separate event paths for HTTP vs JSON-RPC.

### 8. Failure and Retention Model

**Atomicity model:** Task state mutation and event write MUST be atomic — neither can commit without the other.

**Requirement: event store and task store share the same backend.** This is not a new constraint — turul-a2a already has each backend implementing all storage traits. Deployments use one backend (PostgreSQL, DynamoDB, SQLite), not a mix.

**Per-backend atomicity:**

- **PostgreSQL/SQLite:** Single database transaction wrapping both the task update (`a2a_tasks`) and the event append (`a2a_task_events`). If either fails, the transaction rolls back. Both tables are in the same database.
- **DynamoDB:** `TransactWriteItems` API writes both the task item update and the event item atomically. DynamoDB transactions support up to 100 items across tables in the same region.
- **In-memory:** Both maps updated under the same write lock. Trivially atomic.

**Write sequence within the transaction:**
1. Append event to event store (get sequence number)
2. Update task state in task store
3. Commit transaction
4. Publish to local broker (outside transaction, fire-and-forget optimization)

If the transaction fails, neither the task nor the event is persisted. The caller gets an error. No orphan events, no silent event loss, no task-advanced-without-event.

**Cross-backend deployments (e.g., DynamoDB tasks + PostgreSQL events) are not supported.** The event store and task store must use the same backend. This is enforced by the builder — a single storage instance implements both `A2aTaskStorage` and `A2aEventStore`.

**Backpressure:** The event store does not apply backpressure to producers. Events are fire-and-forget from the producer's perspective. Slow subscribers catch up via replay.

**Duplicate delivery:** Possible during the transition from replay to live. Clients MUST be prepared to handle duplicates. Event identity is `(task_id, event_sequence)` — deduplication is straightforward.

**TTL/Cleanup:** Events older than TTL are deleted by `cleanup_expired()`, called periodically by server maintenance. Default TTL: 24 hours. Configurable per deployment.

### 9. Verification Plan

**Single-instance streaming still passes:**
- All existing SSE tests (10 tests) continue to work unchanged
- The durable store is used alongside the local broker, not instead of it

**Two-instance subscriber-on-A / producer-on-B:**
- Create task on instance A
- Subscribe on instance B
- A's executor publishes events to durable store
- B's subscriber replays from durable store and receives events
- Verify event ordering and completeness

**Replay across reconnect:**
- Subscribe, receive some events, disconnect
- Reconnect with `Last-Event-ID`
- Verify only events after the given sequence are delivered
- Verify no duplicates in the replayed set

**Expired replay point:**
- Write events, wait for TTL (or force cleanup)
- Reconnect with expired `Last-Event-ID`
- Verify server returns available events, not an error

**Backend parity for event-store semantics:**
- Same parity test pattern as task storage
- `append_event`, `get_events_after`, `latest_sequence`, `cleanup_expired`
- Run against in-memory, SQLite, PostgreSQL, DynamoDB

### 10. Atomic Write Boundary: `A2aAtomicStore` Trait

Handler-level dual writes (`event_store.append_event()` then `task_storage.update_task()`) are **not acceptable** — even with same-backend enforcement, two separate calls have no transaction boundary in the trait surface.

A new trait, `A2aAtomicStore`, provides the backend-owned consistency boundary:

```rust
#[async_trait]
pub trait A2aAtomicStore: Send + Sync {
    fn backend_name(&self) -> &'static str;

    /// Create a task and append initial events atomically.
    async fn create_task_with_events(
        &self, tenant: &str, owner: &str,
        task: Task, events: Vec<StreamEvent>,
    ) -> Result<(Task, Vec<u64>), A2aStorageError>;

    /// Update a task's status and append events atomically.
    /// Validates state machine transition.
    async fn update_task_status_with_events(
        &self, tenant: &str, task_id: &str, owner: &str,
        new_status: TaskStatus, events: Vec<StreamEvent>,
    ) -> Result<(Task, Vec<u64>), A2aStorageError>;

    /// Full replacement update of a task and append events atomically.
    async fn update_task_with_events(
        &self, tenant: &str, owner: &str,
        task: Task, events: Vec<StreamEvent>,
    ) -> Result<Vec<u64>, A2aStorageError>;
}
```

**Guarantees:**
- No "event committed, task failed"
- No "task committed, event failed"
- Backend implementations own the consistency boundary

**Per-backend atomicity:**
- **In-memory:** All maps locked together for duration of operation (consistent lock ordering: tasks → event_counters → events)
- **PostgreSQL/SQLite:** Single database transaction wrapping both table writes
- **DynamoDB:** `TransactWriteItems` API for cross-table atomic write

**Existing `A2aTaskStorage` write methods** (`create_task`, `update_task`, `update_task_status`) remain for non-streaming paths and backward compatibility. Streaming handlers exclusively use `A2aAtomicStore` for writes.

### 11. Local Broker Role: Wake-Up Signal Only

The in-process `TaskEventBroker` is **not the event data path**. It is a wake-up signal for same-instance subscribers.

**Handler write flow:**
1. Call `atomic_store.create_task_with_events()` or `update_task_with_events()` — persist durably, get sequences
2. Call `event_broker.notify(task_id)` — fire-and-forget signal, no payload needed
3. Broker does NOT carry event data or sequence numbers

**Subscriber read flow:**
1. On connect: replay from durable store using `Last-Event-ID` (or from sequence 0)
2. Subscribe to broker wake-up notifications for the task
3. On wake-up: query durable store for events after last-seen sequence
4. Emit SSE events with `id: {task_id}:{sequence}` from store data
5. On terminal event: close stream cleanly

**Why not push events through the broker?**
- Conflates "local fanout optimization" with "durable ordering" — two concerns ADR-009 explicitly separates
- Forces sequence numbers into the broker channel type, coupling broker to store
- Breaks existing broker tests and API surface
- In-memory-only deployments carry meaningless metadata

The broker channel may carry `()` (pure signal) or remain `StreamEvent` for backward compat — the SSE formatter must never trust broker data as authoritative. The store is always re-queried.

### 12. Terminal Task Replay — **SUPERSEDED by ADR-010 §4.3 (2026-04-17)**

> **Status**: This section is superseded and retained for historical context only. The proposal below was never implemented and is out of spec with A2A v1.0 §3.1.6 / §9.4.6. Current and future behavior: terminal SubscribeToTask returns `UnsupportedOperationError` (HTTP 400 / JSON-RPC −32004). `Last-Event-ID` replay applies only to streams attached while the task is non-terminal. See ADR-010 §4.3.

~~The current `core_subscribe_to_task` rejects terminal tasks with `UnsupportedOperation`. This **cannot survive D3**: if events exist in the store for a terminal task, a subscriber (or reconnecting client) must be able to replay them.~~

~~**Proposed (superseded) semantics:**~~
- ~~Subscribe to a terminal task → replay all stored events → close stream cleanly~~
- Subscribe to a non-terminal task → replay stored events → switch to live wake-ups → close on terminal event (this part remains in force; non-terminal replay is ADR-009's core contribution)
- ~~Reconnect with `Last-Event-ID` on terminal task → replay events after that sequence → close~~

~~**Proposed (superseded) rationale:** A subscriber that disconnects during the terminal event delivery and reconnects finds no broker channel (already cleaned up). Without terminal replay, that subscriber can never retrieve the terminal event. The store has it — serve it.~~

**Actual behavior (per ADR-010 §4.3)**: clients MUST either (a) track terminal state locally before disconnecting, or (b) call `GetTask` on the task to retrieve its final state. `GetTask` remains valid on terminal tasks indefinitely (until task TTL expiry). The `SubscribeToTask` API is a stream API, not a history API.

### 13. D3 Scope: Coarse Events Only

D3 delivers durable coordination for the **existing coarse event model**:
- `SUBMITTED` event on task creation
- `WORKING` event on status transition
- Terminal event (`COMPLETED`, `FAILED`, `CANCELED`, etc.) on final state
- `ArtifactUpdate` events as produced by the executor

**Not in D3:**
- Granular intermediate events from within executor execution
- An `EventSink` handle passed to the executor for streaming token-by-token
- Redesign of the `AgentExecutor` trait

If finer-grained executor events are wanted later, that gets a **separate ADR** and a new `EventSink` parameter on `AgentExecutor::execute()`. D3 must not be blocked on executor redesign.

### 14. Required Tests Before D3 Complete

1. **Replay then live handoff** — subscribe, replay stored events, receive live event, verify no gaps
2. **Terminal task replay** — subscribe to a completed task, receive retained events, stream closes cleanly
3. **Reconnect with Last-Event-ID** — replay only newer events, no duplicates
4. **Subscriber on A / producer on B** — shared durable store, verify event completeness
5. **No broker correctness dependency** — disable local broker delivery, verify subscriber still receives all events from store
6. **Atomicity** — verify task+event commit is all-or-nothing (fail one → neither commits)

### 15. What Changes in Existing Code

**New trait: `A2aAtomicStore`** in `storage/atomic.rs`:
- Atomic task+event write operations
- Implemented by `InMemoryA2aStorage` (and future backends)

**`AppState`:**
- Add `atomic_store: Arc<dyn A2aAtomicStore>` alongside existing storage fields
- `event_store` retained for read operations (`get_events_after`, `latest_sequence`)

**Router (`core_send_streaming_message`, `core_subscribe_to_task`) — planned:**
- Streaming writes will use `atomic_store`, not `task_storage` directly
- Subscribe will read from `event_store`, with broker as wake-up signal
- Accept `Last-Event-ID` header for replay
- Terminal task subscribe replays from store then closes
- SSE events include `id: {task_id}:{sequence}` from store data

**`A2aServer::builder()`:**
- Same-backend enforcement now checks 4 traits (task, push, event, atomic)
- In practice, all 4 are on the same struct — one `backend_name()` check

**`NoStreamingLayer` (Lambda):**
- Removed once D3 is implemented — Lambda can stream via DynamoDB event store

**Existing tests:**
- All existing tests continue to pass (non-streaming paths unchanged)
- Terminal subscribe tests updated (replay instead of rejection)
- New D3 tests for atomicity, replay, cross-instance

## Consequences

- Cross-instance streaming becomes correct for any multi-instance deployment
- The in-process broker is preserved as a latency optimization, not removed or repurposed
- Atomic task+event writes are enforced by the type system — handlers cannot accidentally dual-write
- Event store is a new trait with its own parity tests — same discipline as task storage
- Lambda streaming becomes possible (event store = DynamoDB, no in-process coordination needed)
- `Last-Event-ID` replay provides resilient reconnection
- Terminal task replay eliminates a class of missed-event bugs
- The implementation can be phased: durable store first, optional bus/fanout later
- Backward compatible: existing single-instance deployments work unchanged with in-memory defaults
- Executor redesign (EventSink) is explicitly deferred — clean scope boundary
