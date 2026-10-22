# ADR-004: Error Model with A2A Error Codes and google.rpc.ErrorInfo

- **Status:** Accepted
- **Date:** 2026-04-10

## Context

The A2A spec (v1.0.1) defines 9 specific error types (-32001 to -32009 for JSON-RPC; HTTP status 404 for TaskNotFound, 500 for InvalidAgentResponse, 400 for the rest) and requires `google.rpc.ErrorInfo` in error responses across all bindings. ErrorInfo must include `reason` (UPPER_SNAKE_CASE, no "Error" suffix), `domain` ("a2a-protocol.org"), and optional `metadata`.

## Decision

`A2aError` enum has one variant per A2A error type plus generic variants (InvalidRequest, Internal). Each variant maps to an exact HTTP status, JSON-RPC code, gRPC status and ErrorInfo reason via wire constants from `turul_a2a_types::wire::errors`. The owning symbols are `A2aError::{http_status, jsonrpc_code, grpc_status, error_reason}`; every binding renders from them:

- `to_http_error_body()` produces the AIP-193 `google.rpc.Status` JSON shape of the spec §11.6 example: `{"error": {"code", "status", "message", "details"}}`, where `status` is the `google.rpc.Code` name from `grpc_status()`.
- `grpc::error::a2a_to_status` only translates the `grpc_status()` name into a `tonic::Code`; it holds no mapping of its own.
- `to_jsonrpc_error()` produces JSON-RPC 2.0 format with ErrorInfo as the single `@type`-tagged entry of the `error.data` array. The `turul-rpc` dispatcher path (`ToJsonRpcError`) emits the same array shape.
- The ErrorInfo JSON object itself is owned by `turul_a2a_types::ErrorInfo` (`@type`, `reason`, `domain`, string-to-string `metadata`, omitted when empty). `A2aError::error_info` builds through it (as do the auth-failure details, see ADR-016), and `turul-a2a-client` takes the reason from the first detail that deserializes as an `ErrorInfo`. Adopters that put an ErrorInfo in a task's data part use the same type.
- Because seven of the nine A2A errors share HTTP 400 (and most share gRPC `FAILED_PRECONDITION`), `ErrorInfo.reason` — not the status code — is the discriminator clients must match on.
- Non-A2A errors (InvalidRequest, Internal) have no ErrorInfo.
- `A2aStorageError` converts to `A2aError` via `From` impl. `TerminalState`, `TerminalStateAlreadySet` and `InvalidTransition` carry the task id, so the `TaskNotCancelable` they become names the task.
- **Transport-layer rejections.** `middleware::transport` rejects before dispatch: an unsupported `A2A-Version` (header, `?A2A-Version=` query parameter, or gRPC `a2a-version` metadata; `Major.Minor` compared, patch ignored; absent or empty means 0.3 per spec §3.6.2) is `VersionNotSupported`, validated by `check_version` for both the HTTP layer and the gRPC layer. 0.3 — absent, empty or explicit — is accepted only on HTTP with the `compat-v03` feature; gRPC never serves 0.3 and always rejects it. A POST whose Content-Type is not `application/json` or `application/a2a+json` (parameters such as `charset` allowed) is `ContentTypeNotSupported`. On the JSON-RPC endpoint these are JSON-RPC error envelopes (see ADR-005); elsewhere the HTTP error body above.
- **Response media type follows `Accept`.** A2A v1.0.1 §11.1 says `application/a2a+json` SHOULD be used on the HTTP+JSON binding; §9.1 makes the JSON-RPC binding `application/json` for requests and responses, so JSON-RPC responses are never relabelled. On HTTP+JSON the transport-compliance layer labels a JSON response (success or error) `application/a2a+json` only when the client accepts it: `Accept` names it, or `Accept` is absent, empty, `*/*` or `application/*` and the request body was itself `application/a2a+json`. A range naming `application/a2a+json` with `q=0` refuses it, whatever the wildcards say. Every other response is `application/json`, so a client that listed only `application/json` (the official Go SDK sends `text/event-stream, application/json` on streaming calls and rejects any other JSON type) is never sent a type it did not accept. v0.3 requests under `compat-v03` and the version-independent well-known agent card are always `application/json`. Owner: `middleware::transport::accepts_a2a_json`.
- **Request media type → `ContentTypeNotSupported` (spec silent, kept).** The spec defines `ContentTypeNotSupportedError` for media types "provided in the request's message parts or implied for an artifact" (§3.3.2) and does not name an error for an unsupported request-body media type; §3.3.2 lists "unsupported content type" only as an example scenario of a validation error (HTTP 400). The framework keeps `ContentTypeNotSupported`: it is HTTP 400 like a validation error, and its `CONTENT_TYPE_NOT_SUPPORTED` reason tells the client exactly what to change, where a generic `InvalidRequest` carries no ErrorInfo at all.
- **Transport auth (401/403) is NOT modeled in `A2aError`.** Authentication and authorization failures are handled by `MiddlewareError` in the Tower auth layer and produce HTTP responses directly, before any handler or JSON-RPC dispatch. See ADR-007.

Key mappings:

