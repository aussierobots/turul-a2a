# ADR-005: Dual Transport (HTTP+JSON and JSON-RPC) with Shared Core Handlers

- **Status:** Accepted
- **Date:** 2026-04-10

## Context

A2A v1.0 defines HTTP/REST and JSON-RPC protocol bindings. Both must be supported in v0.1. The HTTP routes come from proto `google.api.http` annotations (e.g., `POST /message:send`, `GET /tasks/{id=*}:cancel`). JSON-RPC uses PascalCase method names (SendMessage, GetTask, etc.). gRPC is deferred.

## Decision

All business logic lives in `core_*` functions that take explicit `tenant: &str` parameters. HTTP handlers extract tenant from path (tenant-prefixed routes like `/{tenant}/message:send`) or use default. JSON-RPC handler extracts tenant from `params.tenant`. Both call the same core functions.

The router uses axum with wildcard catch-all (`/tasks/{*rest}`) for the proto's `{id=*}:action` patterns, with manual path parsing to disambiguate GetTask vs CancelTask vs Subscribe vs push config CRUD. JSON-RPC dispatch matches against `wire::jsonrpc` constants.

Notifications (no `id`) return 204 No Content. Non-object `params` return -32602.

## Consequences

- **No behavior fork between transports.** Both HTTP and JSON-RPC exercise the same core functions, so parity is structural rather than aspirational.
- **Adding a new operation requires three touch-points:** add core function, add HTTP route, add JSON-RPC dispatch case.
- **Axum wildcard routing adds one level of indirection for task paths** but handles the proto's colon-action syntax cleanly.
