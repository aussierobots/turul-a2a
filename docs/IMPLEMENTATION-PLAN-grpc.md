# Implementation Plan · gRPC Transport (0.1.7)

**Status:** Draft — implementation MUST NOT begin until ADR-014 status changes from Proposed to Accepted.
**Date:** 2026-04-21
**Covers ADR:** [ADR-014](adr/ADR-014-grpc-transport.md)
**Release target:** 0.1.7 (additive, feature-gated, no breaking changes to existing transports)

## Intent

Wire gRPC as a third thin transport adapter over the shared core handlers established by ADR-005. All 11 RPCs defined in `proto/a2a.proto:19-140` (`service A2AService`) dispatch to the same `core_*` functions in `crates/turul-a2a/src/router.rs` that HTTP and JSON-RPC already call. Two server-streaming RPCs replicate the replay-then-live pattern from the SSE path but emit `pb::StreamResponse` messages. The entire surface is behind a `grpc` Cargo feature; default HTTP-only builds are unaffected.

---

## 1. Scope and Non-Goals

#### In scope

- All 11 RPCs: `SendMessage`, `SendStreamingMessage`, `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask`, `CreateTaskPushNotificationConfig`, `GetTaskPushNotificationConfig`, `ListTaskPushNotificationConfigs`, `DeleteTaskPushNotificationConfig`, `GetExtendedAgentCard`.
- Server-streaming for `SendStreamingMessage` and `SubscribeToTask` via the durable event store (ADR-009). `a2a-last-event-id` ASCII metadata for resume, matching the SSE `Last-Event-ID` convention.
- Auth via existing `A2aMiddleware` Tower stack (`crates/turul-a2a/src/middleware/layer.rs`) applied to the tonic server. No parallel auth pipeline.
- `A2aError` → `tonic::Status` mapping with mandatory `google.rpc.ErrorInfo` (domain `"a2a-protocol.org"`, per `turul_a2a_types::wire::errors::ERROR_DOMAIN`). Normative table in ADR-014 §2.5; implementation in `crates/turul-a2a/src/grpc/error.rs`.
- Spec-compliance suite extension — three-transport matrix under `--features grpc`.
- `grpc-reflection` and `grpc-health` as optional sub-features of `grpc`.
- `grpc` feature on `turul-a2a-client` providing `A2aGrpcClient`.
- CHANGELOG, README, CLAUDE.md, ADR cross-reference updates.

#### Out of scope

- **gRPC on `turul-a2a-aws-lambda`**: Lambda lacks persistent HTTP/2 connections — deployment-environment constraint, not a protocol limitation. The `grpc` feature MUST NOT be listed or transitively enabled in `turul-a2a-aws-lambda/Cargo.toml`. Lambda adopters MUST NOT declare a `GRPC` interface on their agent card.
- gRPC-Web (`tonic-web`), bidirectional streaming (proto defines none), xDS.
- TLS termination (deployer-owned; deployers front with Envoy, ALB, service mesh sidecar, etc.).
- Skill-level `security_requirements`.

---

## 2. Crate and Feature Changes

#### Root `Cargo.toml` — additions to `[workspace.dependencies]`

```toml
# Runtime
tonic            = "0.14"
tonic-prost      = "0.14"        # Prost codec implementation for tonic
tonic-types      = "0.14"        # StatusExt + ErrorDetails for google.rpc.ErrorInfo
tonic-reflection = "0.14"        # grpc-reflection sub-feature
tonic-health     = "0.14"        # grpc-health sub-feature

# Build-time (for turul-a2a-proto build.rs)
tonic-prost-build = "0.14"       # wraps prost-build; generates service stubs
tonic-build       = "0.14"       # code-gen support used by tonic-prost-build
```

All versions align with prost 0.14 (tonic 0.14 series requires prost 0.14). Workspace MSRV 1.85 exceeds tonic's MSRV 1.75 — no MSRV change.

#### `crates/turul-a2a-proto/Cargo.toml`