| Variant | HTTP Status | JSON-RPC Code | gRPC Status | ErrorInfo Reason |
|---------|-------------|---------------|-------------|------------------|
| TaskNotFound | 404 | -32001 | NOT_FOUND | TASK_NOT_FOUND |
| TaskNotCancelable | 400 | -32002 | FAILED_PRECONDITION | TASK_NOT_CANCELABLE |
| PushNotificationNotSupported | 400 | -32003 | FAILED_PRECONDITION | PUSH_NOTIFICATION_NOT_SUPPORTED |
| UnsupportedOperation | 400 | -32004 | FAILED_PRECONDITION | UNSUPPORTED_OPERATION |
| ContentTypeNotSupported | 400 | -32005 | INVALID_ARGUMENT | CONTENT_TYPE_NOT_SUPPORTED |
| InvalidAgentResponse | 500 | -32006 | INTERNAL | INVALID_AGENT_RESPONSE |
| ExtendedAgentCardNotConfigured | 400 | -32007 | FAILED_PRECONDITION | EXTENDED_AGENT_CARD_NOT_CONFIGURED |
| ExtensionSupportRequired | 400 | -32008 | FAILED_PRECONDITION | EXTENSION_SUPPORT_REQUIRED |
| VersionNotSupported | 400 | -32009 | FAILED_PRECONDITION | VERSION_NOT_SUPPORTED |
| InvalidRequest (non-A2A) | 400 | -32602 | INVALID_ARGUMENT | — |
| Internal (non-A2A) | 500 | -32603 | INTERNAL | — |

The executable contract is the table test `error::tests::every_a2a_error_maps_exactly_per_spec_table`, which states every row with literal spec values and, under the `grpc` feature, checks the `tonic::Code` and ErrorInfo of `a2a_to_status`.

## Consequences

- **Error mapping is centralized and tested.** All error-to-wire conversions go through a single code path per binding (HTTP, JSON-RPC, gRPC).
- **Wire constants are the single source of truth.** `turul_a2a_types::wire::errors` defines codes and reasons; no magic numbers in handler code.
- **Adding new A2A error types is predictable.** Add a variant, add a wire constant, add the mapping -- all in known locations.
- **Non-A2A errors omit ErrorInfo intentionally.** Generic errors like InvalidRequest and Internal are framework-level, not protocol-level, and do not carry `domain`/`reason` metadata.
- **Storage errors convert cleanly.** The `From<A2aStorageError>` impl ensures storage-layer failures surface as the correct A2A error variant without manual mapping at each call site.

## Revision log

- 2026-04-10 — Initial acceptance against A2A v1.0.0.
- 2026-10-22 — Aligned to A2A v1.0.1 §5.4: TaskNotCancelable 409→400, ContentTypeNotSupported 415→400, InvalidAgentResponse 502→500. Full nine-row table. JSON-RPC `error.data` is now an array on both producers (the code had drifted to a bare object; this ADR always said array). Owner unchanged: `turul_a2a_types::wire::errors` constants, consumed by `A2aError::{http_status, to_http_error_body, to_jsonrpc_error}` and the `ToJsonRpcError` impl.
- 2026-10-22 — gRPC column added and owned by `A2aError::grpc_status` (`wire::errors::GRPC_*`); `a2a_to_status` translates it instead of keeping a parallel mapping. HTTP error bodies gain the AIP-193 `status` field (spec §11.6 example). Storage terminal/transition errors carry the task id. Transport-layer rejections documented: `A2A-Version` via header, query parameter or gRPC metadata with `Major.Minor` matching; `application/a2a+json` accepted; the request-media-type → `ContentTypeNotSupported` choice recorded with its rationale (spec silent). Version 0.3 (absent, empty or explicit) is accepted only on HTTP under `compat-v03`; gRPC rejects it whatever the features. Literal-value table test replaces constant-to-constant tests.
- 2026-10-22 — Media types: JSON responses (success and error) are labelled `application/a2a+json` (A2A v1.0.1 §11.1 SHOULD), by the transport-compliance layer; v0.3 requests under `compat-v03` keep `application/json`, as does the version-independent well-known agent card. Requests may use either.
- 2026-10-22 — `ContentTypeNotSupported` is raised for a message part whose declared `mediaType` is not in `AgentCard.defaultInputModes` (`*/*` and `type/*` are wildcards; untyped parts and agents with no declared input modes are unrestricted), before any write, by `router::check_part_media_types` on every send path. It is also raised for an unsupported request Content-Type.
- 2026-10-23 — ErrorInfo JSON gets one owner, the public `turul_a2a_types::ErrorInfo`; A2A error details and the client's reason extraction route through it. Wire bytes unchanged (byte-exact fixtures on HTTP and JSON-RPC bodies). The client no longer takes the reason from whatever detail comes first: only a detail typed as ErrorInfo counts.
- 2026-10-23 — Response media type is negotiated from `Accept` instead of always `application/a2a+json`: the unconditional label broke the official Go SDK (a2a-go v2.7.0), which rejects a JSON error on a streaming call unless it is `application/json`. JSON-RPC responses are always `application/json` (§9.1; 0.2.0 relabelled them too). Rule moved from this log into the Decision.
