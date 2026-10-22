# ADR-004: Error Model with A2A Error Codes and google.rpc.ErrorInfo

- **Status:** Accepted
- **Date:** 2026-04-10

## Context

The A2A spec (v1.0.1) defines 9 specific error types (-32001 to -32009 for JSON-RPC; HTTP status 404 for TaskNotFound, 500 for InvalidAgentResponse, 400 for the rest) and requires `google.rpc.ErrorInfo` in error responses across all bindings. ErrorInfo must include `reason` (UPPER_SNAKE_CASE, no "Error" suffix), `domain` ("a2a-protocol.org"), and optional `metadata`.

## Decision

`A2aError` enum has one variant per A2A error type plus generic variants (InvalidRequest, Internal). Each variant maps to an exact HTTP status, JSON-RPC code, and ErrorInfo reason via wire constants from `turul_a2a_types::wire::errors`.

- `to_http_error_body()` produces AIP-193 format.
- `to_jsonrpc_error()` produces JSON-RPC 2.0 format with ErrorInfo as the single `@type`-tagged entry of the `error.data` array. The `turul-rpc` dispatcher path (`ToJsonRpcError`) emits the same array shape.
- Because seven of the nine A2A errors share HTTP 400 (and most share gRPC `FAILED_PRECONDITION`), `ErrorInfo.reason` — not the status code — is the discriminator clients must match on.
- Non-A2A errors (InvalidRequest, Internal) have no ErrorInfo.
- `A2aStorageError` converts to `A2aError` via `From` impl.
- **Transport auth (401/403) is NOT modeled in `A2aError`.** Authentication and authorization failures are handled by `MiddlewareError` in the Tower auth layer and produce HTTP responses directly, before any handler or JSON-RPC dispatch. See ADR-007.

Key mappings:

| Variant | HTTP Status | JSON-RPC Code | ErrorInfo Reason |
|---------|-------------|---------------|------------------|
| TaskNotFound | 404 | -32001 | TASK_NOT_FOUND |
| TaskNotCancelable | 400 | -32002 | TASK_NOT_CANCELABLE |
| PushNotificationNotSupported | 400 | -32003 | PUSH_NOTIFICATION_NOT_SUPPORTED |
| UnsupportedOperation | 400 | -32004 | UNSUPPORTED_OPERATION |
| ContentTypeNotSupported | 400 | -32005 | CONTENT_TYPE_NOT_SUPPORTED |
| InvalidAgentResponse | 500 | -32006 | INVALID_AGENT_RESPONSE |
| ExtendedAgentCardNotConfigured | 400 | -32007 | EXTENDED_AGENT_CARD_NOT_CONFIGURED |
| ExtensionSupportRequired | 400 | -32008 | EXTENSION_SUPPORT_REQUIRED |
| VersionNotSupported | 400 | -32009 | VERSION_NOT_SUPPORTED |

## Consequences

- **Error mapping is centralized and tested.** All error-to-wire conversions go through a single code path per binding (HTTP, JSON-RPC).
- **Wire constants are the single source of truth.** `turul_a2a_types::wire::errors` defines codes and reasons; no magic numbers in handler code.
- **Adding new A2A error types is predictable.** Add a variant, add a wire constant, add the mapping -- all in known locations.
- **Non-A2A errors omit ErrorInfo intentionally.** Generic errors like InvalidRequest and Internal are framework-level, not protocol-level, and do not carry `domain`/`reason` metadata.
- **Storage errors convert cleanly.** The `From<A2aStorageError>` impl ensures storage-layer failures surface as the correct A2A error variant without manual mapping at each call site.

## Revision log

- 2026-04-10 — Initial acceptance against A2A v1.0.0.
- 2026-10-22 — Aligned to A2A v1.0.1 §5.4: TaskNotCancelable 409→400, ContentTypeNotSupported 415→400, InvalidAgentResponse 502→500. Full nine-row table. JSON-RPC `error.data` is now an array on both producers (the code had drifted to a bare object; this ADR always said array). Owner unchanged: `turul_a2a_types::wire::errors` constants, consumed by `A2aError::{http_status, to_http_error_body, to_jsonrpc_error}` and the `ToJsonRpcError` impl.