```toml
[features]
grpc = ["dep:tonic", "dep:tonic-prost"]

[dependencies]
tonic       = { workspace = true, optional = true }
tonic-prost = { workspace = true, optional = true }

[build-dependencies]
tonic-prost-build = { workspace = true, optional = true }
tonic-build       = { workspace = true, optional = true }
```

#### `crates/turul-a2a/Cargo.toml`

```toml
[features]
grpc             = ["dep:tonic", "dep:tonic-prost", "dep:tonic-types",
                    "turul-a2a-proto/grpc"]
grpc-reflection  = ["grpc", "dep:tonic-reflection"]
grpc-health      = ["grpc", "dep:tonic-health"]

[dependencies]
tonic            = { workspace = true, optional = true }
tonic-prost      = { workspace = true, optional = true }
tonic-types      = { workspace = true, optional = true }
tonic-reflection = { workspace = true, optional = true }
tonic-health     = { workspace = true, optional = true }
```

#### `crates/turul-a2a-client/Cargo.toml`

```toml
[features]
grpc = ["dep:tonic", "dep:tonic-prost", "turul-a2a-proto/grpc"]

[dependencies]
tonic       = { workspace = true, optional = true }
tonic-prost = { workspace = true, optional = true }
```

#### `crates/turul-a2a-aws-lambda/Cargo.toml`

No change. Add comment to `[features]` table:
```toml
# grpc is intentionally absent — Lambda lacks persistent HTTP/2 (ADR-014 §2.6).
```

---

## 3. Proto Crate Changes

#### `crates/turul-a2a-proto/build.rs`

Current path: `prost_build::Config::new()` → descriptor file → `pbjson_build::Builder`. New dual-mode structure:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_file = "proto/a2a.proto";
    let proto_includes = &["proto"];
    println!("cargo:rerun-if-changed=proto");
    let descriptor_path = PathBuf::from(env::var("OUT_DIR")?).join("a2a_descriptor.bin");

    #[cfg(feature = "grpc")]
    {
        // tonic-prost-build generates message types + service stubs.
        // .file_descriptor_set_path() feeds the descriptor to pbjson-build below.
        tonic_prost_build::configure()
            .compile_well_known_types()
            .extern_path(".google.protobuf", "::pbjson_types")
            .file_descriptor_set_path(&descriptor_path)
            .build_server(true)
            .build_client(true)
            .compile_protos(&[proto_file], proto_includes)?;
    }
    #[cfg(not(feature = "grpc"))]
    {
        prost_build::Config::new()
            .compile_well_known_types()
            .extern_path(".google.protobuf", "::pbjson_types")
            .file_descriptor_set_path(&descriptor_path)
            .compile_protos(&[proto_file], proto_includes)?;
    }

    // pbjson-build runs unconditionally from the same descriptor.
    let descriptor_bytes = std::fs::read(&descriptor_path)?;
    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_bytes)?
        .ignore_unknown_fields()
        .build(&[".lf.a2a.v1"])?;
    Ok(())
}
```

Build-dep gating: use `[build-dependencies]` with `optional = true` for `tonic-prost-build` and `tonic-build`; add `build = "build.rs"` with feature-conditional compilation via `cfg`. [TO VERIFY DURING BUILD]: whether `tonic_prost_build::configure()` exposes `.file_descriptor_set_path()`. If not, fall back to two-pass: prost-build for messages+descriptor, then tonic-prost-build for stubs only.

#### `crates/turul-a2a-proto/src/lib.rs`

Add after the existing re-exports:

```rust
#[cfg(feature = "grpc")]
pub mod grpc {
    // Exact submodule paths depend on tonic-prost-build's snake_case output
    // for `service A2AService` in package `lf.a2a.v1`.
    pub use crate::lf::a2a::v1::a2a_service_server::{A2aService, A2aServiceServer};
    pub use crate::lf::a2a::v1::a2a_service_client::A2aServiceClient;
}
```

#### Verification

`cargo check -p turul-a2a-proto --features grpc` must resolve `turul_a2a_proto::grpc::A2aServiceServer` and `A2aServiceClient`.

---

## 4. turul-a2a Server Changes

All new files are in `crates/turul-a2a/src/grpc/` and compiled only under `#[cfg(feature = "grpc")]`. Add `#[cfg(feature = "grpc")] pub mod grpc;` to `crates/turul-a2a/src/lib.rs`.

