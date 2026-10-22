# ADR-005: Dual Transport (HTTP+JSON and JSON-RPC) with Shared Core Handlers

- **Status:** Accepted
- **Date:** 2026-04-10

## Context

A2A v1.0 defines HTTP/REST and JSON-RPC protocol bindings. Both must be supported in v0.1. The HTTP routes come from proto `google.api.http` annotations (e.g., `POST /message:send`, `GET /tasks/{id=*}:cancel`). JSON-RPC uses PascalCase method names (SendMessage, GetTask, etc.). gRPC is now the third transport under the same shared-core invariant — see [ADR-014](ADR-014-grpc-transport.md); feature-gated behind `grpc`, default off.

## Decision

All business logic lives in `core_*` functions that take explicit `tenant: &str` parameters. HTTP handlers extract tenant from path (tenant-prefixed routes like `/{tenant}/message:send`) or use default. JSON-RPC handler extracts tenant from `params.tenant`. Both call the same core functions.

The router uses axum with wildcard catch-all (`/tasks/{*rest}`) for the proto's `{id=*}:action` patterns, with manual path parsing to disambiguate GetTask vs CancelTask vs Subscribe vs push config CRUD. JSON-RPC dispatch matches against `wire::jsonrpc` constants.

Notifications (no `id`) return 204 No Content. Non-object `params` return -32602.

SubscribeToTask is routed for both `GET /tasks/{id}:subscribe` (proto `google.api.http` annotation) and `POST /tasks/{id}:subscribe` (spec §11.3.2 text), on the default and tenant-prefixed routes.

Transport-layer rejections that happen before dispatch (`A2A-Version` not supported, request Content-Type not JSON — ADR-004) are rendered in the binding of the endpoint: on the JSON-RPC endpoint they are JSON-RPC 2.0 error envelopes with `id: null` (the request body has not been read) and the A2A code and ErrorInfo in `error.data`; elsewhere the HTTP+JSON error body. The JSON-RPC binding (spec §9) does not specify an HTTP status for error responses; they use HTTP 200, the same as every error the JSON-RPC dispatcher itself returns. Auth failures are the exception and stay HTTP 401/403 on every path (ADR-007, ADR-016).

## Consequences

- **No behavior fork between transports.** Both HTTP and JSON-RPC exercise the same core functions, so parity is structural rather than aspirational.
- **Adding a new operation requires three touch-points:** add core function, add HTTP route, add JSON-RPC dispatch case.
- **Axum wildcard routing adds one level of indirection for task paths** but handles the proto's colon-action syntax cleanly.

## Revision log

- **2026-10-22** — `POST /tasks/{id}:subscribe` accepted alongside `GET`. Transport-layer `A2A-Version` / Content-Type rejections on the JSON-RPC endpoint are JSON-RPC error envelopes (HTTP 200, `id: null`) instead of HTTP+JSON error bodies.
