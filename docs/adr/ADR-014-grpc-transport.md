# ADR-014: gRPC Transport as a Third Thin Adapter

- **Status:** Accepted
- **Date:** 2026-04-21
- **Depends on:** ADR-001 (proto-first architecture), ADR-004 (error model),
  ADR-005 (dual-transport shared-core handlers), ADR-006 (SSE streaming /
  `last_chunk` semantics), ADR-007 (auth middleware), ADR-008 (Lambda
  adapter), ADR-009 (durable event coordination), ADR-010 (executor
  `EventSink`)
- **Spec reference:** A2A v1.0 §1.4 ("the proto is the single authoritative
  normative definition"); `proto/a2a.proto` lines 19-140 (`service A2AService`);
  A2A v1.0 §4 ("Transports") — gRPC binding

## 1. Context

ADR-005 committed the workspace to two wire transports — HTTP+JSON and
JSON-RPC — sharing a single core-handler layer. gRPC was explicitly
deferred. It is item #1 on the Deferred list in `CLAUDE.md` §"Deferred
(ordered by priority)" and is the final-remaining major transport required
to close the A2A v1.0 surface. The normative gRPC service is already present
in the vendored proto (`proto/a2a.proto:19-140`) — 11 RPCs matching one-to-one
to the HTTP and JSON-RPC methods, two of them server-streaming
(`SendStreamingMessage`, `SubscribeToTask`). No protocol design is required;
this ADR decides *how* to expose the already-defined service without
forking the shared-core invariants established by ADR-005, ADR-006, ADR-007,
and ADR-009.

Today:

- `crates/turul-a2a-proto/build.rs` calls `prost_build` + `pbjson_build`.
  `tonic-build` is NOT invoked. No service stubs exist.
- `turul-a2a-aws-lambda` (ADR-008) is request/response + buffered
  `/message:stream` over Lambda Function URLs. It does NOT hold persistent
  HTTP/2 connections.
- `AgentCard.AgentInterface.protocol_binding` already accepts `GRPC`
  (`proto/a2a.proto:340-343`); `AgentCardBuilder::url(..., "GRPC", ...)`
  already works (`crates/turul-a2a/src/card_builder.rs:332`). No schema
  work is needed on the advertisement side.
- Business logic is concentrated in `core_*` functions in
  `crates/turul-a2a/src/router.rs` (ADR-005 §"Decision"). Both HTTP and
  JSON-RPC dispatch through the same functions.
- Streaming is fed from `A2aAtomicStore` + `A2aEventStore` with per-task
  monotonic `event_sequence` (ADR-009 §§1-4); the in-process
  `TaskEventBroker` is a wake-up signal, not a data path (ADR-009 §11).
- Auth is a Tower layer (`A2aMiddleware` stack, ADR-007 §§3-5) with
  `AuthIdentity` extension injection and anti-spoofing rules on
  `X-Authenticated-User` / `X-Authenticated-Tenant`.
- Error handling is an `A2aError` → wire-format mapping (ADR-004 §§3-6)
  with mandatory `google.rpc.ErrorInfo` payload.

This ADR is about adding a third adapter behind these same contracts, NOT
about redesigning any of them. Anything gRPC-specific that doesn't fit
those contracts is an indication the adapter is wrong, not the core.

## 2. Decision

### 2.1 gRPC is a third thin adapter over the shared core

The gRPC surface is implemented as a tonic `A2aService` impl that
translates `tonic::Request<pb::*Request>` ↔ `tonic::Response<pb::*Response>`
by calling the **same** `core_*` functions HTTP and JSON-RPC already
call. gRPC MUST NOT own business logic, validation, state-machine
enforcement, storage interaction, or streaming orchestration. ADR-005's
invariant ("no behavior fork between transports") extends to a third
transport with zero relaxation.

Concretely:

- `SendMessage` / `SendStreamingMessage` / `GetTask` / `ListTasks` /
  `CancelTask` / `SubscribeToTask` / the four push-config RPCs / and
  `GetExtendedAgentCard` all dispatch into existing `core_*` functions.
- Per-RPC request translation extracts the tenant (from
  `pb::*Request.tenant` when present; from `x-tenant-id` metadata
  otherwise; default tenant fallback matches HTTP behaviour).
- Owner is injected by the auth layer via the same `AuthIdentity`
  extension mechanism used by HTTP (ADR-007 §§3-5); the gRPC adapter
  reads it from the request extensions tonic surfaces, not from a
  parallel interceptor stack.

Rationale: a three-way parity obligation (HTTP / JSON-RPC / gRPC) is only
tractable if the core is one. Every divergence adds combinatorial test
burden and creates silent compliance drift. ADR-005's third-transport
touch-point ("add core function, add HTTP route, add JSON-RPC dispatch
case") becomes a four-touch-point list with this ADR: core + HTTP route
+ JSON-RPC case + gRPC RPC case. The core function remains the single
source of truth.

### 2.2 Feature gating and build-pipeline integration (verified)

Verified against the workspace as of 2026-04-21:
`crates/turul-a2a-proto/build.rs` runs `prost_build::Config::new()` +
`pbjson_build::Builder::new()` unconditionally and does NOT invoke
`tonic_build` (the file is 36 lines; no tonic-related code path
exists today). gRPC codegen MUST be additive and MUST NOT change
default-feature behaviour of that build script.

**Locked feature names** (normative — do not rename without amending
this ADR):

- `turul-a2a-proto` feature `grpc`: enables optional build-dependencies
  on tonic 0.14's codegen crates (`tonic-prost-build` and/or
  `tonic-build` per tonic 0.14's split) and emits tonic server + client
  stubs for `lf.a2a.v1.A2AService` in a `grpc` submodule of
  `turul-a2a-proto`, gated by `#[cfg(feature = "grpc")]`.

  The exact `build.rs` mechanism (descriptor sharing with `prost-build`
  vs. two-pass generation vs. an alternative tonic entry point) is
  an implementation decision resolved in the proto-crate commit of
  the implementation plan. Whatever shape `build.rs` takes, the
  following stable invariants MUST hold:

    1. **Default builds unchanged.** Without `--features grpc`,
       `build.rs` MUST produce byte-identical output to today's
       `prost-build` + `pbjson-build` pipeline. `cargo build -p
       turul-a2a-proto` with no features MUST be a no-op observable
       diff relative to 0.1.6.
    2. **Feature-gated at compile time.** HTTP-only adopters MUST
       NOT pay any tonic dependency weight; tonic and its codegen
       crates MUST NOT enter the dependency graph without an explicit
       `--features grpc`.
    3. **Type consistency.** The prost message types, pbjson serde
       impls, and tonic service stubs MUST refer to the same
       underlying proto source and produce a single set of Rust
       types. `pb::SendMessageRequest` as seen by the HTTP/JSON-RPC
       handlers under default features MUST be the identical Rust
       type seen by the tonic-generated service trait under
       `--features grpc` — not a parallel regenerated copy.

- `turul-a2a` feature `grpc` (MUST enable `turul-a2a-proto/grpc`
  transitively): exposes `A2aServer::into_tonic_router()` plus a
  `grpc` module with the service impl. `into_tonic_router()` always
  applies the Tower auth stack per §2.4 — this is the only public
  entry point into the gRPC surface in 0.1.7. A raw service
  accessor (e.g. `grpc_service()` returning the un-layered
  `A2aServiceServer`) is deliberately omitted: exposing the
  unauthenticated inner service as a public API would permit
  adopters to compose it into a custom tonic server without the
  Tower layer and silently bypass authentication. If advanced
  composition is required in a later release, the accessor MUST be
  named to make its unauthenticated nature explicit (e.g.
  `unauthenticated_grpc_service()`) and documented accordingly; 0.1.7
  does not ship that path.
- `turul-a2a-client` feature `grpc`: enables a thin tonic-client
  wrapper module (`A2aGrpcClient`). Rationale for the name — symmetric
  with server-side `grpc` on `turul-a2a` and `turul-a2a-proto`,
  because the client crate only re-exports the tonic-generated client
  stub from `turul-a2a-proto` under a thin ergonomic wrapper; it does
  not generate an independent client surface. Single-word `grpc`
  keeps the feature namespace consistent across all three crates
  (see §2.7).

All three features default to **off**. The HTTP+JSON default tree
(`cargo build -p turul-a2a`) MUST NOT add a single tonic crate to
the dependency graph — HTTP-only adopters pay zero weight for gRPC.

**Optional sub-features on `turul-a2a`** (§2.9 is normative; neither
is implied by `grpc` alone):

- `grpc-reflection` (enables `tonic-reflection`): serves
  `grpc.reflection.v1alpha.ServerReflection`. Off by default.
  Enabling `grpc` alone does NOT pull in `tonic-reflection`.
- `grpc-health` (enables `tonic-health`): serves `grpc.health.v1.Health`.
  Off by default. Enabling `grpc` alone does NOT pull in `tonic-health`.

MSRV MUST NOT rise for this change. tonic's current MSRV is compatible
with the workspace's 1.85 pin; if a future tonic release raises the
floor, that's a separate workspace-level decision — this ADR MUST NOT
pre-raise MSRV for a feature that is off by default.

### 2.3 Streaming — single source of truth

`SendStreamingMessage` and `SubscribeToTask` server-streams MUST consume
the same durable event path as SSE (ADR-009 §§3-4, §7). The gRPC adapter
SHALL NOT open a parallel event pipeline, SHALL NOT subscribe to the
broker for data, and SHALL NOT assign its own event sequences.

Concretely:

- The gRPC stream handler calls the same replay-then-live helper that
  feeds the SSE encoder (`crates/turul-a2a/src/streaming/*`), receives
  `StreamEvent` values tagged with `(task_id, event_sequence)`, and
  serializes each into a `pb::StreamResponse` proto message.
- `last_chunk` (ADR-006) has a two-layer persistence contract that
  gRPC inherits unchanged from SSE:
    1. It is **not** persisted as task artifact state —
       `A2aTaskStorage::append_artifact` explicitly drops it (see the
       `_last_chunk` parameter in all four backend impls and the
       contract note at `crates/turul-a2a/src/storage/traits.rs:104-109`).
    2. It **is** persisted as stream-event metadata inside the
       `ArtifactUpdatePayload` field of `StreamEvent::ArtifactUpdate`
       in the durable event store
       (`crates/turul-a2a/src/streaming/mod.rs:65-80`), so replay
       and reconnection preserve chunk boundaries for every
       transport.

  The gRPC adapter conveys `last_chunk` through the corresponding
  `pb::StreamResponse` field exactly as the SSE encoder emits it —
  the value is read from the persisted `ArtifactUpdatePayload`, not
  invented by the adapter.
- Terminal `SubscribeToTask` semantics are unchanged across transports:
  the shared core raises `A2aError::UnsupportedOperation`
  (CLAUDE.md §"Spec-aligned behaviors"; ADR-009 §12 supersession;
  ADR-010 §4.3). The gRPC adapter MUST NOT perform its own
  terminal-state check and MUST NOT invent a transport-specific
  error. The canonical mapping in §2.5 maps this variant to
  `FAILED_PRECONDITION` (state-based rejection — the operation
  exists and is implemented, but the task's state makes it
  inapplicable). Using `UNIMPLEMENTED` here would be semantically
  wrong: the service **does** implement `SubscribeToTask` — it is
  refusing this specific request on state grounds.
- `Last-Event-ID`-equivalent resume for gRPC: the client sends
  `a2a-last-event-id` ASCII metadata (value = UTF-8
  `"{task_id}:{sequence}"` matching the SSE convention in ADR-009
  §2); the adapter parses it and calls the same replay entry point
  as the HTTP handler. Missing or unparseable metadata means "start
  from 0" — same behaviour as SSE without the header. The `-bin`
  suffix MUST NOT be used; the value is printable ASCII by
  construction.
- Spec §3.1.6 / §9.4.6 first-event semantics (first event is the `Task`
  object) are enforced by the shared core, not by the gRPC adapter.

Under ADR-006's single-instance local-broker model, multi-instance gRPC
streaming inherits ADR-009's correctness guarantees (durable store is
truth, broker is wake-up). No new streaming coordination mechanism is
introduced by this ADR.

### 2.4 Auth — same Tower stack, metadata-mapped

Tonic servers compose `tower::Service` layers. The gRPC transport MUST
reuse the existing `A2aMiddleware` stack (ADR-007 §§3-5) as a Tower
layer on the tonic server — not a tonic `Interceptor`, not a parallel
auth pipeline. Specifically:

- The same `A2aAuthLayer` that wraps the axum HTTP router wraps the
  tonic server. Both expose the `AuthIdentity` extension on the inner
  request so the downstream service reads it identically.
- gRPC metadata keys are treated as HTTP headers for auth purposes.
  All auth-relevant metadata is ordinary ASCII metadata (no `-bin`
  suffix): `authorization` (Bearer / API key per `BearerMiddleware`
  / `ApiKeyMiddleware`), `x-tenant-id` (tenant scoping), `x-api-key`
  (alternative key carrier). First-party middleware MUST NOT consult
  binary (`-bin`-suffixed) metadata.
- ADR-007 §6 anti-spoofing rules apply identically: if a client sends
  `x-authenticated-user` or `x-authenticated-tenant` metadata, the
  middleware MUST strip or reject it before dispatching. The adapter
  MUST NOT trust metadata keys the HTTP layer would not trust.
- `SecurityContribution` propagation into the executor (ADR-007
  §§7-8) uses the same `ExecutionContext` field; the gRPC adapter
  does not add a new pathway.

`AgentCard.security_schemes` remains agent-level (CLAUDE.md §"Deferred"
item 2). Skill-level security is out of scope for this ADR just as for
HTTP and JSON-RPC today.

### 2.5 Error mapping — `A2aError` → `tonic::Status` with `ErrorInfo`

**Normative requirement.** Every gRPC response carrying an A2A error
MUST include a `google.rpc.ErrorInfo` message in `Status.details`
with `reason` set per the table below and `domain = "a2a-protocol.org"`
(the value verified in-tree at `crates/turul-a2a-types/src/wire.rs`
line 83 as `ERROR_DOMAIN`). Reason strings are the exact constants
from `wire::errors::REASON_*`. Tonic's own default status mapping
MUST NOT be relied upon — every `A2aError` variant passes through a
centralised `A2aError` → `tonic::Status` conversion, mirroring the
existing centralised HTTP and JSON-RPC conversions (ADR-004).

The mapping table is normative and is the single source of truth for
the gRPC column. Every row was verified against
`crates/turul-a2a-types/src/wire.rs` (HTTP / JSON-RPC constants and
REASON strings) and `crates/turul-a2a/src/error.rs` (`A2aError`
variants) on 2026-04-21.

| A2A variant / source                        | HTTP | JSON-RPC | gRPC `Status.code`   | `ErrorInfo.reason`                      |
|---------------------------------------------|------|----------|----------------------|-----------------------------------------|
| `A2aError::TaskNotFound`                    | 404  | -32001   | `NOT_FOUND`          | `TASK_NOT_FOUND`                        |
| `A2aError::TaskNotCancelable`               | 409  | -32002   | `FAILED_PRECONDITION`| `TASK_NOT_CANCELABLE`                   |
| `A2aError::PushNotificationNotSupported`    | 400  | -32003   | `UNIMPLEMENTED`      | `PUSH_NOTIFICATION_NOT_SUPPORTED`       |
| `A2aError::UnsupportedOperation` (state)    | 400  | -32004   | `FAILED_PRECONDITION`| `UNSUPPORTED_OPERATION`                 |
| `A2aError::ContentTypeNotSupported`         | 415  | -32005   | `INVALID_ARGUMENT`   | `CONTENT_TYPE_NOT_SUPPORTED`            |
| `A2aError::InvalidAgentResponse`            | 502  | -32006   | `INTERNAL`           | `INVALID_AGENT_RESPONSE`                |
| `A2aError::ExtendedAgentCardNotConfigured`  | 404  | -32007   | `UNIMPLEMENTED`      | `EXTENDED_AGENT_CARD_NOT_CONFIGURED`    |
| `A2aError::ExtensionSupportRequired`        | 400  | -32008   | `FAILED_PRECONDITION`| `EXTENSION_SUPPORT_REQUIRED`            |
| `A2aError::VersionNotSupported`             | 400  | -32009   | `FAILED_PRECONDITION`| `VERSION_NOT_SUPPORTED`                 |
| `A2aError::InvalidRequest` / deser failure  | 400  | -32602   | `INVALID_ARGUMENT`   | *(no ErrorInfo — non-A2A, per ADR-004)* |
| `A2aError::Internal` (catch-all)            | 500  | -32603   | `INTERNAL`           | *(no ErrorInfo — non-A2A, per ADR-004)* |
| Auth failure (`MiddlewareError`, 401)       | 401  | —        | `UNAUTHENTICATED`    | *(transport auth; not A2aError — ADR-007)* |
| Authz failure (`MiddlewareError`, 403)      | 403  | —        | `PERMISSION_DENIED`  | *(transport auth; not A2aError — ADR-007)* |

Notes on the two state-based `FAILED_PRECONDITION` rows:

- `UnsupportedOperation` is returned when the service implements the
  RPC but the current resource state makes it inapplicable (most
  notably: `SubscribeToTask` against a terminal task, per CLAUDE.md
  §"Spec-aligned behaviors"). `FAILED_PRECONDITION` — not
  `UNIMPLEMENTED` — is the canonical gRPC code for
  "implemented-but-refused-due-to-state".
- `PushNotificationNotSupported` maps to `UNIMPLEMENTED` because
  the error is structurally a *capability* absence (the deployment
  has not wired a push-notification backend, per ADR-011), not a
  state-based refusal. This is the one A2A variant where
  `UNIMPLEMENTED` is correct.

`AuthenticatedExtendedCardNotConfigured` (-32007) likewise reports
absent capability (the extended-card endpoint has no configured
override), mapping to `UNIMPLEMENTED`.

Non-A2A errors (`InvalidRequest`, `Internal`) carry no `ErrorInfo`
— this matches ADR-004's decision that framework-level errors are
not protocol-level and do not carry `domain`/`reason`. Transport auth
failures (401/403) are produced by the auth layer before dispatch
(ADR-004 §"Transport auth is NOT modeled in `A2aError`"; ADR-007);
under gRPC they surface from the Tower auth layer as
`tonic::Status::unauthenticated` / `tonic::Status::permission_denied`
with a human-readable message, no `ErrorInfo`.

A2A v1.0's gRPC binding does not publish a normative A2A-variant →
gRPC-status table independent of the `google.rpc.Code` conventions
applied above; this table is the workspace's adoption of those
conventions, mechanically consistent with ADR-004 §"Key mappings".
If the upstream spec later publishes a divergent table, this ADR
is amended; until then this table is the authority.

### 2.6 Lambda — out-of-scope for `turul-a2a-aws-lambda` in the current release

The A2A protocol is **not** gRPC-unfriendly. gRPC-over-HTTP/2 is a
first-class A2A binding and is fully supported by this framework on
any deployment target that provides a persistent HTTP/2 server
endpoint: ECS, Fargate, AppRunner, EKS / vanilla Kubernetes, bare
VMs, on-prem — all supported. The constraint is specific to the AWS
Lambda execution environment:

- AWS Lambda's invocation model is request-scoped. Execution
  environments MAY be frozen indefinitely between invocations
  (ADR-008 §"Context"), and the runtime does not hold a persistent
  HTTP/2 server socket across invocations.
- Tonic servers require long-lived HTTP/2 connections; server-
  streaming RPCs (`SendStreamingMessage`, `SubscribeToTask`) require
  the connection to remain open for the duration of the stream.
- Neither condition is satisfiable under the current Lambda + API
  Gateway / Function URL surface.

Decision for this release:

- `turul-a2a-aws-lambda` MUST NOT expose a gRPC adapter.
- The `grpc` feature on `turul-a2a` MUST NOT be transitively enabled
  by `turul-a2a-aws-lambda`'s default or opt-in features, to
  prevent accidental misconfiguration.
- `AgentCard` advertisement: an operator who deploys via Lambda
  exclusively MUST NOT declare a `GRPC` interface on the card. The
  framework does not enforce this (the card is operator-authored),
  but the ADR-008 Lambda README MUST state it and the card-builder
  docs SHOULD cross-reference this ADR.
- Operators who want gRPC alongside a Lambda surface run a second
  deployment on a persistent host (ECS/Fargate, AppRunner, etc.)
  and advertise both interfaces on the card — this is an adopter
  deployment-architecture concern, not a framework concern.

This mirrors ADR-008 §"Streaming deferred" posture: where the
deployment environment is incompatible with a feature, the adapter
for that environment omits the feature rather than shipping a
half-broken approximation. If a future Lambda runtime surface
offers persistent HTTP/2 (an Envoy / ALB gRPC fronting arrangement
that terminates on non-Lambda compute already works today and is
out-of-scope here), a follow-up ADR evaluates it.

### 2.7 Client story — single crate, opt-in `grpc` feature

A single client crate, `turul-a2a-client`, gains an optional `grpc`
feature (name-locked per §2.2) that enables a `grpc` module exposing
`A2aGrpcClient` — a thin ergonomic wrapper over the tonic-generated
client stub re-exported from `turul-a2a-proto`. The existing
HTTP/JSON-RPC `A2aClient` is unchanged.

Rationale for one crate (vs. spinning `turul-a2a-grpc-client`):

- The A2A spec treats transport as an interchangeable binding, not a
  separate protocol; adopters choose per-interface, not per-SDK.
- `MessageBuilder`, `ClientAuth`, `ListTasksParams`, and the prelude
  produce proto types that tonic-generated clients accept directly.
  Duplicating that surface across two crates is pure carrying cost.
- Release discipline stays simpler (one fewer publish crate, no new
  LICENSE symlink set — see CLAUDE.md §"Release & Publish" item 9).
- The feature name `grpc` is consistent across `turul-a2a-proto`,
  `turul-a2a`, and `turul-a2a-client`. Because `turul-a2a-client`
  does not generate an independent client surface — it re-exports
  the proto's generated tonic client under a wrapper — the symmetric
  single-word `grpc` name is preferable to `grpc-client` (the latter
  would misleadingly imply a separately-generated artifact).

The `grpc` feature on `turul-a2a-client` enables
`turul-a2a-proto/grpc`. Default features stay HTTP-only. Client
discovery (`.well-known/agent-card.json`) reads `AgentCard` and
selects an interface by `protocol_binding`; the client MAY expose
helpers to pick `GRPC` when the `grpc` feature is enabled.

### 2.8 TLS — deployer-owned (same stance as HTTP)

A2A v1.0 §7.1 requires TLS 1.2+ in production. This framework defers
TLS termination to the deployer's reverse proxy / load balancer /
service-mesh sidecar — the same posture as HTTP today (CLAUDE.md
§"Out of scope for the framework release"). The gRPC adapter builds
plaintext `h2c` tonic servers; operators MUST front them with a TLS
terminator (ALB, Envoy, Istio/Linkerd mesh, nginx, etc.). The framework
does NOT ship rustls/openssl bindings as default or opt-in dependencies
on the server crate.

Clients MAY connect with TLS via tonic's standard transport config;
that's an adopter concern in `A2aGrpcClient` construction, not a
framework-imposed policy.

### 2.9 Reflection and health — optional sub-features

- `grpc.reflection.v1alpha.ServerReflection` behind
  `grpc-reflection`. Rationale: grpcurl / Postman / BloomRPC / buf
  CLI all rely on reflection; turning it on is a one-line developer
  quality-of-life win and costs nothing when off. Off-by-default is
  correct because production deployments often disable reflection
  for attack-surface reasons.
- `grpc.health.v1.Health` behind `grpc-health`. Rationale: load
  balancers and service meshes (Envoy, ALB gRPC health checks, k8s
  readiness probes) consume this. The default health status for
  `A2AService` is `SERVING` when the server is accepting requests
  and `NOT_SERVING` during graceful shutdown. Off-by-default so
  adopters can substitute their own health schema if they compose
  multiple services.

Neither sub-feature affects correctness; both are convenience layers
around the same tonic server.

### 2.10 Scope for 0.1.7 (MVP) vs. further-deferred

**In 0.1.7 (gRPC MVP):**

- `tonic-build`-generated server stubs behind `turul-a2a-proto/grpc`.
- All 11 RPCs implemented as adapters over the existing `core_*`
  functions.
- Auth via the existing `A2aMiddleware` Tower stack.
- Streaming (SendStreamingMessage / SubscribeToTask) from the durable
  event store with `a2a-last-event-id` resume.
- Error mapping per §2.5.
- Spec-compliance suite (`crates/turul-a2a/tests/spec_compliance.rs`)
  extended with a third transport axis under `--features grpc` —
  same assertions, three transports.
- `grpc-reflection` and `grpc-health` sub-features available opt-in.
- Example agent: one of the existing agents gains a `--transport grpc`
  flag; no new example crate (unless reflecting adopter demand during
  implementation review).

**Deferred beyond 0.1.7:**

- gRPC-Web (`tonic-web`). Useful for browsers; no immediate adopter
  demand. Separate ADR if pursued.
- Bidirectional streaming. The A2A proto declares no bidi-stream RPC
  (`proto/a2a.proto:19-140` — streams are all server-streaming); the
  framework MUST NOT invent one.
- xDS / client-side load balancing configuration.
- Skill-level `security_requirements` (already deferred per CLAUDE.md).
- gRPC over Lambda (§2.6 — not a deferral, an explicit non-goal).

### 2.11 Test obligations (normative)

The spec-compliance suite at
`crates/turul-a2a/tests/spec_compliance.rs` extends its transport
axis from `{HTTP+JSON, JSON-RPC}` to `{HTTP+JSON, JSON-RPC, gRPC}`.
gRPC assertions are gated by `--features grpc`; the HTTP and
JSON-RPC axes remain enabled unconditionally.

**Mandatory — RPC coverage.** Under `--features grpc`, all 11 RPCs
MUST be exercised by at least one spec-compliance test case each,
across all three transports:

- 9 unary RPCs: `SendMessage`, `GetTask`, `ListTasks`, `CancelTask`,
  `GetExtendedAgentCard`, `CreateTaskPushNotificationConfig`,
  `GetTaskPushNotificationConfig`,
  `ListTaskPushNotificationConfigs`,
  `DeleteTaskPushNotificationConfig`.
- 2 server-streaming RPCs: `SendStreamingMessage`,
  `SubscribeToTask`.

Exactly the same assertions (state transitions, error codes,
owner/tenant enforcement, push-config CRUD lifecycle, streaming
first-event == `Task`, terminal-subscribe rejection, pagination
cursor stability, `ListTasks` ordering — CLAUDE.md §"Spec-aligned
behaviors") run on all three transports. No transport MAY diverge
from another on any of these.

**Mandatory — core-handler sharing is enforced by parity.** ADR-005's
"no behavior fork between transports" invariant (extended by this
ADR to three transports) is a structural test obligation: a bug
introduced in a `core_*` function MUST surface identically on HTTP,
JSON-RPC, and gRPC. If only one transport fails a compliance
assertion, the gRPC adapter (or another adapter) has leaked
business logic and MUST be corrected before merge.

**Mandatory — streaming tests.**

- `SendStreamingMessage` (gRPC): full event stream from initial
  `Task` event through to terminal event; ordering preserved;
  every event carries the same `(task_id, event_sequence)` it
  would carry over SSE. The durable event store is the source of
  truth (ADR-009 §§3-4, §7); the gRPC stream MUST NOT invent its
  own sequence numbers.
- `SubscribeToTask` (gRPC):
  - Attach-to-non-terminal + replay-from-0: identical event set
    to the SSE equivalent.
  - `Last-Event-ID` reconnection: client sends
    `a2a-last-event-id: {task_id}:{sequence}` ASCII metadata;
    adapter delivers only events with `event_sequence >` the
    given value; no duplicates; no gaps within the stored range.
  - Terminal-subscribe rejection: subscribing to a task in a
    terminal state MUST fail with the A2A
    `UnsupportedOperationError`, surfaced as
    `tonic::Status::failed_precondition` with `ErrorInfo { reason
    = "UNSUPPORTED_OPERATION", domain = "a2a-protocol.org" }`
    per §2.5. The adapter MUST NOT perform its own state check —
    the rejection comes from the shared core.

**Mandatory — same storage backend across all three transports per
parity run.** Each parity run constructs one `A2aServer` with one
storage backend and exercises all three transports against that
single server instance. gRPC tests do not bring their own storage
stack; they share the in-memory default used by the rest of
`spec_compliance.rs`. Per-backend parity (SQLite, PostgreSQL,
DynamoDB) remains the responsibility of ADR-003 / ADR-009 storage
parity tests and is not re-done at the transport layer — but when
those backend-specific tests run the spec-compliance suite, they
MUST run the gRPC axis too.

**Streaming cross-transport observability regression guard.** A
same-tenant / same-task SSE subscriber and gRPC subscriber attached
to the same server MUST agree on event sequence numbers — they
share the event store, so divergence would indicate the gRPC
adapter has acquired a parallel event source (forbidden by §2.3).
This is a regression guard, not a new invariant.

### 2.12 Same-backend / push-delivery invariants unchanged

ADR-009 §8 (event store and task store share the same backend) and
ADR-013 §4.3 (`.storage()` does not auto-wire push delivery;
`.push_delivery_store(...)` is explicit) apply verbatim. Adding gRPC
does not relax either constraint. The `A2aServer::builder()`
consistency checks run identically whether the server is subsequently
served as axum, as tonic, or both.

## 3. Rationale

- **Proto-first stays proto-first (ADR-001).** The gRPC service is
  defined in the proto; this ADR does not re-specify it and cannot
  drift from it. `tonic-build` is another code generator operating
  on the same normative source as `prost-build` and `pbjson-build`.
- **Shared core stays the source of truth (ADR-005).** The marginal
  cost of a third transport is one adapter thin enough to be
  reviewable in a single sitting. Any business logic that sneaks
  into the tonic impl is a bug.
- **Streaming stays correct across instances (ADR-009).** The
  durable event store already solves the cross-instance streaming
  problem for SSE; gRPC inherits that solution by consuming the
  same source.
- **Auth stays Tower-native (ADR-007).** tonic was designed to
  compose with `tower::Layer`; an interceptor-based fork would be
  a self-inflicted wound.
- **Lambda stays supported by NOT supporting gRPC there.** The
  framework's Lambda story is its most complex deployment path;
  adding a half-working gRPC mode would add more surface area than
  value. ADR-008's posture ("don't ship what doesn't fit") applies.
- **Default dependency tree stays HTTP-only.** Feature-gating is
  strict on both the proto crate and the server crate; HTTP-only
  adopters don't pay for gRPC.

## 4. Trade-offs

- **Dependency surface when enabled.** Enabling `grpc` on
  `turul-a2a` pulls in tonic + h2 + http2-stack — ~20 transitive
  crates. Acceptable because it's opt-in; unacceptable if it
  leaked into defaults.
- **Test-matrix growth.** Three-transport parity triples some
  streaming test execution time. Mitigation: gRPC tests run only
  under `--features grpc`; CI runs the matrix on one platform,
  default HTTP-only on the others.
- **`#[non_exhaustive]` on tonic-generated client types.** tonic's
  generated requests and responses are `#[non_exhaustive]` for
  forward compatibility. Adopters constructing these directly must
  use builders or update syntax; this is tonic's standard contract,
  not a framework-introduced constraint.
- **MSRV sensitivity.** tonic releases occasionally raise MSRV.
  Pinning tonic through workspace dependencies contains blast
  radius; an MSRV bump on tonic does NOT automatically bump the
  workspace MSRV as long as the feature is off by default.
- **Reflection and health are tonic-specific.** Enabling either
  sub-feature adds small tonic-side crates (`tonic-reflection`,
  `tonic-health`). Both are standard and well-maintained; no
  alternatives evaluated.
- **Client-side proto-type ergonomics.** gRPC clients work natively
  with raw proto types. `MessageBuilder` already produces proto
  types that tonic accepts, so ergonomic surface is reused; no
  wrapper round-trip needed. If future ergonomic drift shows up
  (e.g. wrapper-only features leaking into HTTP but not gRPC),
  that's a signal to promote the missing helper to the wrapper
  layer, consistent with CLAUDE.md §"Example and API Surface Policy".

## 5. Rejected alternatives

#### 5.1 Separate `turul-a2a-grpc` crate re-implementing handlers

**Rejected.** Violates ADR-005's shared-core invariant and
multiplies the parity-test obligation. A separate crate would
have to either (a) depend on `turul-a2a` for core handlers
(correct, but then the crate boundary is purely organisational
overhead) or (b) re-implement them (bug-for-bug divergence
guaranteed). Neither option adds value; the single-crate
`turul-a2a[grpc]` feature is strictly better.

#### 5.2 tonic `Interceptor` for auth

**Rejected.** tonic interceptors are a parallel abstraction to
Tower layers — they run before the service but don't compose
with the existing `A2aMiddleware` stack. Using them would fork
auth semantics between HTTP and gRPC, violating ADR-007 §§3-5.
tonic supports Tower layers natively; there is no reason to
prefer the narrower interceptor API.

#### 5.3 gRPC events via a dedicated event pipeline

**Rejected.** Duplicates ADR-009 §§3-4 without adding capability.
A parallel stream topology (e.g. gRPC consumes the broker while
SSE consumes the store) would either desynchronise
`event_sequence` or require the broker to carry store data,
both of which ADR-009 §11 explicitly rejects. gRPC reads from
the same durable-store replay path as SSE — no exceptions.

#### 5.4 gRPC over Lambda via API Gateway HTTP/2 proxy

**Rejected as out-of-scope for 0.1.x.** API Gateway's HTTP/2
support does not preserve long-lived server streams in the
Lambda execution model — streams terminate at the adapter
boundary at the latest at Lambda timeout. Attempting a "buffered
gRPC stream" mirror of the buffered SSE path would require
re-serializing streaming RPCs as finite unary responses,
violating the gRPC contract on the wire and surprising standard
gRPC clients (they would see an inexplicable early stream close
with no trailer). If an HTTP/2-capable Lambda target (e.g. API
Gateway v2 with enhanced streaming, or a future Lambda runtime
surface) becomes viable, a follow-up ADR would evaluate it; for
now this is NOT shipped.

#### 5.5 Always-on tonic dependency

**Rejected.** Forces every HTTP-only adopter — including the
Lambda adopter, which cannot use gRPC at all — to pull in
~20 tonic-stack transitive crates. The workspace's
lightweight-by-default posture (ADR-001 rationale) is
incompatible with an unconditional tonic dependency.

#### 5.6 Reflection and health on by default

**Rejected.** Reflection leaks the entire proto surface to any
caller who can reach the port — reasonable for development,
risky as a default for production. Health is schema-coupling:
if the adopter composes `A2AService` alongside their own gRPC
service, the framework's health server would collide with
theirs. Opt-in is the correct posture for both.

## 6. Consequences

**Positive:**

- A2A v1.0's final major transport (gRPC) ships, closing the
  Deferred-list item #1.
- Three-transport parity is structural (one core) rather than
  aspirational — same discipline as ADR-005 two-transport.
- HTTP-only adopters incur zero dependency or binary-size cost.
- Lambda adopters are explicitly and predictably unaffected.
- Streaming correctness carries over from ADR-009 with no
  additional coordination mechanism.
- Auth, error mapping, owner/tenant scoping, state-machine
  enforcement, and push-config CRUD all reuse existing code
  paths — the new surface is bounded.
- Reflection and health are standard gRPC affordances when
  adopters want them, and absent when they don't.

**Negative:**

- Workspace CI grows a matrix axis (`--features grpc`) that
  must be run regularly to catch regressions.
- `turul-a2a-proto` build graph gets a second code-generator
  path; `build.rs` complexity increases. Mitigation: feature-
  gate the tonic-build block; when off, `build.rs` is
  unchanged from today.
- Adopters deploying to both Lambda (no gRPC) and a non-Lambda
  target (gRPC available) must author two `AgentCard`s or omit
  the `GRPC` interface entry from the Lambda advertisement.
  This is a documentation obligation, not a framework one, but
  it is a real adopter friction surface.
- tonic's MSRV trajectory is outside workspace control; if
  tonic ever outpaces the workspace MSRV, the `grpc` feature
  becomes a de facto higher-MSRV opt-in.
- Three rejected-alternatives constraints (no separate crate,
  no interceptor auth, no parallel event pipeline) narrow
  future refactoring options; if any of them later becomes
  desirable, this ADR must be amended before implementation.