#### `crates/turul-a2a/src/grpc/mod.rs`

Module root. Declares submodules `error`, `service`, `streaming`. Exports:

```rust
pub use service::GrpcService;

pub fn make_grpc_router(
    state: AppState,
    middleware_stack: Arc<crate::middleware::MiddlewareStack>,
) -> tonic::transport::server::Router { ... }
```

`make_grpc_router` builds a `tonic::transport::Server`, applies the `AuthLayer` as a Tower layer, and calls `.add_service(A2aServiceServer::new(GrpcService { state }))`. If `grpc-reflection` is enabled, adds the reflection service using the file descriptor registered from `OUT_DIR/a2a_descriptor.bin`. If `grpc-health` is enabled, adds the health service with `A2AService → SERVING`.

#### `crates/turul-a2a/src/grpc/error.rs`

```rust
pub fn a2a_to_status(err: A2aError) -> tonic::Status
pub fn middleware_to_status(err: MiddlewareError) -> tonic::Status
fn make_status_with_error_info(code: tonic::Code, reason: &str, msg: &str) -> tonic::Status
```

`make_status_with_error_info` uses `tonic_types::StatusExt::with_error_details` to attach `ErrorInfo { reason, domain: errors::ERROR_DOMAIN }`. Domain constant from `turul_a2a_types::wire::errors::ERROR_DOMAIN = "a2a-protocol.org"`.

Error mapping table (normative per ADR-014 §2.5; domain is `"a2a-protocol.org"`; every row MUST match ADR-014 §2.5 exactly):

| `A2aError` variant | gRPC `Status.code` | `ErrorInfo.reason` |
|---|---|---|
| `TaskNotFound` | `NOT_FOUND` | `TASK_NOT_FOUND` |
| `TaskNotCancelable` | `FAILED_PRECONDITION` | `TASK_NOT_CANCELABLE` |
| `PushNotificationNotSupported` | `UNIMPLEMENTED` | `PUSH_NOTIFICATION_NOT_SUPPORTED` |
| `UnsupportedOperation` | `FAILED_PRECONDITION` | `UNSUPPORTED_OPERATION` |
| `ContentTypeNotSupported` | `INVALID_ARGUMENT` | `CONTENT_TYPE_NOT_SUPPORTED` |
| `InvalidAgentResponse` | `INTERNAL` | `INVALID_AGENT_RESPONSE` |
| `ExtendedAgentCardNotConfigured` | `UNIMPLEMENTED` | `EXTENDED_AGENT_CARD_NOT_CONFIGURED` |
| `ExtensionSupportRequired` | `FAILED_PRECONDITION` | `EXTENSION_SUPPORT_REQUIRED` |
| `VersionNotSupported` | `FAILED_PRECONDITION` | `VERSION_NOT_SUPPORTED` |
| `InvalidRequest` / deser failure | `INVALID_ARGUMENT` | *(no ErrorInfo — non-A2A, per ADR-004)* |
| `Internal` (catch-all) | `INTERNAL` | *(no ErrorInfo — non-A2A, per ADR-004)* |

**Reason string constants are owned by `turul_a2a_types::wire::errors::REASON_*`** — the implementation MUST read them from that module, not hard-code the strings. Reasons listed above are shown for cross-reference with ADR-014 §2.5.

Auth layer errors:

| `MiddlewareError` variant | gRPC status code |
|---|---|
| `Unauthenticated` / `HttpChallenge` | `UNAUTHENTICATED` |
| `Forbidden` | `PERMISSION_DENIED` |
| `Internal` | `INTERNAL` |

#### `crates/turul-a2a/src/grpc/service.rs`

