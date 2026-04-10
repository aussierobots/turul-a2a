# ADR-004: Error Model with A2A Error Codes and google.rpc.ErrorInfo

- **Status:** Accepted
- **Date:** 2026-04-10

## Context

The A2A spec defines 9 specific error types (-32001 to -32009 for JSON-RPC, HTTP status codes 400/404/409/415/502) and requires `google.rpc.ErrorInfo` in error responses across all bindings. ErrorInfo must include `reason` (UPPER_SNAKE_CASE, no "Error" suffix), `domain` ("a2a-protocol.org"), and optional `metadata`.

## Decision

`A2aError` enum has one variant per A2A error type plus generic variants (InvalidRequest, Unauthenticated, Internal). Each variant maps to an exact HTTP status, JSON-RPC code, and ErrorInfo reason via wire constants from `turul_a2a_types::wire::errors`.

- `to_http_error_body()` produces AIP-193 format.
- `to_jsonrpc_error()` produces JSON-RPC 2.0 format with ErrorInfo in `error.data` array.
- Non-A2A errors (InvalidRequest, Internal) have no ErrorInfo.
- `A2aStorageError` converts to `A2aError` via `From` impl.

Key mappings:

| Variant | HTTP Status | JSON-RPC Code | ErrorInfo Reason |
|---------|-------------|---------------|------------------|
| TaskNotFound | 404 | -32001 | TASK_NOT_FOUND |
| TaskNotCancelable | 409 | -32002 | TASK_NOT_CANCELABLE |
| ContentTypeNotSupported | 415 | -32005 | CONTENT_TYPE_NOT_SUPPORTED |
| InvalidAgentResponse | 502 | -32006 | INVALID_AGENT_RESPONSE |

## Consequences

- **Error mapping is centralized and tested.** All error-to-wire conversions go through a single code path per binding (HTTP, JSON-RPC).
- **Wire constants are the single source of truth.** `turul_a2a_types::wire::errors` defines codes and reasons; no magic numbers in handler code.
- **Adding new A2A error types is predictable.** Add a variant, add a wire constant, add the mapping -- all in known locations.
- **Non-A2A errors omit ErrorInfo intentionally.** Generic errors like InvalidRequest and Internal are framework-level, not protocol-level, and do not carry `domain`/`reason` metadata.
- **Storage errors convert cleanly.** The `From<A2aStorageError>` impl ensures storage-layer failures surface as the correct A2A error variant without manual mapping at each call site.
