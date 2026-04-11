# ADR-008: Lambda Adapter and Multi-Instance Streaming Coordination

- **Status:** Accepted
- **Date:** 2026-04-11

## Context

turul-a2a needs an AWS Lambda deployment path. The current SSE streaming architecture (ADR-006) uses an in-process `TaskEventBroker` (`tokio::sync::broadcast`) that is **single-instance only**. This limitation is not Lambda-specific — it affects any multi-instance deployment:

- Lambda (each invocation is a separate process)
- 2+ binaries behind a load balancer
- ECS/Fargate with multiple tasks
- Kubernetes with multiple pods
- Rolling deploys with concurrent instances

The core issue: the in-process broker provides local live fanout to currently attached clients on the same instance. It does not provide cross-instance event delivery or replay. Shared task storage (DynamoDB, PostgreSQL) solves request/response correctness across instances, but does not solve streaming coordination.

## Decision

### D2 Scope: Request/Response Subset

D2 delivers the Lambda adapter for the **request/response-compatible subset** of A2A operations. Streaming endpoints are explicitly unsupported in any multi-instance deployment until D3 provides durable streaming coordination.

**Supported (D2):**
- `POST /message:send` — full support (synchronous execution, returns `SendMessageResponse`)
- `GET /tasks/{id}` — full support
- `POST /tasks/{id}:cancel` — full support
- `GET /tasks` — full support
- `POST/GET/DELETE` push notification configs — full support
- `GET /.well-known/agent-card.json` — full support
- `GET /extendedAgentCard` — full support
- `POST /jsonrpc` — all request/response methods

**Not supported (D2):**
- `POST /message:stream` → returns `UnsupportedOperationError` (HTTP 400, JSON-RPC -32004, ErrorInfo `UNSUPPORTED_OPERATION`)
- `GET /tasks/{id}:subscribe` → returns `UnsupportedOperationError`
- JSON-RPC `SendStreamingMessage` / `SubscribeToTask` → returns `UnsupportedOperationError`

Streaming rejection uses the existing A2A error contract — same wire format as any other unsupported operation. No deployment-specific HTTP status codes.

### Adapter Architecture: Thin Wrapper

The Lambda adapter wraps the existing `axum::Router` without modifying `turul-a2a` core. Zero changes to the server crate.

```
Lambda Event (API Gateway / Function URL)
  → lambda_to_axum_request() (strip spoofed headers, inject authorizer context)
  → NoStreamingLayer (rejects streaming paths with UnsupportedOperationError)
  → TransportComplianceLayer (A2A-Version, Content-Type)
  → AuthLayer (reads headers or LambdaAuthorizerMiddleware)
  → axum Router (same handlers as local server)
  → axum_to_lambda_response()
  → Lambda Response
```

### Auth: API Gateway Authorizer Mapping

API Gateway authorizers produce a context (e.g., `{ "userId": "user-123", "tenantId": "acme" }`). The Lambda adapter injects this as synthetic `x-authorizer-*` headers before the request reaches the Tower auth stack.

**Trust boundary rule:** The adapter MUST strip any client-supplied `x-authorizer-*` headers from the incoming request before injecting authorizer context. Only data from Lambda's `requestContext.authorizer` is trusted to populate these headers. This prevents spoofing where an external caller sends fake headers to bypass authentication.

`LambdaAuthorizerMiddleware` (implements `A2aMiddleware`) reads these adapter-injected headers and constructs `AuthIdentity::Authenticated`. This does NOT bypass the auth stack — it IS a registered middleware, same as `BearerMiddleware` or `ApiKeyMiddleware`.

### Storage: DynamoDB (D1)

Lambda uses the existing `DynamoDbA2aStorage` from D1. No new tables or traits needed for request/response operations.

### D3: Durable Streaming Coordination (Future — Not Lambda-Specific)

Cross-instance streaming requires durable event coordination that works for **any** multi-instance deployment, not just Lambda. The architecture needs:

1. **Durable event store** — source of truth for task events with monotonic IDs. DynamoDB table (`a2a_task_events`, PK=taskId, SK=eventSequence) or equivalent.
2. **Replay semantics** — `Last-Event-ID` header parsing, `get_events_after(event_id)` query for reconnection catch-up.
3. **Cross-instance notification** — optional SNS/EventBridge/Redis for wake/fanout. Polling the durable store is acceptable but higher latency.
4. **New trait** — `A2aEventStore` or extension of existing storage traits.

The in-process `TaskEventBroker` remains as a **local optimization** for attached clients on the same instance. The durable event store is the source of truth; local broadcast is a cache for live connections.

D3 gets its own ADR when prioritized.

### What Is Only Optimization, Not Correctness-Critical

- Cold-start caching / `OnceCell` for DynamoDB client
- Local connection manager reuse
- Instance-local broadcast (the in-process broker is inert on Lambda but harmless)

## Verification Scope

**D2 proves:** The Lambda adapter correctly wraps the same router/middleware stack for request/response operations in a single-instance Lambda runtime. Verified locally with cargo-lambda.

**D2 does NOT prove:** Multi-instance behavior under shared storage. Cross-instance request/response (create on instance A, fetch on instance B) requires a separate distributed verification pass against a real shared backend (DynamoDB). This is a future verification task, not part of D2.

**D3 proves (future):** Cross-instance streaming/subscription coordination. Subscriber on instance A + producer on instance B is the architecture gap D3 solves.

## Consequences

- Lambda adapter works for the request/response A2A surface (11 of 13 proto RPC methods) in single-instance execution
- Streaming is honestly scoped as "not supported in multi-instance deployments" rather than silently broken
- The adapter requires zero changes to `turul-a2a` core
- Auth mapping from API Gateway authorizers uses the same middleware stack
- Distributed multi-instance verification (two instances, shared backend, alternating requests) is a separate future task
- Full streaming parity (D3) applies to all multi-instance deployments, not just Lambda