`pub struct GrpcService { pub(crate) state: AppState }`

Implements `turul_a2a_proto::grpc::A2aService`. All 9 unary RPCs follow this adapter pattern:

1. Extract `AuthIdentity` from tonic request extensions (set by `AuthLayer`).
2. Read `x-tenant-id` ASCII metadata (fallback: `tenant` field in proto request if present, else `"default"`).
3. Call the corresponding `core_*` function from `crates/turul-a2a/src/router.rs`.
4. Extract the `Json<serde_json::Value>` from the `Ok` result and deserialize to the expected proto response type via `serde_json::from_value`.
5. Map errors via `a2a_to_status`.

The `core_*` functions are `pub` or `pub(crate)` — both are accessible inside `turul-a2a`. The JSON round-trip (core returns `Json<serde_json::Value>`, adapter calls `serde_json::from_value`) is the correct approach: it reuses the same serialization path that HTTP uses and keeps business logic centralized.

Special cases:

- `SendMessage`: calls `core_send_message(state, tenant, owner, body_json)` where `body_json = serde_json::to_string(req.get_ref())`.
- `CreateTaskPushNotificationConfig`: calls `core_create_push_config(state, tenant, owner, task_id, body_json)` similarly.
- `GetExtendedAgentCard`: no `core_*` function exists. Calls `state.executor.extended_agent_card(claims.as_ref())` directly, mirroring the inline handler at `router.rs:391-402`.
- `DeleteTaskPushNotificationConfig`: returns `google.protobuf.Empty` — check for the empty JSON object from `core_delete_push_config`.

The two streaming RPCs (`SendStreamingMessage`, `SubscribeToTask`) delegate to helpers in `streaming.rs` and return `impl Stream`.

Anti-spoofing: the `extract_tenant_owner` helper reads `x-tenant-id` metadata but MUST NOT trust `x-authenticated-user` or `x-authenticated-tenant` metadata keys (ADR-007 §6 anti-spoofing applies identically). The `AuthLayer` strips or rejects these before the service sees them.

#### `crates/turul-a2a/src/grpc/streaming.rs`

Two public async functions returning `impl Stream<Item = Result<pb::StreamResponse, Status>>`.

Both replicate the replay-then-live loop from the SSE handlers (`router.rs:422-582`):

1. Subscribe to `TaskEventBroker::subscribe(task_id)` before any storage read.
2. For `SubscribeToTask`: read the task, return `FAILED_PRECONDITION` (via `a2a_to_status(A2aError::UnsupportedOperation { ... })`) if the task is already terminal — same check as `router.rs:546-553`.
3. Parse `a2a-last-event-id` ASCII metadata using `replay::parse_last_event_id`; extract sequence. Missing or invalid → `after_sequence = 0`.
4. Replay: call `A2aEventStore::get_events_after(tenant, task_id, after_sequence)`. Each `(seq, StreamEvent)` is serialized to `pb::StreamResponse` via `serde_json::to_value` + `serde_json::from_value`. Yield each as `Ok(response)`.
5. After replay, enter live tail: loop on broker `recv()`, re-read store from last seen sequence, yield new events.
6. On a terminal event in the stream, yield it and close the stream.

`a2a-last-event-id` format: `"{task_id}:{sequence}"` — identical to SSE convention (ADR-009 §2 / `replay::format_event_id`).

#### Builder change — `crates/turul-a2a/src/server/mod.rs`

One new method on `A2aServer` (name locked by ADR-014 §2.2; behind `#[cfg(feature = "grpc")]`):

```rust
/// Returns a fully-wired `tonic::transport::server::Router` — Tower
/// auth layer applied per ADR-007/ADR-014 §2.4, optional
/// reflection/health services added per enabled features.
///
/// This is the only public entry point to the gRPC surface in 0.1.7.
/// A raw, un-layered service accessor is deliberately not exposed
/// (ADR-014 §2.2): returning the inner `A2aServiceServer` would permit
/// adopters to compose it into a custom tonic server without the auth
/// layer, silently bypassing authentication.
pub fn into_tonic_router(self) -> tonic::transport::server::Router {
    let (state, _) = self.into_augmented_state();
    let middleware_stack = state.middleware_stack.clone();
    crate::grpc::make_grpc_router(state, middleware_stack)
}
```

