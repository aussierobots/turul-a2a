# Changelog

All notable changes to the `turul-a2a` workspace are documented here.
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Format inspired by [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed — `examples/skill-manifest-ollama-agent` now consumes `turul-llm` (pinned git rev)

- Replaces the inline `reqwest`-based Ollama call with a dependency on
  `turul-llm-core::LlmClient` + `turul-llm-ollama::OllamaClient` from
  the sibling [`turul-llm`](https://github.com/aussierobots/turul-llm)
  workspace. Pinned to rev
  `52eff3c2cbf63efea11dcef7686ef588b9aa79a6` so a clean clone of
  `turul-a2a` builds without requiring a sibling checkout on disk.
- Offline-stub mode is unchanged.
- The provider-neutral invariant on the framework dep-graph still
  binds: no publishable `turul-a2a` crate depends on `turul-llm-*`;
  only `examples/skill-manifest-ollama-agent` does. See ADR-023
  revision 3.
- Workspace `Cargo.toml` adds `turul-llm-core` and `turul-llm-ollama`
  as git deps under `[workspace.dependencies]` with a dep-direction
  guard comment. Versioned crates.io migration deferred until
  `turul-llm` ships its first stable release.

## [0.1.22] — 2026-05-23

### Added — ADR-022 skill-invocation dispatcher profile (Accepted + implemented)

- **ADR-022 Accepted.** Phase A red-phase tests landed; first-profile placement decided (inside `turul-a2a` as `pub mod profile_dispatch`, per ADR-021 §5 single-profile rule); interop probe via the new showcase agent + clients (below).
- New `pub(crate)` dispatcher in `crates/turul-a2a/src/profile_dispatch.rs`: parses the `A2A-Extensions` request header, validates against `AgentCard.capabilities.extensions[]`, echoes activated URIs on the response, rejects required-but-missing extensions with `UnsupportedOperationError`. Wired into HTTP `/message:send` + `/message:stream`, JSON-RPC `/jsonrpc`, and gRPC `SendMessage` + `SendStreamingMessage`. 13 tests in `crates/turul-a2a/tests/profile_dispatch_tests.rs` (8 pure-unit + 5 axum end-to-end).
- Public constant `pub const SKILL_INVOCATION_PROFILE_V1: &str = "https://turul.dev/a2a/extensions/skill-invocation/v1"` available as `turul_a2a::profile_dispatch::SKILL_INVOCATION_PROFILE_V1` for adopter use.
- New `AgentCardBuilder::extension(AgentExtension)` setter — additive non-breaking method letting adopters declare advertised extensions in their agent card.

### Added — showcase agent + interop clients for the dispatcher profile

- `examples/skill-dispatch-profile-agent/` (port 3015) — multi-skill agent with `echo_loud` + `reverse` skills (both SKILL.md-backed). Advertises the profile URI; reads `Message.metadata["a2a.skillId"]` + `["a2a.skillParams"]` for routing per ADR-022 §2.3. 9 tests (5 unit + 4 smoke).
- `examples/interop-clients/skill-dispatch-profile/{python,go,rust}/` — three per-language clients exercising the dispatcher end-to-end (header activation + metadata-keyed routing + response echo verification). All three smoke-verified.
- `examples/interop-clients/CLIENT_MATRIX.md` grows the new row; matrix is now 5 agents × 3 languages = 15 cells, all manually verified.

### Added — `turul-a2a-client::A2aClient::with_extensions`

- Additive `pub` builder method on `A2aClient` for sending the `A2A-Extensions` HTTP header alongside outbound requests. Enables Rust adopters to activate server-side profile extensions (e.g. the skill-invocation dispatcher) ergonomically without dropping to a raw HTTP client. ~15 LOC patch; zero existing API changed; all 20 `client_tests` still pass.

### Revised — ADR-023 disposition

- ADR-023 (LlmClient abstraction) revised from "Proposed" to **Accepted with cross-repo decision**: the LLM-client abstraction lives in a separate `turul-llm` GitHub repo, not in this workspace. `turul-a2a` stays provider-neutral; examples MAY optionally consume `turul-llm` crates once that repo ships its first stable releases. Until then, provider calls stay example-local. Original §4 trait sketch and §7 open questions preserved as seed material for `turul-llm`'s own design ADRs.

### Internal

- ADR-022 review-status section + Phase B acceptance commit precede the Phase C implementation commit in `git log`, mirroring the pattern established by ADR-021.
- `.claude/agents/adr-review.md` subagent definition added — codifies the precommit ADR-compliance review (comment-style, dep-direction, newtype bridge, async_trait ergonomics, SKILL.md coverage, wrapper-API policy, generated-artifact contamination, public-API parity).
- Comment-style rule tightened: `CLAUDE.md` + `AGENTS.md` both reject ADR/§/Phase/Wave refs in source comments. The earlier in-workspace ADR refs (235 hits across 51 files) have been scrubbed; surviving `§N`-pattern matches are all A2A spec / JSON-RPC / RFC citations (durable external anchors, explicitly permitted).

## [0.1.21] — 2026-05-23

### Added — `turul-a2a-patterns` skill-pattern crate (ADR-021 Phase C)

- New **path-only** workspace crate `crates/turul-a2a-patterns/` (`publish = false`, `version.workspace = true`) hosting reusable A2A skill-authoring abstractions. Per ADR-021 §2.1 / §4.1, the crate is intentionally NOT published until §4 gates clear (two real adopters + dispatcher decision + test coverage + downstream impact survey). The crate is consumed today by example agents via workspace path.
- Public surface (six primary + one Q5-resolved additive):
  - `SkillHandler` — `#[async_trait]` async trait; impls write plain `async fn run(&self, params, sink) -> Result<Value, SkillError>`.
  - `SkillProgressSink` + `ProgressState` (non-terminal states only, `#[non_exhaustive]`) + `SinkError` (`Closed` / `Backend`) — `#[async_trait]` per Wave 8 ergonomics pass.
  - `SkillRegistry` trait + `InMemorySkillRegistry` impl; `register_manifest(card, handler)` is the canonical path.
  - `SkillCard` + SKILL.md manifest parser (camelCase frontmatter, JSON Schema 2020-12 strict-reject-unknown-keywords, `{{ var }}` template grammar with dotted paths, missing-variable structured error). Three-section frontmatter: discovery / provider-neutral execution metadata / opaque `providerConfig`.
  - `SkillDescriptor` — single source of truth for `params_schema` (derived from manifest input schema for manifest-backed skills; supplied once at registration for programmatic skills; never adopter-fillable independently).
  - `SkillError` — two variants only (`InvalidRequest`, `Internal`).
  - `TerminalHook` + `SkillOutcome` — Q5-resolved minimal generic variant. `#[async_trait]`. No framework-level isolation/timeout semantics in v1 (richer extractor-registry variant remains deferred pending the dispatcher ADR).
  - `validate_json(schema, instance) -> Result<(), ValidationError>` — public free function for arbitrary schema validation.
- 26 tests across 8 test files; full §7.4 gate clean.

### Added — showcase example agents (ADR-021 §7.3 step 6 + §9 Q3/Q5)

- `examples/skill-manifest-ollama-agent/` — primary SKILL.md reference example. LLM-backed `greet` skill. Offline-by-default; live Ollama opt-in via `OLLAMA_BASE_URL` / `RUN_OLLAMA_SMOKE` env vars. `dotenvy` autoload of local `.env` (gitignored); `.env.example` committed with `OLLAMA_BASE_URL=http://localhost:11434` placeholder.
- `examples/agent-role-planner-router-agent/` — deterministic planner + router idiom example (no LLM). Skills `add`, `concat` are manifest-backed.
- `examples/agent-role-critic-agent/` — deterministic critic/evaluator idiom example (no LLM). Skills `validate_against_schema` (uses `turul_a2a_patterns::validate_json`) and `check_invariants` (four invariant kinds) are manifest-backed.
- `examples/post-task-hook-agent/` — `TerminalHook` demonstration. Skills `count`, `metrics` are manifest-backed; the hook is example-level code per ADR-021 §9 Q5.
- Every callable showcase skill is manifest-backed via `SKILL.md`. The role/hook patterns (planner, router, critic, terminal hook) stay code-first because they're orchestration/observation idioms, not skills — each example's README explains why.
- All examples carry novice-focused READMEs (architecture sketch, per-client run commands, expected I/O, troubleshooting, ADR cross-references).

### Added — cross-language interop client matrix (ADR-021 §7.3 step 8)

- `examples/interop-clients/<agent>/<language>/` — 12 client paths (4 agents × 3 languages). Each subdir is a self-contained client crate/script with novice-focused README. No shared abstraction module — duplication is preferred over cleverness for adopter readability.
  - Python: `a2a-sdk==1.0.2` (PyPI). Selects JSON-RPC transport at `/jsonrpc`; sends `SendStreamingMessage` (SSE). Uses `ClientFactory(ClientConfig(...)).create(card)` constructor.
  - Go: `github.com/a2aproject/a2a-go/v2 v2.3.1`. Selects JSON-RPC transport at `/jsonrpc`; sends `SendMessage` (non-streaming). Uses `agentcard.DefaultResolver.Resolve(...)` + `a2aclient.NewFromCard(...)`.
  - Rust: `turul-a2a-client` (in-workspace). Uses REST `/message:send` per the proto's `google.api.http` annotation.
- `examples/interop-clients/CLIENT_MATRIX.md` — agent-rows × language-columns matrix. All 12 cells manually smoke-tested. The framework's `A2aServer` serves both REST and JSON-RPC simultaneously; clients differ only in which surface their SDK selects.

### Added — ADR-022 (Proposed) + ADR-023 (Proposed)

- ADR-022 — Skill-invocation dispatcher profile extension. Specifies the four-point profile contract (declaration via `AgentCapabilities.extensions`, activation via `A2A-Extensions` header, request shape via `Message.metadata`, rejection via `UnsupportedOperationError`). Implementation gated on ADR-022 acceptance.
- ADR-023 — LlmClient abstraction. **Decision (2026-05-23):** the LLM-client abstraction lives in a separate `turul-llm` GitHub repository, not in this workspace. `turul-a2a` stays provider-neutral; examples may optionally consume `turul-llm` crates once that repo ships its first stable releases. Until then, provider calls stay example-local. See ADR-023.

### Internal

- Workspace deps added: `async-trait = "0.1"`, `dotenvy = "0.15"`, `jsonschema = "0.30"`, `serde_yaml = "0.9"`.
- `examples/clients/` (the earlier generic-client layout) removed; replaced by per-agent `examples/interop-clients/<agent>/<language>/` structure.
- ADR-021 amended through Phase C to reflect: orphan-rule-safe newtype bridge in examples (§2.3), `async_trait` adopter ergonomics (§2.3, §9 Q5), reduced-expressivity `securityRequirements` in v1 SKILL.md (§2.2 item 3), explicit "patterns crate ships zero role abstractions; planner/router/critic/post-task-hook are examples only" (§9 Q3), `a2aproject/a2a-go/v2 v2.3.1` Go SDK research result (§7.3 step 8).

## [0.1.20] — 2026-05-21

### Fixed — `BearerMiddleware::with_required_scopes` now enforced at runtime

- `turul-a2a-auth`: `BearerMiddleware` previously accepted required scopes via `with_required_scopes(...)` and advertised them on the `AgentCard` via `SecurityContribution`, but never checked them at request time. The `turul-a2a-auth` README documented a "Required scope missing (Bearer) → 403 / `error="insufficient_scope"`" failure mode that did not exist. Adopters relying on the documented contract were authenticating callers whose tokens did not carry the required scopes.
- Enforcement now runs after JWT validation and principal extraction: every entry in `required_scopes` must appear as an exact SP-delimited token in the OAuth 2.0 `scope` claim, per the RFC 6749 §3.3 ABNF `scope = scope-token *( SP scope-token )`. Tabs and other whitespace are NOT delimiters; substring matches do not count.
- Failure surfaces as `MiddlewareError::HttpChallenge(AuthFailureKind::InsufficientScope)`, which now resolves to **HTTP 403** with a `WWW-Authenticate: Bearer realm="a2a", error="insufficient_scope"` header per RFC 6750 §3. All other `HttpChallenge` kinds continue to resolve to 401. This is the only special case in `MiddlewareError::http_status`.
- Documentation alignment: the `turul-a2a-auth` README's "Reading identity in your executor" example previously referenced `ctx.identity` and `AuthIdentity::Authenticated { .. }`; `ExecutionContext` exposes `owner` and `claims` as direct fields and never had an `identity` field. The example now reads `ctx.owner` and `ctx.claims` directly.

### Fixed — gRPC auth failure wire surface matches HTTP

- The gRPC auth Tower layer was emitting `format!("{err:?}")` into `grpc-message`, which leaked Debug-formatted variant names (`Unauthenticated(InvalidApiKey)`) and — for `MiddlewareError::Internal(String)` — the inner payload itself (e.g. JWKS URLs, validator internals). This reproduced the original ADR-016 motivating regression on the gRPC transport even though the HTTP path had been fixed.
- Both transports now route the wire string through a new `MiddlewareError::wire_body_string()` helper, the single source of truth for the snake_case identifier. `Internal(_)` always collapses to `"internal_error"` on both transports; the inner String is never exposed.
- Status-code mapping addendum: `HttpChallenge(InsufficientScope)` is HTTP 403 (RFC 6750 §3) on the HTTP path and `PERMISSION_DENIED` on gRPC (gRPC has no 403 equivalent under `UNAUTHENTICATED`; `PERMISSION_DENIED` is the closest match, same code used for `Forbidden(_)`).
- New gRPC parity tests pin both `Status::code()` and `Status::message()` for `invalid_api_key`, `invalid_token`, `insufficient_scope`, and `internal_error`, including a regression assertion that `Internal`'s inner String never reaches the wire.

### Added — non-breaking middleware ergonomics

- `RequestContext::from_headers(headers: HeaderMap) -> Self` consolidates the four previously-duplicated struct-literal builders in the HTTP and gRPC auth layers. Adopter middleware that needs to synthesise a context (e.g. for replay, tests) gets a single entry point instead of a five-field literal.
- `MiddlewareStack::empty()` replaces `MiddlewareStack::new(vec![])` boilerplate. Convenience for test harnesses and Lambda adapters wiring `AppState` without auth.
- `PUBLIC_PATHS` / `VERSION_EXEMPT_PATHS` constants in `layer.rs` and `transport.rs` consolidated into one `BYPASS_PATHS` const + `is_bypass_path` helper in `middleware/mod.rs`. Internal refactor; behaviour unchanged.
- Trait and module documentation on `A2aMiddleware` clarified to describe the auth / authorisation / inbound credential forwarding contract. Non-auth interception (metrics, rate-limiting, tracing) should be a separate Tower layer composed outside this trait.

### Known limitation

- The `scp` claim (array form, used by some IdPs including Microsoft Entra) is not consulted. Only the OAuth 2.0 `scope` claim (single space-delimited string) is recognised. Adopters whose IdP emits `scp` instead of `scope` should either configure the IdP to emit `scope`, or layer their own scope check in `AgentExecutor`. A future release may add explicit `scp` support behind an opt-in.

### Compatibility

- Patch release per project versioning rule ("Patch bumps cover compatible runtime changes"). The wire contracts for both scope rejection (HTTP) and gRPC `Status::message()` were already advertised — for HTTP via the documented failure-modes table, for gRPC implicitly via the HTTP parity contract this release makes explicit. Adopters that called `with_required_scopes(...)` without realising it was inert will now see 403 responses for tokens that previously authenticated successfully. Adopters reading `tonic::Status::message()` for assertions will need to update from Debug-formatted strings (`"Unauthenticated(InvalidApiKey)"`) to the canonical snake_case (`"invalid_api_key"`). The added ergonomics methods are additive — no removals or signature changes.

### Deferred to 0.2.0 (no work scheduled)

- `RequestContext::bearer_token` and `RequestContext::extensions` field removal, and `MiddlewareError::Internal(String)` reshape to `Internal(Box<dyn std::error::Error + Send + Sync>)`. See [ADR-020](docs/adr/ADR-020-auth-middleware-contract-cleanup.md) for the full plan, downstream impact survey, and migration guidance. The `A2aMiddleware` trait rename and the `compat-v03` feature-to-runtime promotion are explicitly off the table per the same ADR.

## [0.1.19] — 2026-05-18

### Fixed — publish gate compatible with older clippy

- The 0.1.18 suppression `#[allow(clippy::useless_borrows_in_formatting)]` on the pbjson-generated module raised `unknown_lints` on toolchains where that lint name does not yet exist. Combined with `-D warnings` in the publish gate, this aborted `cargo clippy` before any crate could be packaged.
- Suppression now reads `#[allow(unknown_lints, clippy::useless_borrows_in_formatting)]` so the same source compiles cleanly on both older and newer clippy.
- No runtime, wire, or contract change. Library behavior is identical to 0.1.18.

## [0.1.18] — 2026-05-18

### Added — opt-in least-privilege bootstrap for SQL backends (ADR-019)

- `PostgresConfig` and `SqliteConfig` gain a new field `assume_schema_initialized: bool` (default `false`). When set to `true`, `PostgresA2aStorage::new()` and `SqliteA2aStorage::new()` skip DDL entirely — no `CREATE TABLE`, no `ALTER TABLE`, no `CREATE INDEX`. The runtime then trusts that the operator has provisioned the schema out-of-band (Terraform, sqlx-migrate, hand-rolled SQL).
- Motivation: a deployed agent that uses Postgres should be able to run with `INSERT/UPDATE/DELETE/SELECT` grants on the six `a2a_*` tables only — no `CREATE`/`ALTER` on the schema. Today the framework demands schema-create privilege on every boot, which is more privilege than steady-state operation requires.
- Default is `false`, so existing adopters see zero behavior change. Setting the flag is an explicit transfer of forward-migration responsibility to the operator: future releases that change the schema will document the delta in this CHANGELOG, and adopters running with the flag MUST apply the delta with a higher-privilege role before rolling the new binary.
- DynamoDB is unchanged. `DynamoDbA2aStorage::new()` already does no DDL — tables are provisioned out-of-band, and the runtime IAM principal can omit `dynamodb:CreateTable` / `UpdateTable` / `DeleteTable` today. No flag needed.
- See [ADR-019](docs/adr/ADR-019-least-privilege-storage-bootstrap.md) for the full rationale, including rejected alternatives (separate constructor, transparent detection, default-on) and follow-ups (shipping canonical migration files).

### Schema delta from 0.1.17

None. The schema produced by `create_tables()` is unchanged from 0.1.17. Operators upgrading with `assume_schema_initialized: true` have no migration to apply on this release.

## [0.1.17] — 2026-05-10

### Changed — JSON-RPC `id: null` rejection (small wire tightening)

- Explicit `"id": null` on a JSON-RPC request is now rejected with `-32600 Invalid Request`. The rejection response carries `id: null` per JSON-RPC 2.0 §5.1 (id MUST be Null when the request id cannot be detected/used).
- Notifications continue to be signalled by the **absence** of the `id` key (→ HTTP 204, no body) per §4.1. Only the explicit-null shape is affected.
- First-party clients are unaffected: `turul-a2a-client` does not construct JSON-RPC envelopes (HTTP+JSON / gRPC only), so no client-side guard is required. Third-party callers that previously sent `id: null` must omit the key (notification semantics) or send a string/number id.

### Changed — internal JSON-RPC dispatch via `turul-rpc 0.1`

- The inner non-streaming dispatch table for `/jsonrpc` now routes through `turul-rpc 0.1`. A2A still owns the full outer wire boundary: parse error, envelope validation, `id: null` rejection, notification handling, params-shape validation (`-32602` on array/scalar), `compat-v03` normalization, method-not-found wording (`"Method not found"`, no method name), and streaming.
- `turul-rpc` only sees a `JsonRpcRequest` constructed from already-validated state with a `RequestId` proven to be `String|Number(i64)` and a `RequestParams::Object` — never `Array`. Unknown methods, exotic numeric ids (non-i64), and other edge cases fall back to the legacy direct path so wire bytes are unchanged.
- `impl turul_rpc::r#async::ToJsonRpcError for A2aError` adapts our existing error model (code, message, `google.rpc.ErrorInfo` carried in `data`) onto turul-rpc's error-object trait. The full error envelope shape (including `ErrorInfo`) is unchanged on the wire.

### Added — runtime dependency

- `turul-rpc = "0.1"` (default-features off, `async` feature) added as a runtime dependency of `turul-a2a`. No new crate published, no new public API exposed.

### Unchanged in this release

- **Streaming** (`SendStreamingMessage`, `SubscribeToTask`): SSE wire format and dispatch path are untouched. `turul-rpc::StreamingJsonRpcDispatcher` is not used.
- **Batch**: A2A does not currently advertise a JSON-RPC batch surface; that is unchanged.
- **`compat-v03`**: method/param/response normalization is unchanged.
- **JSON-RPC error envelopes** (success and error response shapes, including `data: google.rpc.ErrorInfo`): byte-identical to 0.1.16 except for the new `id: null` rejection above.
- **All 11 JSON-RPC method names**, HTTP routes, and proto-driven wire shapes: unchanged.

## [0.1.16] — 2026-04-25

### Changed — push-config client API hides proto (breaking)

- **Breaking on the push-config CRUD methods** in both `turul-a2a-client::A2aClient` and `turul-a2a-client::grpc::A2aGrpcClient`:
  - `create_push_config(&task_id, url, token)` — 3-arg ergonomic call replacing the old `(&task_id, pb::TaskPushNotificationConfig)` shape. No proto types in adopter code.
  - `create_push_config_with(&task_id, PushConfig)` — escape hatch for configs that need `authentication`, `tenant`, or other advanced fields.
  - `get_push_config` / `list_push_configs` now return `turul_a2a_types::PushConfig` / `PushConfigPage` instead of the raw proto types. `PushConfigPage` carries `next_page_token: Option<String>` so pagination is preserved.
- **`turul-a2a-types`** gains:
  - `PushConfig` — wrapper over `TaskPushNotificationConfig` with `id()` / `task_id()` / `url()` / `token()` / `authentication()` / `tenant()` accessors and `as_proto()` / `into_proto()` escape hatches.
  - `PushConfigBuilder::new(url, token)` with optional `.task_id(...)` / `.tenant(...)` / `.authentication(PushAuth)` setters.
  - `PushAuth::new(scheme, credentials)` mirroring `AuthenticationInfo`.
  - `PushConfigPage::new(configs, next_page_token)` for paginated list responses.
- Brings push-config into line with `send_message`, `get_task`, etc. — adopter code no longer needs `use turul_a2a_proto as pb` or `..Default::default()` literals.

## [0.1.15] — 2026-04-25

### Added — `MessageBuilder::reference_task_ids` + Life-of-a-Task examples

- **`turul-a2a-client::MessageBuilder::reference_task_ids(I)`** — wires `Message.reference_task_ids` for refinement requests. Required by the new `conversation-agent` example; useful for any adopter implementing the A2A spec's task-refinement flow.

- **`examples/conversation-agent`** (port 3007) — demonstrates task refinement within a single `contextId`. Turn 1: originating task with artifact `sailboat_image.png` + `artifactId=sailboat-v1`. Turn 2: refinement message with `referenceTaskIds=[task1.id]` and same `contextId` → new task (task immutability per spec), same artifact name, new `artifactId=sailboat-v2`. Smoke test asserts `task1.id != task2.id`, `contextId` stable, artifact name reused, `artifactId` differs, v2 body references `task1.id`.

- **`examples/interrupting-agent`** (port 3008) — demonstrates the `INPUT_REQUIRED` interrupted state. Turn 1: pauses the task with a question message; no artifact yet. Turn 2: client sends follow-up with the same `taskId` and `contextId` carrying the answer; executor reads history, completes the task, emits booking artifact. Smoke test asserts `task.id` stable across turns, state transitions `INPUT_REQUIRED → COMPLETED`, artifact contains both the original request and the answer.

- **Lambda example skill declarations** — `lambda-agent` (`lambda-echo`), `lambda-durable-agent` / `lambda-durable-worker` / `lambda-durable-single` (`durable-echo`) now declare a skill on the agent card. Discovery clients see what each agent can do.

- **Per-example verification scripts** under `examples/<name>/scripts/` for the three Lambda examples:
  - `lambda-durable-single/scripts/life-of-a-task.sh` — drives the deployed Function URL through the spec's refinement flow (POST originate → GET → POST refinement → GET), asserts task immutability + `contextId` continuity + both terminals `COMPLETED`. 8 assertions.
  - `lambda-durable-agent/scripts/life-of-a-task.sh` — same flow against the two-Lambda topology.
  - `lambda-agent/scripts/smoke.sh` — single POST + GET against an HTTP-only Lambda.
  - All take `$1` (or `FN_URL` / `A2A_FN_URL` env), exit with the failure count.

### Added — shared pbjson conversion + `Message.metadata` on the client builder

- **`turul-a2a-types::pbjson`** — new module with `json_to_value`, `value_to_json`, `json_object_to_struct`, and `struct_to_json_object` for conversions between `serde_json::Value` and proto `google.protobuf.Value` / `Struct` (re-exported from `turul_a2a_proto::pbjson_types`). Adopters setting `Message.metadata` or `Part.metadata` no longer need to hand-roll the ~20 lines of recursive conversion. Numeric precision note: proto `NumberValue` is `f64`, so `serde_json` integers collapse to floats through the conversion (documented in the module).

- **`turul-a2a-client::MessageBuilder::metadata_json`** + **`metadata`** — new builder methods for populating `Message.metadata`. `metadata_json(HashMap<String, serde_json::Value>)` is the ergonomic form (converts via `pbjson::json_object_to_struct`); `metadata(pbjson_types::Struct)` takes a pre-built proto `Struct`. Calling `metadata_json` with an empty `HashMap` leaves `metadata` as `None` on the wire.

- **Drop-in replacement** for code that previously constructed `SendMessageRequest` by hand to attach `Message.metadata`:
  ```rust
  // Before:
  let base: pb::SendMessageRequest = MessageBuilder::new().data(req).into();
  let mut message = base.message.unwrap();
  message.metadata = Some(my_json_to_pbjson_helper(corrs));
  // ... construct the rest of SendMessageRequest manually
  // After:
  let req = MessageBuilder::new().data(req).metadata_json(corrs).build();
  ```

### Fixed — `handle_http_event_value` emits the matching response shape

`LambdaA2aHandler::handle_http_event_value` previously hardcoded the
response envelope as `ApiGatewayV2httpResponse`. Adopters whose
Lambda sat behind a REST API Gateway (`AWS::ApiGateway::RestApi` with
`AWS_PROXY` integration) received a v2 response shape for a v1
invocation, which API Gateway rejected with `502 Bad Gateway` —
agent-card discovery, `/message:send`, and every other route returned
502 against REST v1 deployments.

The method now detects the inbound shape via
`lambda_http::request::LambdaRequest` and emits the matching
envelope:

| Inbound shape                                       | Response envelope             |
|-----------------------------------------------------|-------------------------------|
| API Gateway v1 (REST) — top-level `httpMethod`/`path` | `ApiGatewayProxyResponse`     |
| API Gateway v2 / Function URL — `requestContext.http` | `ApiGatewayV2httpResponse`    |
| ALB target group — `requestContext.elb`             | `AlbTargetGroupResponse`      |
| WebSocket / unrecognised                             | `Err(...)`                    |

Five new regression tests in
`crates/turul-a2a-aws-lambda/src/lib.rs::handle_http_event_value_shape_tests`
pin the contract: REST v1 in / REST v1 out (the live-fleet 502
regression), v2 in / v2 out, ALB in / ALB out, the wire-distinguishability
of v1 vs v2 envelopes, and the realistic stage-prefixed `/dev/...`
path combined with `strip_path_prefix("/dev")` on the handler — all
emitting v1-shaped responses.

This also corrects the same bug for `LambdaA2aHandler::run_http_and_sqs`,
which dispatches HTTP events through `handle_http_event_value`
internally, and for any adopter composition that calls
`handler.handle_http_event_value(value)` directly. `run_http_only`
was unaffected because it dispatches through `lambda_http::run`,
which has shape-aware response handling of its own.

### Added — HTTP envelope primitive + classifier exposed for composition

Adopters whose single Lambda receives HTTP plus one non-framework
trigger (EventBridge scheduler, DynamoDB stream, custom invoke)
previously had to re-implement the `LambdaRequest` /
`ApiGatewayV2httpResponse` envelope conversion themselves. The
framework now exposes the same primitive it already used internally.

- **`LambdaA2aHandler::handle_http_event_value(value: serde_json::Value) -> Result<serde_json::Value, lambda_runtime::Error>`** — converts a raw Lambda HTTP event `Value` (API Gateway v1/v2 / Function URL / ALB shapes) into a `lambda_http::Request`, dispatches through `Self::handle` (so `strip_path_prefix` is applied), and rebuilds an `ApiGatewayV2httpResponse` `Value`. Text-vs-base64 content-type detection matches the internal dual-event runner. **Not behind the `sqs` feature** — HTTP + EventBridge does not imply SQS.
- **`classify_event` and `LambdaEvent` no longer require the `sqs` feature.** Moved out of `durable.rs` (sqs-gated) into a new `event.rs`. `LambdaEvent::Sqs` is still a variant; the SQS-specific handlers (`handle_sqs`, `run_sqs_only`, `run_http_and_sqs`) remain gated.
- **Removed:** `turul-a2a-aws-lambda::drive_sqs_batch` re-export was lost in the refactor shuffle — restored. `LambdaEvent` and `classify_event` are re-exported from the non-sqs path.
- Docstring for `classify_event` now shows the adopter composition pattern with `handler.handle_http_event_value(value).await` on the HTTP branch (previously said "handled by lambda_http layer" which was wrong).
- 2 unit tests carried over and renamed (`handle_http_event_value_agent_card_returns_text_json`, `handle_http_event_value_rejects_unknown_shape`); 6 new unit tests on `classify_event` in `event.rs` covering API Gateway v1, v2, SQS, EventBridge scheduled, DynamoDB stream, and custom payloads.

### Added — `LambdaA2aServerBuilder::strip_path_prefix`

- **`strip_path_prefix(impl Into<String>)`** on `LambdaA2aServerBuilder`. Rewrites the incoming HTTP request URI — drops a leading path segment before the A2A router sees the request. Needed when Lambda sits behind a REST API Gateway with `AWS_PROXY` integration, which forwards the full stage + resource tree in the path (e.g. a request to `/stage/agent/message:send` hitting a router that expects `/message:send`). The prefix must start with `/` and must not end with `/`; invalid shapes are rejected at `build()` with an error that names the method. Applies to `run_http_only` and `run_http_and_sqs`; SQS events unaffected. Non-matching paths pass through unchanged (router 404s on genuinely unknown paths — the correct failure mode). Segment-boundary aware: `/dev` will not strip `/devs/...`. 13 new tests across the adapter unit tests and the builder integration tests, covering the prefix variants named in A2A spec (`/.well-known/agent-card.json`, `/message:send`, `/tasks/{id}`, `/jsonrpc`, `/extendedAgentCard`).

### Added — Lambda runners hide envelope dispatch from adopters

Adopter `main.rs` no longer reaches for `lambda_http::run`,
`lambda_runtime::run`, `service_fn`, `SqsEvent`, or any AWS
event-envelope type. Pick the runner whose name matches the Lambda's
AWS triggers and `.await?` — the adapter owns the dispatch.

- **`LambdaA2aHandler::run_http_only`** — strict HTTP Lambda (Function URL / APIGW / ALB). Any non-HTTP event errors.
- **`LambdaA2aHandler::run_sqs_only`** *(gated on `sqs`)* — strict SQS consumer. Any non-SQS event errors. Pure consumer Lambdas do **not** need `LambdaA2aServerBuilder::with_sqs_return_immediately(...)` — that wires *enqueueing*, which consumers never do.
- **`LambdaA2aHandler::run_http_and_sqs`** *(gated on `sqs`)* — dual-event Lambda. Classifies each event and routes HTTP → `handle()`, SQS → `handle_sqs()`, anything else → error. Replaces the previous `run_mixed_http_and_sqs` free function (removed, see below).
- **`LambdaA2aHandler::run`** — default entry point. Without `sqs` it aliases `run_http_only`; with `sqs` it aliases `run_http_and_sqs` (handles either shape). Use the explicit runners when you want strict fail-fast on a mis-configured trigger.
- **`LambdaA2aServerBuilder::run`** — fluent shortcut for `self.build()?.run().await`. Builder ends with `.run().await` in `main.rs`; no handler variable needed for the common case.
- Crate-level **"Choosing a runner"** docs in `turul-a2a-aws-lambda/src/lib.rs` with the topology-to-runner matrix.

### Changed (breaking) — runner surface is handler methods, not free functions

- **Removed: `turul-a2a-aws-lambda::run_mixed_http_and_sqs`** (free function, gated on `sqs`). Replaced by `LambdaA2aHandler::run_http_and_sqs` with the same dispatch semantics. Migration: `run_mixed_http_and_sqs(handler).await` → `handler.run_http_and_sqs().await` (or `handler.run().await` for the default).

Unpublished pre-release change — `run_mixed_http_and_sqs` shipped earlier
in this same 0.1.15 commit stack; no `cargo publish` happened between
the introduction and the rename, so no external adopter can have
depended on the free-function shape.

- **`turul-a2a-client::MessageBuilder` configuration-shape methods** — the builder now reaches the `SendMessageRequest.configuration` fields that were previously only accessible by dropping to raw proto literals:
  - `tenant(impl Into<String>)` — request-root tenant.
  - `return_immediately(bool)` — configuration flag.
  - `accepted_output_modes(impl IntoIterator<Item = impl Into<String>>)` — configuration list.
  - `history_length(i32)` / `clear_history_length()` — preserves proto tri-state (None / 0 / n).
  - `inline_push_config(url, token)` — 80%-case inline `TaskPushNotificationConfig`; leaves `task_id` empty for server assignment during atomic registration.
  - `push_config(pb::TaskPushNotificationConfig)` — raw-struct passthrough for adopters who need `authentication` or a pre-minted id.
  - `build()` emits `Some(SendMessageConfiguration)` only when any field is non-default, so callers not touching these methods see no wire change.

- **`turul-a2a-types::Message` wrapper accessors** — `context_id()`, `task_id()`, `metadata()`, `metadata_keys()`. Covers the repeated `msg.as_proto().context_id` / `msg.as_proto().metadata.as_ref().map(...)` patterns without forcing adopters into `as_proto()`. `metadata_keys()` returns a sorted `Vec<String>` of the top-level metadata keys (values intentionally not exposed — adopters who want values reach for `metadata()`).

### Tests

- `crates/turul-a2a/tests/adr018_tests.rs::durable_path_preserves_text_task_id_and_context_id_across_enqueue_dequeue` — automated payload-survival regression. Captures the `QueuedExecutorJob` enqueued by `core_send_message` with `return_immediately=true`, drives `run_queued_executor_job` directly, and asserts the terminal artifact contains the probe text + task id + context id + metadata keys — with a negative assertion that metadata values never leak into the artifact.
- 2 new unit tests for the HTTP envelope helper in `crates/turul-a2a-aws-lambda/src/durable.rs` (agent-card round-trip + unknown-shape rejection).

### Added — earlier on 0.1.14 (examples, not in the release contract)

- `examples/lambda-durable-agent` + `examples/lambda-durable-worker` — ADR-018 production-shape demo (two Lambdas + shared DynamoDB + `examples/lambda-infra/cloudformation.yaml`).
- `examples/lambda-durable-single` — simplest ADR-018 demo (single Lambda, `ReservedConcurrency=1`, in-memory).
- Both verified end-to-end on AWS `ap-southeast-2`; zero ERROR logs on the happy path.

## [0.1.14] — 2026-04-24

### Added — ADR-018 Phase 1 + Phase 2

- **`turul-a2a::durable_executor` module.** `DurableExecutorQueue` trait + `QueuedExecutorJob` payload + `QueueError`. Public extension point for adopters who want framework-managed durable continuation on runtimes that cannot honour `return_immediately=true` with `tokio::spawn` (AWS Lambda).
- **`turul-a2a-aws-lambda` gains `sqs` feature.** Behind the `sqs` feature: `SqsDurableExecutorQueue` (first-party impl, `max_payload_bytes = 256 * 1024`), `LambdaA2aServerBuilder::with_durable_executor(queue)` / `with_sqs_return_immediately(queue_url, client)`, `LambdaA2aHandler::handle_sqs(event)`, `LambdaEvent` + `classify_event` for dual-event routing. `aws-sdk-sqs` is an optional dep behind the `sqs` feature.
- When a durable executor queue is wired, `LambdaA2aServerBuilder::build()` re-enables `RuntimeConfig.supports_return_immediately`. The capability-taking builder shape enforces the "capability, not intent" contract from ADR-017 / ADR-018 — no public boolean setter ships.
- SQS dequeue path commits `TASK_STATE_CANCELED` directly via `atomic_store.update_task_status_with_events` when the ADR-012 cancel marker is set pre-dispatch. The executor is never invoked for queued-then-cancelled tasks, so no side effects leak. `TerminalStateAlreadySet` on a race with a concurrent terminal commit is treated as success.

### Changed — ADR-018 Phase 1

- **`core_send_message` signature change (additive).** Takes a new `claims: Option<serde_json::Value>` parameter before `body`. HTTP and JSON-RPC call sites pass `ctx.identity.claims().cloned()`; gRPC passes `None` until a future ADR wires gRPC auth. `core_send_message` is `#[doc(hidden)]` so this is not a public-API break.
- **Claims now reach executors on the blocking-send path.** `SpawnScope` gains a `claims` field; `ExecutionContext.claims` was previously hardcoded to `None` in `server/spawn.rs` — a pre-existing bug from before ADR-018 where executors silently observed `None` even when the HTTP request presented a valid JWT. Adopter executors that branched on `ctx.claims.is_none()` will observe the fix as an additive behaviour delta.
- **`run_executor_body` extracted as `server::spawn::run_executor_for_existing_task`** (and the SQS consumer entry `run_queued_executor_job`). `spawn_tracked_executor` unchanged externally.
- **Pending-dispatch marker skipped when no push configs exist.** On the atomic-store terminal commit path (all four backends: in-memory, SQLite, Postgres, DynamoDB), the framework now pre-checks whether any push config is registered for the task before writing a `a2a_push_pending_dispatches` marker. Downstream impact: deployments that never register webhooks no longer accumulate marker rows that the recovery workers would scan-and-discard. Configs registered after terminal are not eligible for that terminal event per ADR-009 `registered_after_event_sequence`, so the pre-check is correctness-neutral.

### Documentation

- [ADR-018](https://github.com/aussierobots/turul-a2a/blob/main/docs/adr/ADR-018-lambda-durable-executor-sqs.md) — Lambda durable executor continuation via SQS. Status: Accepted.

### Tests

- 6 new tests in `crates/turul-a2a/tests/adr018_tests.rs` covering envelope round-trip with claims, oversize preflight, successful enqueue leaves `Working` without local executor spawn, enqueue-failure FAILED compensation, blocking-send claims-reach-executor, pending-dispatch-skipped-without-configs.
- 9 new tests in `crates/turul-a2a-aws-lambda/tests/sqs_durable_tests.rs` (gated on `sqs`) covering event classification, terminal idempotent no-op, cancel-marker direct-CANCELED commit without executor, non-terminal executor-runs path, unknown envelope version as batch-item failure, SQS `max_payload_bytes` contract.
- Existing `test_atomic_marker_written_for_terminal_status` parity test updated to register a push config before the terminal commit (otherwise the task-45 fix would skip the marker write).

## [0.1.13] — 2026-04-24

### Changed — `SendMessageConfiguration` honoured on all three transports (ADR-017)

`core_send_message` (shared by HTTP `POST /message:send`, JSON-RPC
`SendMessage`, and gRPC `SendMessage`) now honours three
`SendMessageConfiguration` fields that were previously silently
ignored or silently broken. One of the changes is a breaking behaviour
change on AWS Lambda; the others are additive fixes on all runtimes.

- **Lambda: `return_immediately = true` now returns
  `UnsupportedOperationError`** (HTTP 400 / JSON-RPC -32004 / gRPC
  `INVALID_ARGUMENT`). Previously the flag was silently accepted but
  the background executor had no guarantee of completing in the Lambda
  execution environment (ADR-008, ADR-013 §4.4), leaving tasks stuck
  in `Working` indefinitely with no push callback. If your client sets
  `returnImmediately: true` on Lambda, remove the flag or submit with
  `returnImmediately: false` (blocking send, bounded by the API
  Gateway timeout). Adopters needing fire-and-forget-style work on
  Lambda should invoke their durable mechanism (Step Functions, SQS,
  EventBridge, etc.) from inside their `AgentExecutor::execute` body;
  the A2A task is then Completed for "workflow accepted / started",
  not "workflow finished". See ADR-017 §"Alternatives considered"
  Pattern A. No action required for non-Lambda deployments — the
  existing `return_immediately=true` semantics are preserved.

- **`SendMessageConfiguration.taskPushNotificationConfig` inline push
  configs are now registered atomically with task creation on all
  runtimes.** URL parses up front — malformed URL returns HTTP 400 /
  JSON-RPC -32602 / gRPC `INVALID_ARGUMENT` with zero task persisted.
  If push-storage registration fails after the task was created, the
  task is transitioned to `TASK_STATE_FAILED` with an agent `Message`
  whose `Part::text` reads `inline push notification config
  registration failed: <error>`, and the error is returned to the
  caller; the executor is never spawned. Callers that previously
  worked around the silent drop with a separate
  `CreateTaskPushNotificationConfig` round-trip may remove that
  workaround.

- **`SendMessageConfiguration.historyLength` now trims response
  message history on both `return_immediately=true` and blocking send
  responses, matching `GetTask`.** The three-valued proto semantic is
  preserved exactly (`proto/a2a.proto:150-154`): **unset** means no
  limit (full history returned), **`0`** means empty history, **`n > 0`**
  means the last `n` messages. Callers that relied on receiving the
  full history regardless of the flag must omit `historyLength` —
  setting it to `0` now correctly clears the response history per the
  proto contract.

### Added

- `RuntimeConfig.supports_return_immediately: bool` (default `true`) —
  capability flag consumed by `core_send_message`. The AWS Lambda
  adapter sets this to `false` in `LambdaA2aServerBuilder::build()`.
  The field is `pub` on a `#[non_exhaustive]` struct as an escape
  hatch for tests and advanced internal consumers; it is **not** an
  adopter surface (ADR-017 §"Alternatives considered").

### Test matrix

- 12 new tests in `crates/turul-a2a/tests/adr017_tests.rs` cover
  Lambda gate (HTTP + JSON-RPC), inline push config (HTTP + JSON-RPC
  + invalid URL + missing URL + storage-failure compensation), and
  `historyLength` tri-state (`unset`, `0`, `n=1` on return-immediately
  path, `n=1` on blocking path).

### Documentation

- ADR-017 added under `docs/adr/`.

## [0.1.12] — 2026-04-24

### Changed — wire surface tightening (ADR-016)

Transport auth-failure wire surface is now stable and deliberately
narrower than prior releases. **No public A2A-protocol API changes**;
the changes below affect only the 401/403 responses produced by
`turul-a2a-auth` middleware and by any adopter-supplied middleware
that constructs `MiddlewareError` directly.

- **401/403 JSON body** changed from
  `{"error": {"code": <status>, "message": "<Debug of internal enum>"}}`
  to `{"error": "<kind_string>"}` where `<kind_string>` is one of
  `missing_credential`, `invalid_token`, `invalid_api_key`,
  `empty_principal`, `insufficient_scope`. The previous body text was
  `format!("{err:?}")` of an internal enum — not a stable contract.
  Adopters pattern-matching on the old `message` text need to read the
  `error` string instead.
- **Bearer `WWW-Authenticate`** changed from
  `Bearer realm="a2a", error="invalid_token", error_description="<validator internals>"`
  to `Bearer realm="a2a", error="<rfc6750_code>"`. The `error=` code
  derives from `AuthFailureKind` per RFC 6750 §3 and can only be
  `invalid_request`, `invalid_token`, or `insufficient_scope`.
  `error_description` is omitted intentionally — it previously leaked
  validator internals (JWKS URLs, jsonwebtoken errors, token
  fragments).
- **API-key failures** never emit `WWW-Authenticate` (no canonical
  non-Bearer challenge vocabulary).
- **`RequestContext::Debug`** no longer derives — manual impl redacts
  `bearer_token`, redacts every `headers` value that isn't in a fixed
  safe allowlist (`Content-Type`, `Content-Length`, `Accept`,
  `User-Agent`, `Host`), and prints `extensions` keys only. Header
  names remain visible. Any adopter using `{:?}` to debug auth state
  sees less — read `ctx.bearer_token`, `ctx.identity.owner()`, etc.
  explicitly.
- **`AuthIdentity::Debug`** no longer derives — `Authenticated` variant
  shows `owner` but redacts `claims`.

### Changed — error type shape

- **`MiddlewareError`** variants changed to carry `AuthFailureKind`
  instead of ad-hoc `String` / status + www_authenticate fields:
  - `Unauthenticated(String)` → `Unauthenticated(AuthFailureKind)`
  - `HttpChallenge { status, www_authenticate }` → `HttpChallenge(AuthFailureKind)`
  - `Forbidden(String)` → `Forbidden(AuthFailureKind)`
  - `Internal(String)` unchanged.
- Adopter middleware constructing these variants directly needs
  trivial translation. `matches!(err, MiddlewareError::Unauthenticated(_))`
  patterns keep working.
- `AnyOfMiddleware` no longer concatenates child `WWW-Authenticate`
  values; the highest-precedence selected error's kind drives the
  single emitted header at the transport layer.

### Added

- **`turul_a2a::middleware::AuthFailureKind`** — new public enum,
  `#[non_exhaustive]`. Methods: `body_string()` → stable wire string;
  `bearer_rfc6750_code()` → optional RFC 6750 `error=` code.
- **`turul_a2a::middleware::MiddlewareError::kind()`** → `Option<AuthFailureKind>`.
- **`turul_a2a_auth::RedactedApiKeyLookup`** — first-party `ApiKeyLookup`
  reference implementation with a redacted `Debug` impl that never
  emits key material. Adopters can use it directly or treat it as a
  template for backend-specific lookups.
- **Type-level guard**: compile-fail (via `static_assertions`) that
  `ApiKeyMiddleware`, `BearerMiddleware`, and `StaticApiKeyLookup` do
  not implement `Debug`. Guards against accidental
  `#[derive(Debug)]` leaking credential material.
- **New test suite** `crates/turul-a2a/tests/auth_wire_tests.rs` —
  six E2E assertions on body + header shape for every
  `AuthFailureKind` / variant combination (ADR-016 §4).
- Unit tests covering `AuthFailureKind` mapping, `RequestContext::Debug`
  redaction (bearer token, sensitive headers, allowlist passthrough,
  extensions), and `AuthIdentity::Debug` redaction.

### Migration

For most adopters: no code changes needed. `turul-a2a-auth`'s
`BearerMiddleware` and `ApiKeyMiddleware` continue to work and now
emit the cleaner wire shape automatically.

For adopters who wrote their own `A2aMiddleware` impls:
- Replace `MiddlewareError::Unauthenticated("message")` with
  `MiddlewareError::Unauthenticated(AuthFailureKind::<appropriate>)`.
- Replace `MiddlewareError::HttpChallenge { status, www_authenticate }`
  with `MiddlewareError::HttpChallenge(AuthFailureKind::<appropriate>)`.
- Replace `MiddlewareError::Forbidden("message")` with
  `MiddlewareError::Forbidden(AuthFailureKind::InsufficientScope)`
  (or whatever kind fits).
- `MiddlewareError::Internal(String)` is unchanged.

For adopters parsing 401/403 response bodies: switch from reading
`body.error.code` / `body.error.message` to `body.error` (string).

## [0.1.11] — 2026-04-23

### Changed
- Dependency on `turul-jwt-validator` bumped from `"0.1"` → `"0.2"`.
  The `0.2.0` release of `turul-jwt-validator` is the first version
  published exclusively from its own standalone repository
  ([aussierobots/turul-jwt-validator](https://github.com/aussierobots/turul-jwt-validator));
  the `0.1.x` line on crates.io had originated from the pre-extraction
  embedded copy in this workspace. Runtime behaviour is unchanged —
  the `0.2.0` source is API-identical to `0.1.9`.
- No public API changes in any `turul-a2a*` crate.

## [0.1.10] — 2026-04-23

### Changed
- **`turul-jwt-validator` extracted to its own repository and consumed
  as an external dependency.** The crate now lives at
  [`aussierobots/turul-jwt-validator`](https://github.com/aussierobots/turul-jwt-validator)
  and is pulled from crates.io as `turul-jwt-validator = "0.1"`. Source
  is API-identical to the 0.1.9 embedded copy; the split is structural,
  not behavioural, and enables other Rust projects (including
  `turul-mcp-framework`) to consume the same validator without cloning
  this workspace. No public API changes for `turul-a2a-auth` consumers.
- **Workspace dependency normalization.** All intra-workspace deps
  (`turul-a2a`, `turul-a2a-proto`, `turul-a2a-types`) plus the now-external
  `turul-jwt-validator` are declared once in
  `[workspace.dependencies]` and referenced via `{ workspace = true }`
  in every consuming crate, matching the workspace's existing house rule.

### Removed
- `crates/turul-jwt-validator/` and its workspace-members entry.

No breaking changes — the public API of `turul-a2a-auth` is unchanged,
and `turul-jwt-validator 0.1.9` on crates.io is the same source that
shipped with `turul-a2a 0.1.9`.

## [0.1.9] — 2026-04-22

### Added
- **Skill-level `security_requirements` — declaration-only (ADR-015 — Accepted).**
  Adopters can now publish per-skill security metadata on the
  `AgentCard` via `AgentSkillBuilder::security_requirements(Vec<SecurityRequirement>)`
  and adopter-supplied agent-level metadata via
  `AgentCardBuilder::security_scheme(name, scheme)` /
  `::security_requirement(requirement)`. Post-merge truthfulness
  validation at `A2aServerBuilder::build()` rejects cards whose public
  or no-claims extended card references a scheme not present in the
  merged `security_schemes` map, with per-surface error messages
  (`agent_card` vs. `extended_agent_card`). Runtime skill-level
  enforcement remains deferred — advertising a requirement does NOT
  install a middleware. See `docs/adr/ADR-015-skill-level-security-requirements.md`
  for the full rationale and the normative constraint blocking
  enforcement (no `skill_id` binding on `Message`).
- **New example: `examples/skill-security-agent`.** Two skills on
  one agent, one advertising a bearer requirement, no auth middleware
  installed — demonstrates the declaration-vs-enforcement distinction.

### Internal
- `SecurityAugmentedExecutor::{agent_card, extended_agent_card}`
  refactored to share a single `apply_security_merge` helper with the
  build-time validator so served and validated cards are structurally
  identical.

No breaking changes. Additive API surface; cards that did not set
skill-level or adopter-supplied agent-level security metadata in 0.1.8
continue to build and serve byte-identical output.

## [0.1.8] — 2026-04-22

### Changed
- **`turul-a2a-aws-lambda`: `aws_lambda_events` 0.15 → 1.1** (resolves
  to 1.1.3). 1.0 added `#[non_exhaustive]` to the DynamoDB stream
  types (`Event`, `EventRecord`, `StreamRecord`,
  `DynamoDbEventResponse`, `DynamoDbBatchItemFailure`), so struct
  expressions on those types are no longer legal from downstream
  crates. Internal call sites in `stream_recovery.rs` and its test
  helpers now build through `Default::default()` followed by field
  assignment; two shared helpers (`build_event_record`,
  `event_with_records`) keep the pattern in one place.

  **Breaking for adopters of `turul-a2a-aws-lambda`.** If your
  Lambda handler code passes `aws_lambda_events::dynamodb::Event`
  values to `LambdaStreamRecoveryHandler::handle_stream_event`,
  you must bump your own `aws_lambda_events` dep to `^1.1` — the
  two major versions produce incompatible types.

  Workspace, `examples/lambda-*` crates, and every other publish
  crate are unchanged by this release — only `turul-a2a-aws-lambda`
  has behavioural / ABI impact. Versions bumped across all seven
  publish crates to keep workspace-versioned semantics intact.

## [0.1.7] — 2026-04-22

### Added
- **gRPC transport (ADR-014 — Accepted).** Third thin adapter over
  the shared core handlers (ADR-005 extended). Default builds remain
  HTTP+JSON-only and tonic-free; enabling the new `grpc` feature on
  `turul-a2a-proto`, `turul-a2a`, and `turul-a2a-client` activates a
  tonic 0.14 server and client covering every one of the 11
  `lf.a2a.v1.A2AService` RPCs (9 unary + 2 server-streaming).
  Highlights:
    - `A2aServer::into_tonic_router()` is the single public entry
      point to the gRPC surface; it always composes the same Tower
      auth stack (`A2aMiddleware` / `MiddlewareStack`) as the HTTP
      path, so adopters cannot silently bypass auth. No raw service
      accessor is exposed in 0.1.7.
    - Streaming consumes the ADR-009 durable event store and broker
      (no parallel pipeline). Resume via `a2a-last-event-id` ASCII
      metadata (`{task_id}:{sequence}` — mirrors the SSE
      `Last-Event-ID` convention).
    - `SubscribeToTask` emits the `Task` snapshot as the first event
      on fresh attach (spec §3.1.6). `SendStreamingMessage` does NOT
      emit a synthetic `Task` first — matching the established HTTP
      SSE behaviour; the stream starts at the first persisted
      `StatusUpdate(SUBMITTED)` event.
    - Terminal `SubscribeToTask` surfaces `A2aError::UnsupportedOperation`,
      mapped to `FAILED_PRECONDITION` with
      `ErrorInfo { reason = "UNSUPPORTED_OPERATION", domain = "a2a-protocol.org" }`.
      `PushNotificationNotSupported` and
      `ExtendedAgentCardNotConfigured` map to `UNIMPLEMENTED`
      (capability-absence vs. state-rejection — see ADR-014 §2.5).
    - Tenant precedence (normative, ADR-014 §2.4): the proto
      `tenant` field wins over the `x-tenant-id` metadata when both
      are present. Metadata is a fallback for clients that cannot
      populate the proto field.
    - `last_chunk` is persisted as part of `ArtifactUpdatePayload` in
      the event store (ADR-014 §2.3 two-layer contract) — the gRPC
      adapter reads it back from stored events, never invents it.
- **`grpc-reflection` and `grpc-health` sub-features** on
  `turul-a2a`. Off by default; enable alongside `grpc` to serve
  `grpc.reflection.v1alpha.ServerReflection` or
  `grpc.health.v1.Health`.
- **`turul_a2a_client::grpc::A2aGrpcClient`** (feature-gated): thin
  ergonomic wrapper over the tonic-generated client with the same
  verb surface as `A2aClient` (send/get/list/cancel/stream/subscribe
  + push config CRUD). Exposes `A2aClientError::Grpc` +
  `A2aClientError::grpc_code()` for tonic status inspection.
- **Example: `examples/grpc-agent`** — a two-binary crate
  (`grpc-agent` server + `grpc-client` CLI) demonstrating the full
  flow end-to-end, including `send`, `stream` (chunked artifacts
  with `last_chunk`), `list`, and `get`.
- **Three-transport spec-compliance axis.** `spec_compliance.rs`
  gains a `grpc_parity` module under `--features grpc` proving that
  HTTP+JSON, JSON-RPC, and gRPC emit identical `ErrorInfo` reasons
  and share storage state across transports. 11 new end-to-end
  gRPC integration tests in `crates/turul-a2a/tests/grpc_tests.rs`.

### Notes
- **Not available under `turul-a2a-aws-lambda`.** Lambda lacks
  persistent HTTP/2 (ADR-014 §2.6). Host the gRPC transport on
  ECS / Fargate / AppRunner / Kubernetes / self-managed VMs.
- **TLS is deployer-owned.** A2A v1.0 §7.1 requires TLS 1.2+ in
  production; this framework defers termination to the reverse
  proxy / service mesh (same posture as HTTP).

## [0.1.6] — 2026-04-21

### Fixed
- **DynamoDB cold-start read-after-write race.** A request Lambda on a
  fresh container could write a task via `create_task_with_events`
  and then immediately fail its own follow-up
  `update_task_status_with_events` (the Submitted → Working
  transition) with `TaskNotFound`, because the atomic store's
  internal pre-transaction read used DynamoDB's default
  eventually-consistent `GetItem`. The handler surfaced the error as
  HTTP 404 to the caller even though the task row existed, and the
  executor never ran. Classic symptom: a task stuck in `Submitted`
  state with a ~240 ms Lambda invocation and a 404 response body on
  the first call to a just-warmed container. Fix flips
  `ConsistentRead=true` on three read paths in
  `crates/turul-a2a/src/storage/dynamodb.rs`: `get_task`'s
  `GetItem`, `query_latest_event_sequence`'s `GetItem` (feeds the
  ADR-013 §6.3 monotonic sequence invariant), and
  `query_max_sequence`'s `Query` (picks the next event seq for the
  transact write). Cost: 2× RCU on these specific reads; the right
  trade for A2A's state-machine contract. No changes to other
  backends — SQLite, PostgreSQL and in-memory already satisfy
  read-your-writes by construction.

### Added
- **Read-your-writes requirement is now normative in the storage trait
  docs.** `A2aTaskStorage` grew a top-level "Read-your-writes
  requirement" section spelling out that a read issued after a
  successful write on the same instance MUST observe that write, the
  concrete failure mode when the contract is violated, and which
  backends satisfy it natively vs. which (DynamoDB) require an
  explicit `ConsistentRead=true`. `A2aAtomicStore` grew a companion
  "Read-your-writes across traits" clause making the cross-trait
  visibility guarantee explicit. Future backend authors have a
  durable anchor to point at.
- **Parity test `test_read_your_writes_across_traits`** walks the
  exact four-step send-message sequence
  (`create_task_with_events` → `get_task` →
  `update_task_status_with_events` → `get_task`) and asserts
  visibility at each step. Wired into all four backend test modules
  (in-memory, SQLite, PostgreSQL, DynamoDB). Against DynamoDB-Local
  (synchronous, in-memory) the test cannot empirically reproduce the
  cold-start race, but it documents the contract as executable code
  and will catch any future regression (e.g. an unintended read cache
  in front of `get_task`).
- **Example `README.md` files** for all five examples
  (`echo-agent`, `auth-agent`, `lambda-agent`, `lambda-stream-worker`,
  `lambda-scheduled-worker`) covering purpose, run/deploy steps, and
  how the three Lambda examples compose into an ADR-013 deployment.
  GitHub now renders a README on each example directory.

### Internal
- **Pre-existing clippy cleanups** surfaced by the publish gate:
  `rows.sort_by(|a, b| b.cmp(&a))` in `storage/dynamodb.rs` switched
  to `sort_by_key(|r| Reverse(r))`; unused symmetric encoder
  `claim_status_to_str` in `storage/sqlite.rs` marked
  `#[allow(dead_code)]` with a comment explaining parity with the
  PostgreSQL and DynamoDB analogs.

## [0.1.5] — 2026-04-20

### Fixed
- **`.storage()` no longer auto-wires push delivery (ADR-013 §4.3
  errata).** 0.1.4 required the backend passed to `.storage()` to
  implement `A2aPushDeliveryStore` and automatically assigned it as
  the push delivery store. Because all four first-party backends
  implement every storage trait on the same struct (ADR-009
  same-backend requirement), **any non-push adopter calling
  `.storage(storage)` was silently enrolled into push delivery** and
  forced to call `with_push_dispatch_enabled(true)` to pass the
  §4.3 consistency check — even when they never registered a push
  config.

  This was a conflation of "implements the trait" with "is wired for
  use". Fixed in both `turul_a2a::server::A2aServerBuilder::storage`
  and `turul_a2a_aws_lambda::LambdaA2aServerBuilder::storage`: the
  `A2aPushDeliveryStore` bound is removed, and no auto-assignment
  occurs. Push delivery is now strict opt-in via the existing
  `.push_delivery_store(storage.clone())` setter, which must be
  paired with `with_push_dispatch_enabled(true)` on the storage
  instance (unchanged consistency check).

  **Migration:**
  - Non-push deployments: no changes required. `.storage(storage)`
    works unchanged.
  - Push-using deployments: add `.push_delivery_store(storage.clone())`
    explicitly before `.build()`. This was already the documented
    path; it is now the only path.

## [0.1.4] — 2026-04-20

### Added
- **ADR-013: Lambda push-delivery parity.** Push fan-out now survives the
  request-Lambda returning before deliveries complete. A durable
  `a2a_push_pending_dispatches` marker is written atomically with the
  terminal task/event commit (opt-in via
  `InMemoryA2aStorage::with_push_dispatch_enabled(true)` and per-backend
  equivalents). Two recovery workers in `turul-a2a-aws-lambda`:
  - `LambdaStreamRecoveryHandler` — DynamoDB Streams trigger with
    `BatchItemFailures` partial-batch semantics.
  - `LambdaScheduledRecoveryHandler` — EventBridge Scheduler backstop that
    sweeps stale markers + reclaimable claims with bounded batch limits and
    a structured count summary for CloudWatch alerting.
  Two runnable examples: `examples/lambda-stream-worker` and
  `examples/lambda-scheduled-worker`.
- **Causal no-backfill for push configs** (ADR-013 §4.5 / §6). New column
  `a2a_tasks.latest_event_sequence` (+ `latestEventSequence` attribute on
  DynamoDB) is maintained by every atomic commit. `create_config` stamps
  `registered_after_event_sequence` under a CAS against the read sequence;
  `list_configs_eligible_at_event(seq)` enforces a strict-less-than
  eligibility filter so a config registered concurrently with the terminal
  commit is never retroactively delivered.
- **Server and Lambda builder consistency gates** (ADR-013 §4.3): both
  `A2aServer::builder()` and `LambdaA2aServerBuilder` reject any
  configuration where `push_delivery_store` is wired but
  `push_dispatch_enabled` is false (or vice versa), in either direction.
- **Per-crate license files.** Every publish crate now ships
  `LICENSE-APACHE` and `LICENSE-MIT` inline (symlinks in git; cargo
  package copies the real file content). Previously only the SPDX
  identifier was published.

### Changed
- **Release-scoped compliance wording.** `CLAUDE.md` §169 no longer
  asserts "No known spec compliance gaps remaining" as an absolute claim.
  The new wording scopes compliance to the tested A2A v1.0 HTTP+JSON and
  JSON-RPC server surfaces and names out-of-scope items separately:
  gRPC transport (deferred), deployment-layer TLS 1.2+ / gzip
  (reverse-proxy concern), and deeper per-skill security-scheme
  semantics (agent-level only). Re-verification of
  `proto/a2a.proto` SHA256 against
  `a2aproject/A2A:main/specification/a2a.proto` is now an explicit
  pre-release gate.
- **Upstream proto identity verified.** Vendored
  `crates/turul-a2a-proto/proto/a2a.proto` SHA256
  `4b74c0baa923ae0acb55474e548f1d6e5d3f83b80d757b65f8bf3e99a3c2257f` is
  byte-identical to upstream `a2aproject/A2A:main/specification/a2a.proto`
  as of the 0.1.4 release gate.
- **Workspace fmt sweep** under `rustfmt 1.9.0-stable` (2026-04-14). No
  semantic changes; purely cosmetic line-joining.

### Fixed
- **Package hygiene — tokio dev-dep dedup.** Per-crate dev-dependencies no
  longer redeclare `"full"` on top of the workspace-level base; the
  normalized `Cargo.toml` in every published `.crate` now ships
  `features = ["full", "test-util"]` (previously
  `["full", "full", "test-util"]`). No runtime effect.
- **Cargo-doc workspace build unblocked.** All three Lambda example
  crates declare `[[bin]] name = "bootstrap"` (required by cargo-lambda);
  added `doc = false` to the bin targets so
  `cargo doc --workspace --no-deps` no longer collides on output
  filenames.
- **Three rustdoc broken-link warnings** in `turul-a2a` resolved
  (`EventSinkInner` private-item link, `storage::parity_tests`
  `pub(crate)` link, `PushDispatcher::dispatch` unqualified path).
- **`InMemoryA2aStorage` rustdoc placement** restored after the clippy
  sweep had accidentally re-anchored the backend-level docs to a newly
  extracted type alias.

## [0.1.3] — 2026-04-17

First version published to crates.io.

### Added
- **A2A v0.3 compatibility layer** (opt-in `compat-v03` feature) for `a2a-sdk 0.3.x`
  and Strands Agents SDK clients. Provides JSON-RPC method normalization
  (`message/send` → `SendMessage`), role and task-state enum translation,
  optional `A2A-Version` header, a root `POST /` route, and additive agent card
  fields (`url`, `protocolVersion`, `additionalInterfaces`). Canonical v1.0
  builds remain strict.
- `Message::parse_first_data_or_text` for dual `Data`/`Text` part input parsing
  so wrapper-first executors work with both Turul and `a2a-sdk` clients.
- DynamoDB storage TTL support for tasks, events, and push configs (defaults:
  7 days / 24 hours / 7 days).
- Crate-level `//!` documentation on `turul-a2a` and `turul-a2a-types`.
- Workspace-level `CHANGELOG.md`.

### Changed
- Workspace version bumped from `0.1.2` (pre-publish) to `0.1.3`.
- Proto build enables `ignore_unknown_fields` so v1.0 clients tolerate additive
  v0.3 compat fields in remote agent cards (standard forward-compat behavior).
- All publishable crates now carry `repository`, `homepage`, `readme`,
  `keywords`, `categories`, and `rust-version` via workspace inheritance.
- Internal path-only dependencies (`turul-a2a`, `turul-jwt-validator`) now use
  `version + path` form so they can be published to crates.io.
- `bytes` dependency consolidated to workspace root.
- JWT E2E test switched to `rsa::rand_core::OsRng` to avoid `rand 0.10` /
  `rsa 0.9` version-skew (`rsa` still bridges to `rand_core 0.6`).

## [0.1.2] and earlier — unpublished

Pre-crates.io development. Highlights from the 0.1.x series (not published):

- **ADR-001 – ADR-009** accepted and implemented.
- **D3 durable event coordination** (ADR-009): atomic task + event writes via
  `A2aAtomicStore`; event store is the source of truth and the broker is a
  wake-up signal; terminal replay and `Last-Event-ID` reconnection verified;
  cross-instance streaming proven on shared backends.
- **Four parity-proven storage backends**: in-memory, SQLite, PostgreSQL,
  DynamoDB — all implementing `A2aTaskStorage`, `A2aEventStore`, and
  `A2aAtomicStore`.
- **Auth middleware** (`turul-a2a-auth`): Bearer/JWT via the local
  `turul-jwt-validator` (JWKS caching), and API Key. Transport-layer Tower
  layers with `AuthIdentity` injection (ADR-007).
- **AWS Lambda adapter** (`turul-a2a-aws-lambda`): thin wrapper over the same
  Axum `Router`; request/response only with anti-spoofing for authorizer
  identity (ADR-008).
- **Typed client** (`turul-a2a-client`): wrapper-first request/response types,
  SSE streaming with typed events, push-config CRUD.
- **Dual transport** (ADR-005): HTTP+JSON routes (`/message:send`,
  `/tasks/{id}`, …) and JSON-RPC (`POST /jsonrpc`, all 11 methods) share the
  same core handlers.
- **Spec-aligned error model** (ADR-004): `google.rpc.ErrorInfo` on all A2A
  errors, proto-normative HTTP/JSON-RPC status mapping.
- Proto-first architecture (ADR-001): types generated from normative
  `proto/a2a.proto`, wrapped in ergonomic Rust types with state-machine
  enforcement (ADR-002).

[0.1.6]: https://github.com/aussierobots/turul-a2a/releases/tag/v0.1.6
[0.1.5]: https://github.com/aussierobots/turul-a2a/releases/tag/v0.1.5
[0.1.4]: https://github.com/aussierobots/turul-a2a/releases/tag/v0.1.4
[0.1.3]: https://github.com/aussierobots/turul-a2a/releases/tag/v0.1.3
