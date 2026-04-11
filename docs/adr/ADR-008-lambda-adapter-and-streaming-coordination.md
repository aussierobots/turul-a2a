# ADR-008: Lambda Adapter and Streaming Coordination

- **Status:** Proposed
- **Date:** 2026-04-11

## Context

turul-a2a needs an AWS Lambda deployment path. The current SSE streaming architecture (ADR-006) uses an in-process `TaskEventBroker` (`tokio::sync::broadcast`) that is single-instance only. Lambda invocations are stateless — each gets a fresh process with no broker state. This forces a decision about streaming scope before implementing the Lambda adapter.

The turul-mcp-framework's Lambda adapter (`turul-mcp-aws-lambda`) provides a reference pattern: convert Lambda events to axum/hyper requests, delegate to the same handler infrastructure, and convert responses back.

## Decision

### D2 Scope: Request/Response Only

D2 delivers Lambda support for all **request/response** operations. Streaming endpoints are explicitly not supported on Lambda in D2.

**Supported on Lambda (D2):**
- `POST /message:send` — full support (synchronous execution, returns `SendMessageResponse`)
- `GET /tasks/{id}` — full support
- `POST /tasks/{id}:cancel` — full support
- `GET /tasks` — full support
- `POST/GET/DELETE` push notification configs — full support
- `GET /.well-known/agent-card.json` — full support
- `GET /extendedAgentCard` — full support
- `POST /jsonrpc` — all request/response methods

**Not supported on Lambda (D2):**
- `POST /message:stream` → returns `501 Not Implemented` with explanation
- `GET /tasks/{id}:subscribe` → returns `501 Not Implemented`
- JSON-RPC `SendStreamingMessage` / `SubscribeToTask` → returns unsupported operation error

**Why streaming is deferred:** The in-process broker cannot coordinate across Lambda invocations. Making streaming work requires a durable event store (new DynamoDB table, new trait surface, replay semantics) — this is a design-level change, not an adapter-level change. It gets its own ADR.

### Adapter Architecture: Thin Wrapper

The Lambda adapter wraps the existing `axum::Router` (which implements `tower::Service`) without modifying `turul-a2a` core. Zero changes to the server crate.

```
Lambda Event (API Gateway / Function URL)
  → classify_runtime_event()
  → lambda_to_axum_request() (extract headers, body, authorizer context)
  → NoStreamingLayer (rejects /message:stream and :subscribe with 501)
  → TransportComplianceLayer (A2A-Version, Content-Type)
  → AuthLayer (reads headers or LambdaAuthorizerMiddleware)
  → axum Router (same handlers as local server)
  → axum_to_lambda_response()
  → Lambda Response
```

### Auth: API Gateway Authorizer Mapping

API Gateway authorizers produce a context (e.g., `{ "userId": "user-123", "tenantId": "acme" }`). The Lambda adapter injects this as synthetic `x-authorizer-*` headers before the request reaches the Tower auth stack.

`LambdaAuthorizerMiddleware` (implements `A2aMiddleware`) reads these headers and constructs `AuthIdentity::Authenticated`. This does NOT bypass the auth stack — it IS a registered middleware, same as `BearerMiddleware` or `ApiKeyMiddleware`.

If no authorizer is configured, standard header-based auth works as-is (Bearer token, API key headers pass through from API Gateway).

### Storage: DynamoDB (D1)

Lambda uses the existing `DynamoDbA2aStorage` from D1. No new tables or traits needed for request/response operations.

### Streaming Coordination: Deferred to D3

Full Lambda streaming parity requires:
1. **Durable event store** — new DynamoDB table (`a2a_task_events`, PK=taskId, SK=eventSequence) with monotonic event IDs
2. **Replay semantics** — `Last-Event-ID` header parsing, `get_events_after(event_id)` query
3. **Cross-instance notification** — optional SNS/EventBridge for wake/fanout (polling is acceptable alternative)
4. **New trait** — `A2aEventStore` or extension of existing storage traits

This is D3 scope and will get its own ADR when prioritized.

### What Is Only Optimization, Not Correctness-Critical

- Cold-start caching / `OnceCell` for DynamoDB client
- Local connection manager reuse
- Instance-local broadcast (the in-process broker is inert on Lambda but harmless)

## Consequences

- Lambda deployment works for the full request/response A2A surface (11 of 13 proto RPC methods — all except `SendStreamingMessage` and `SubscribeToTask`)
- Streaming on Lambda is honestly scoped as "not supported" rather than silently broken
- The adapter requires zero changes to `turul-a2a` core — it wraps the existing Router
- Auth mapping from API Gateway authorizers uses the same middleware stack as local deployment
- Full streaming parity is a separate, well-defined future work item with known requirements