`A2aServer::run()` is not modified in 0.1.7. Adopters who want to serve both HTTP and gRPC start two listeners themselves. [Open question §11.1: whether to add `.grpc_bind(addr)` to the builder and co-host automatically in `run()`.]

#### Auth integration

`A2aAuthLayer` (existing `crates/turul-a2a/src/middleware/layer.rs`) is a Tower `Layer<S>`. It reads HTTP headers from `http::Request::headers()` into a `RequestContext`, then calls the `MiddlewareStack`. tonic requests also implement `http::Request` — the layer composes directly on a tonic server:

```rust
let svc = tonic_service.layer(A2aAuthLayer::new(middleware_stack));
```

gRPC metadata keys treated as HTTP headers: `authorization` (Bearer / API key), `x-tenant-id`, `x-api-key`. Anti-spoofing: the auth layer strips `x-authenticated-user` and `x-authenticated-tenant` from gRPC metadata (same as HTTP per ADR-007 §6).

Helper `extract_tenant_owner` in `grpc/service.rs`: reads `AuthIdentity` from tonic request extensions (injected by the auth layer), reads `x-tenant-id` metadata (fallback: tenant field in proto request if present, then `"default"`).

---

## 5. turul-a2a-client Changes

New file: `crates/turul-a2a-client/src/grpc.rs` (compiled under `#[cfg(feature = "grpc")]`).

```rust
pub struct A2aGrpcClient {
    inner: turul_a2a_proto::grpc::A2aServiceClient<tonic::transport::Channel>,
    tenant: Option<String>,
    auth: ClientAuth,
}
```

Constructor:
```rust
pub async fn connect(endpoint: impl TryInto<tonic::transport::Endpoint, Error: Into<Box<dyn std::error::Error>>>)
    -> Result<Self, A2aClientError>
```

Public methods mirror `A2aClient` naming:
- `async fn send_message(req: impl Into<pb::SendMessageRequest>) -> Result<pb::SendMessageResponse, A2aClientError>`
- `async fn send_streaming_message(req: impl Into<pb::SendMessageRequest>) -> Result<impl Stream<Item=Result<pb::StreamResponse, A2aClientError>>, A2aClientError>`
- `async fn get_task`, `cancel_task`, `list_tasks` — same verb names.
- `async fn subscribe_to_task(task_id, last_event_id) -> Result<impl Stream<...>, A2aClientError>`
- Push config CRUD.

Auth injection per request: `ClientAuth::Bearer(t)` → `authorization: "Bearer {t}"` ASCII metadata; `ClientAuth::ApiKey { header, key }` → metadata key `header.to_lowercase()`, value `key`. Tenant: adds `x-tenant-id: {tenant}` ASCII metadata when set.

`MessageBuilder` from `crates/turul-a2a-client/src/builders.rs` is already transport-agnostic (produces `pb::SendMessageRequest`); no changes needed there.

Add to `crates/turul-a2a-client/src/lib.rs`:
```rust
#[cfg(feature = "grpc")]
pub mod grpc;
```

Add to `crates/turul-a2a-client/src/prelude.rs`:
```rust
#[cfg(feature = "grpc")]
pub use crate::grpc::A2aGrpcClient;
```

---

## 6. Test Strategy

#### Unit tests — `crates/turul-a2a/src/grpc/error.rs` (inline `#[cfg(test)]`)

- `error_mapping_all_variants` — each `A2aError` variant produces the expected gRPC code + `ErrorInfo.reason` + domain `"a2a-protocol.org"`.
- `middleware_error_mapping` — `MiddlewareError` variants → expected gRPC codes.

#### Integration tests — new file `crates/turul-a2a/tests/grpc_tests.rs`

