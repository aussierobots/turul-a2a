# ADR-009: Durable Event Coordination for Multi-Instance Streaming

- **Status:** Proposed
- **Date:** 2026-04-11

## Context

Multi-instance request/response correctness is verified (distributed tests with shared storage). However, cross-instance streaming/subscription remains unsolved. The current in-process `TaskEventBroker` (`tokio::sync::broadcast`) is a local optimization for attached clients on the same instance — not a coordination mechanism.

This affects any multi-instance deployment: load-balanced binaries, ECS/Fargate, Kubernetes, Lambda, rolling deploys. The problem is the same regardless of deployment target.

**The gap:** If a subscriber connects to instance A and the task's executor runs on instance B, instance A's broker has no events to deliver. The subscriber sees nothing.

## Decision

### 1. Source of Truth: Durable Event Store

A new **durable event store** is the source of truth for streaming events. The in-process broker becomes a local delivery optimization — it accelerates delivery to clients attached to the same instance, but the event store is what guarantees correctness.

**Stored per event:**
- `task_id` — which task this event belongs to
- `event_sequence` — monotonically increasing integer per task (assigned by the store)
- `event_type` — `status_update` or `artifact_update`
- `event_data` — serialized `StreamEvent` JSON
- `created_at` — timestamp
- `ttl` — expiration (for cleanup)

**Key shape:**
- Primary key: `(task_id, event_sequence)`
- Events are append-only within a task
- The store assigns `event_sequence` atomically (no caller-assigned IDs)

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
1. Client sends `Last-Event-ID: {task_id}:{sequence}` header (or equivalent for initial subscribe)
2. Server queries event store: `get_events_after(task_id, sequence)`
3. Server streams historical events first, then switches to live delivery
4. If `sequence` is 0 or absent, all stored events for the task are replayed

**If the requested replay point has expired (TTL):**
- Server returns the earliest available events, not an error
- Server MAY include a synthetic `event: replay_gap` SSE comment indicating missed events
- Client should treat this as best-effort catch-up, not a protocol error

**Idempotency:** Clients SHOULD handle potential duplicate delivery during the transition from historical replay to live events. Events are identified by `(task_id, event_sequence)` — clients can deduplicate by tracking the last seen sequence per task.

### 4. Live Delivery vs Correctness

**Architecture layers:**

```
Producer (executor)
  → write event to durable store (correctness-critical)
  → publish to local in-process broker (optimization)
  → optionally notify cross-instance bus (optimization)

Subscriber (SSE client)
  → on connect: replay from durable store (correctness-critical)
  → switch to live: local broker if same instance (optimization)
  → or poll durable store periodically (correctness fallback)
  → or receive from cross-instance bus (optimization)
```

**Correctness-critical path:** durable store write + durable store replay. This alone is sufficient for correct streaming — everything else is latency optimization.

**Optional bus/fanout layer:** SNS, EventBridge, Redis Pub/Sub, or DynamoDB Streams can provide low-latency cross-instance notification ("wake up, new event for task X"). Without this, subscribers on other instances poll the durable store at a configurable interval (e.g., 1-5 seconds). Polling is correct but higher latency.

**Local broker role:** When producer and subscriber are on the same instance, the in-process `tokio::sync::broadcast` delivers events with zero additional latency. This is preserved as-is from the current architecture. It is NOT the source of truth — just a fast path.

### 5. Trait Boundary: `A2aEventStore`

A new trait, separate from `A2aTaskStorage`. Events are a different concern from task CRUD — they are append-only, sequence-ordered, and have different query patterns.

```rust
#[async_trait]
pub trait A2aEventStore: Send + Sync {
    fn backend_name(&self) -> &'static str;

    /// Append an event for a task. Returns the assigned sequence number.
    async fn append_event(
        &self,
        task_id: &str,
        event: StreamEvent,
    ) -> Result<u64, A2aStorageError>;

    /// Get all events for a task after a given sequence number.
    /// Returns events in sequence order.
    async fn get_events_after(
        &self,
        task_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, StreamEvent)>, A2aStorageError>;

    /// Get the latest event sequence number for a task.
    /// Returns 0 if no events exist.
    async fn latest_sequence(
        &self,
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
- PK: `taskId` (String)
- SK: `eventSequence` (Number) — assigned via atomic counter or conditional write
- Attributes: `eventType`, `eventData` (JSON string), `createdAt`, `ttl` (DynamoDB TTL)
- Query: `taskId = :tid AND eventSequence > :seq` with `ScanIndexForward = true`
- Atomic sequence: use `UpdateItem` with `ADD eventSequence :inc` on a counter item, or conditional `PutItem` with sequence check

**PostgreSQL:**
- Table: `a2a_task_events`
- Columns: `task_id TEXT, event_sequence BIGSERIAL, event_type TEXT, event_data JSONB, created_at TIMESTAMPTZ DEFAULT NOW()`
- Primary key: `(task_id, event_sequence)`
- `event_sequence` is auto-assigned by `BIGSERIAL` — monotonic per-database, not per-task. For per-task monotonic ordering, use `ROW_NUMBER() OVER (PARTITION BY task_id ORDER BY event_sequence)` or a separate sequence table.
- Query: `WHERE task_id = $1 AND event_sequence > $2 ORDER BY event_sequence`

**SQLite:**
- Same schema as PostgreSQL with `INTEGER PRIMARY KEY AUTOINCREMENT` for global sequence
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

**Event write failure:** If the durable store write fails, the event is lost. The task storage update (which happens first) may succeed, creating a state where the task has progressed but the event was not recorded. This is acceptable — the subscriber can detect the gap via sequence numbers and fall back to polling the task's current status.

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

### 10. What Changes in Existing Code

**Router (`core_send_streaming_message`, `core_subscribe_to_task`):**
- Write events to durable store instead of (or in addition to) local broker
- Accept `Last-Event-ID` header for replay
- SSE response reads from durable store for catch-up, then local broker for live

**`AppState`:**
- Add `event_store: Arc<dyn A2aEventStore>` field

**`A2aServer::builder()` / `LambdaA2aServerBuilder`:**
- Add `.event_store()` builder method
- Default: in-memory event store (backward compatible)

**`NoStreamingLayer` (Lambda):**
- Removed once D3 is implemented — Lambda can stream if event store is DynamoDB

**Existing tests:**
- All 334 existing tests continue to pass (event store defaults to in-memory)
- New D3 tests added for cross-instance and replay scenarios

## Consequences

- Cross-instance streaming becomes correct for any multi-instance deployment
- The in-process broker is preserved as a latency optimization, not removed
- Event store is a new trait with its own parity tests — same discipline as task storage
- Lambda streaming becomes possible (event store = DynamoDB, no in-process coordination needed)
- `Last-Event-ID` replay provides resilient reconnection
- The implementation can be phased: durable store first, optional bus/fanout later
- Backward compatible: existing single-instance deployments work unchanged with in-memory event store default
