# ADR-006: SSE Streaming Architecture and last_chunk Semantics

- **Status:** Accepted
- **Date:** 2026-04-10

## Context

A2A defines streaming via `POST /message:stream` (send + stream) and `GET /tasks/{id=*}:subscribe` (subscribe to existing task). Events are `TaskStatusUpdateEvent` and `TaskArtifactUpdateEvent` wrapped in `StreamResponse`. `TaskArtifactUpdateEvent` has `append` and `last_chunk` boolean fields for incremental artifact delivery. The question: should `last_chunk` be persisted as completion state, or treated as transport metadata?

## Decision

In-process `TaskEventBroker` using `tokio::sync::broadcast` for multi-subscriber fan-out per task.

- `POST /message:stream` subscribes before task creation, spawns executor in background, returns SSE stream.
- `GET /tasks/{id}:subscribe` verifies task exists and is non-terminal, subscribes, publishes current status as first event.
- `last_chunk` is transport-level metadata only in v0.1 -- forwarded to streaming subscribers but not persisted in storage.
- The `append` flag IS implemented in storage: `append=true` with matching `artifact_id` appends parts to existing artifact.

**Single-instance limitation:** The broker is in-process only. Multi-instance deployments (Lambda, horizontal scale) require a shared event coordination mechanism (Redis, DynamoDB, EventBridge) -- documented as future work in CLAUDE.md.

## Consequences

- **Streaming works for single-instance deployments.** The in-process broker is sufficient for v0.1 targets.
- **`last_chunk` can be upgraded to persisted state** in a future version without breaking the storage trait (the parameter already exists).
- **Broker cleanup on terminal state prevents memory leaks.** Subscriptions are dropped when a task reaches a terminal status.
- **Multi-subscriber fan-out is proven by parity tests.** Both HTTP SSE and JSON-RPC streaming exercise the same broker path.