Gated `#[cfg(feature = "grpc")]`. Starts a tonic server on a free port via `A2aServer::builder().build()?.into_tonic_router().serve(addr)` in `tokio::spawn`. Connects with `A2aGrpcClient::connect(addr)`.

Required tests (covers all 11 RPCs — 9 unary + 2 server-streaming — per ADR-014 §2.11):
1. `grpc_send_message_success` — unary; assert task in response.
2. `grpc_send_message_auth_failure` — no credentials → `UNAUTHENTICATED`.
3. `grpc_get_task_not_found` → `NOT_FOUND`, `ErrorInfo.reason = "TASK_NOT_FOUND"`, domain `"a2a-protocol.org"`.
4. `grpc_cancel_terminal_task` → `FAILED_PRECONDITION`, `ErrorInfo.reason = "TASK_NOT_CANCELABLE"`.
5. `grpc_list_tasks_pagination` — two pages, cursor stable, descending `updated_at`.
6. `grpc_push_config_crud_lifecycle` — create / get / list / delete round-trip (covers 4 RPCs).
7. `grpc_send_streaming_message_to_terminal` — stream emits events in sequence order; last event is terminal; stream closes.
8. `grpc_subscribe_to_task_terminal_rejection` → `FAILED_PRECONDITION`, `ErrorInfo.reason = "UNSUPPORTED_OPERATION"`.
9. `grpc_subscribe_to_task_last_event_id_replay` — reconnect with `a2a-last-event-id: {task_id}:{sequence}` metadata; asserts replayed events have `event_sequence > given`, no duplicates, no gaps within stored range (per ADR-014 §2.11).
10. `grpc_extended_agent_card_success` — executor returns a card; assert fields.
11. `grpc_tenant_scoped_send_message` — sends with `x-tenant-id` metadata; asserts task stored under that tenant.

Default test gate: `cargo test -p turul-a2a --features "sqlite grpc"`.

#### Spec compliance extension — `crates/turul-a2a/tests/spec_compliance.rs`

Per ADR-014 §2.11, HTTP+JSON, JSON-RPC, and gRPC MUST share the core-handler parity suite. Under `#[cfg(feature = "grpc")]`, add a gRPC transport variant running the same assertions as the existing HTTP and JSON-RPC columns. The compliance matrix gains a third row. Implementation: extract a `ComplianceDriver` trait (or parameterized function set) from the existing per-transport test bodies; gRPC calls the same assertions through `A2aGrpcClient`. Streaming-specific additions:

- Same-tenant SSE and gRPC subscribers see identical event sequence numbers (shared event store is the invariant).
- `a2a-last-event-id` replay assertion under gRPC (ADR-009 §14 equivalents).

This refactor is contained within `spec_compliance.rs`. Same storage backend across all three transport runs per ADR-014 §2.11.

---

## 7. Documentation

- **`/Users/nick/turul-a2a/docs/adr/ADR-001-proto-first-architecture.md`**: add a note in the deferred section: "gRPC transport: implemented in 0.1.7 per ADR-014."
- **`/Users/nick/turul-a2a/docs/adr/ADR-005-dual-transport-shared-core-handlers.md`**: update "gRPC deferred" note to reference ADR-014 and 0.1.7.
- **`/Users/nick/turul-a2a/CLAUDE.md`**: move gRPC from `§Deferred` to `§Completed`. Update `§Out of scope` to remove gRPC. Add `--features grpc` variants to `§Build & Development Commands`.
- **`/Users/nick/turul-a2a/README.md`**: one paragraph on enabling gRPC via `--features grpc`, co-hosting pattern, and Lambda exclusion.
- **`/Users/nick/turul-a2a/CHANGELOG.md`**: 0.1.7 entry: gRPC transport, all 11 RPCs, streaming with `a2a-last-event-id`, auth parity, `grpc-reflection` / `grpc-health` sub-features, `turul-a2a-client` grpc feature.
- **Example update** (phase 2, after server-side lands): `examples/echo-agent/src/main.rs` — add `AgentCardBuilder::url("grpc://localhost:50051", "GRPC", "1.0")`. Do not enable `grpc` feature on the example binary by default; add a README note for grpc invocation.

---

## 8. Build Sequence (Ordered Commits)

| # | Commit message | Files touched | Bisect gate |
|---|---|---|---|
| 1 | `deps: add tonic 0.14 workspace dependencies` | Root `Cargo.toml` | `cargo check --workspace` |
| 2 | `proto: add grpc feature + tonic-prost-build wiring` | `crates/turul-a2a-proto/Cargo.toml`, `build.rs`, `src/lib.rs` | `cargo check -p turul-a2a-proto --features grpc` |
| 3 | `turul-a2a: add grpc/error.rs with A2aError → tonic::Status mapping` | `crates/turul-a2a/Cargo.toml`, `src/grpc/mod.rs`, `src/grpc/error.rs`, `src/lib.rs` | `cargo test -p turul-a2a --features grpc --lib grpc::error` |
| 4 | `turul-a2a: grpc service stubs for all 9 unary RPCs` | `crates/turul-a2a/src/grpc/service.rs` | `cargo check -p turul-a2a --features grpc` |
| 5 | `turul-a2a: grpc streaming for SendStreamingMessage + SubscribeToTask` | `crates/turul-a2a/src/grpc/streaming.rs` | `cargo check -p turul-a2a --features grpc` |
| 6 | `turul-a2a: auth Tower layer on tonic server + into_tonic_router builder method` | `crates/turul-a2a/src/grpc/mod.rs`, `src/server/mod.rs` | `cargo check -p turul-a2a --features grpc` |
| 7 | `tests: grpc integration suite (all 11 RPCs)` | `crates/turul-a2a/tests/grpc_tests.rs` | `cargo test -p turul-a2a --features "sqlite grpc" grpc_` |
| 8 | `tests: spec_compliance extended for grpc transport axis` | `crates/turul-a2a/tests/spec_compliance.rs` | `cargo test -p turul-a2a --features "sqlite grpc" spec_compliance` |
| 9 | `client: grpc feature + A2aGrpcClient` | `crates/turul-a2a-client/Cargo.toml`, `src/grpc.rs`, `src/lib.rs`, `src/prelude.rs` | `cargo test -p turul-a2a-client --features grpc` |
| 10 | `examples: echo-agent grpc card advertisement` | `examples/echo-agent/src/main.rs` | `cargo check -p echo-agent` |
| 11 | `docs: ADR-001/005 cross-refs, CLAUDE.md, README, CHANGELOG` | ADR files, `CLAUDE.md`, `README.md`, `CHANGELOG.md` | Documentation review |
| 12 | `release: workspace 0.1.6 → 0.1.7` | Root `Cargo.toml`, intra-workspace dep versions | Full pre-publish gate (CLAUDE.md §"Release & Publish") |

Commits 7 and 9 are independent after commit 6; they may be developed in parallel. Commit 8 depends on 7 (uses the same test harness pattern).

---

## 9. Verification Gates

```bash
# Default HTTP-only build — must be unchanged
cargo build --workspace
cargo test --workspace

# gRPC feature builds
cargo build --workspace --features grpc
cargo test -p turul-a2a --features "sqlite grpc"
cargo test -p turul-a2a-client --features grpc
cargo clippy --workspace --features grpc -- -D warnings
cargo fmt --all -- --check
cargo doc --no-deps --features grpc   # must be clean on turul-a2a and turul-a2a-client

# Pre-publish dry-run (every crate)
cargo package -p turul-a2a-proto --no-verify --allow-dirty
cargo package -p turul-a2a --no-verify --allow-dirty
cargo package -p turul-a2a-client --no-verify --allow-dirty
# (remaining crates)
```

**Manual smoke with reflection enabled:**
```bash
cargo run -p echo-agent --features grpc,grpc-reflection
grpcurl -plaintext localhost:50051 list
# → lf.a2a.v1.A2AService
grpcurl -plaintext -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":{"text":"hello"}}]}}' \
  localhost:50051 lf.a2a.v1.A2AService/SendMessage
```

---

## 10. Risk and Rollback

| Risk | Mitigation | Rollback |
|---|---|---|
| `tonic-prost-build` does not expose `.file_descriptor_set_path()` for pbjson | Use two-pass build: prost-build (messages + descriptor) then tonic-prost-build (stubs only, message gen disabled) | Revert commits 1–2; HTTP-only builds unaffected |
| `AuthLayer` Tower layer incompatible with tonic server composition | Both use `tower::Service`; `grpc_send_message_auth_failure` test (commit 7) catches divergence | Revert commits 5–6 |
| Co-hosting complexity (single port vs two ports) | Default to two separate listeners; adopters compose themselves | Single-port left as open question, does not block 0.1.7 |
| tonic MSRV rises above workspace MSRV | Feature-gated (`grpc` is off by default); MSRV bump is a workspace-level decision | Drop the `grpc` feature; HTTP-only builds pay no cost |
| Lambda adopters enabling `grpc` by mistake | `turul-a2a-aws-lambda/Cargo.toml` has no `grpc` feature; enabling it is a compile error on the lambda crate | Lambda crate comment documents the exclusion explicitly |
| JSON round-trip in unary RPCs introducing serialization drift | The same `serde_json` + pbjson path that HTTP uses; covered by spec-compliance three-transport parity in commit 8 | Same core function, same serialization — divergence is impossible by construction |

---

## 11. Open Questions

1. **Co-hosting: single port vs two ports.** Two-listener default (axum on HTTP port, tonic on HTTP+1) is safe and orthogonal. Single-port via `tonic::transport::Server` + axum integration requires `h2c` protocol negotiation. Resolve during commit 6 implementation. If single-port is chosen, `A2aServerBuilder` needs a `.grpc_bind(addr)` setter and `run()` must `tokio::spawn` both servers.

2. **`tonic-prost-build` file descriptor path API.** Verify at commit 2 that `tonic_prost_build::configure().file_descriptor_set_path(path)` writes the descriptor for pbjson consumption. If the API differs from `prost_build::Config`, use the two-pass approach.

3. **Builder API for gRPC port.** `A2aServerBuilder` currently has a single `bind_addr`. Options: (a) convention (HTTP port + 1 = gRPC port), (b) new `.grpc_bind(addr)` setter, (c) leave co-hosting entirely to the adopter. Option (c) is the 0.1.7 default — `into_tonic_router()` returns a tonic router; adopters call `.serve(addr)` themselves.

4. **`grpc-reflection` default in example agents.** Enabling reflection in `echo-agent` during development aids grpcurl testing but deviates from the production-off posture. Decision: enable via `--features grpc-reflection` as an opt-in argument in the README; do not bake it into example's default features.

5. **Streaming reconnect with in-memory storage.** `a2a-last-event-id` replay requires events to persist across reconnects. In-memory storage does not survive a restart. The streaming integration tests in `grpc_tests.rs` MUST use `sqlite` (via `--features "sqlite grpc"`) to prove durable reconnect. Add a `#[doc]` comment to `grpc_subscribe_to_task_last_event_id_replay` explaining this requirement.

6. **`AuthLayer` extension injection compatibility with tonic.** The existing `AuthLayer` was written for axum; tonic uses the same `tower::Service` interface but the request type is `http::Request<BoxBody>`. Commit 6 must verify `AuthLayer` injects `RequestContext` / `AuthIdentity` into `http::Request::extensions_mut()` rather than any axum-specific path, and that tonic surfaces those extensions through to the service impl.

---

## Dependency Graph

```
1 (workspace deps)
    ↓
2 (proto grpc feature)
    ↓
3 (error mapping)
    ↓
4 (unary service stubs)
    ↓
5 (streaming)
    ↓
6 (auth + builder)
    ↓
7+8 (integration + spec-compliance tests)     9 (client)
    ↓                                              ↓
               10+11 (examples + docs)
                        ↓
                    12 (release)
```
