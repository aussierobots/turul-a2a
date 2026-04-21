# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Turul-a2a is a Rust implementation of the A2A (Agent-to-Agent) Protocol v1.0. Licensed under MIT OR Apache-2.0.

**Proto-first architecture**: Types are generated from the normative `proto/a2a.proto` (package `lf.a2a.v1`) using `prost` + `pbjson`, then wrapped in ergonomic Rust types.

**Current release**: 0.1.7 — see `CHANGELOG.md` for the per-release contract. §Completed below tracks which ADRs have shipped.

## Build & Development Commands

```bash
cargo build --workspace                    # Build all crates
cargo check --workspace                    # Type-check
cargo test --workspace                     # Run all tests
cargo test --workspace --features sqlite   # Include SQLite parity tests
cargo clippy --workspace -- -D warnings    # Lint (deny warnings)
cargo fmt --all -- --check                 # Format check

# Per-crate tests
cargo test -p turul-a2a-proto              # Proto generation + wire format
cargo test -p turul-a2a-types              # Types, state machine, wrappers
cargo test -p turul-a2a                    # Server, storage, handlers, auth, E2E
cargo test -p turul-a2a-auth               # Auth middleware (API key, Bearer)
cargo test -p turul-a2a-client             # Client library
cargo test -p turul-a2a-aws-lambda         # Lambda adapter
cargo test -p turul-jwt-validator          # JWT validator (E2E with wiremock)

# Storage backend parity tests (feature-gated)
cargo test -p turul-a2a --features sqlite --lib -- storage::sqlite
cargo test -p turul-a2a --features postgres --lib -- storage::postgres --test-threads=1
cargo test -p turul-a2a --features dynamodb --lib -- storage::dynamodb --test-threads=1

# gRPC transport tests (feature-gated, ADR-014)
cargo test -p turul-a2a --features grpc --lib grpc::                # grpc module unit tests (error + service)
cargo test -p turul-a2a --features grpc --test grpc_tests           # gRPC end-to-end (all 11 RPCs + auth)
cargo test -p turul-a2a --features grpc --test spec_compliance      # 3-transport parity axis

# Examples
cargo run -p echo-agent                    # Echo agent on :3000
cargo run -p auth-agent                    # Auth agent on :3001 (requires X-API-Key)
cargo run -p grpc-agent --bin grpc-agent   # gRPC agent example on :3005 (crate opts into turul-a2a/grpc internally)
cargo lambda watch -p lambda-agent         # Lambda agent via cargo-lambda
```

**All crate dependencies MUST use `workspace = true`** — versions are managed in root `Cargo.toml` `[workspace.dependencies]`. This includes dev-dependencies. Never put a version number in a crate's own `Cargo.toml` — add the dependency to the workspace root first, then reference it with `{ workspace = true }` in the crate.

## Git Conventions

- Commit messages: succinct, no Co-Authored-By attribution
- Do not push unless explicitly asked
- Version bumps: minor = 0.0.x (e.g., 0.1.0 → 0.1.1), major = 0.x.0

## Release & Publish (crates.io)

The workspace publishes 7 crates. The workflow is:

1. **Pre-publish gate (always run, every release):**
   - `cargo test --workspace` must be green — including `--features compat-v03` spot check.
   - `cargo package -p <crate> --no-verify --allow-dirty` must be warning-free for every crate.
   - `cargo doc --no-deps` must build without warnings on crates with `//!` docs.

2. **Authorization is per-`cargo publish` call.** Publishing is irreversible (yank only hides; never deletes). Never run `cargo publish` without explicit user instruction in this session — a prior "go ahead" does not authorize future releases.

3. **Publish in dependency order** (hard requirement — later crates won't resolve otherwise):
   1. `turul-a2a-proto`
   2. `turul-a2a-types`
   3. `turul-jwt-validator`
   4. `turul-a2a`
   5. `turul-a2a-client`, `turul-a2a-auth`, `turul-a2a-aws-lambda` (parallelizable — none depend on each other)

4. **crates.io rate limit** is the #1 practical obstacle:
   - New-crate registrations: ~5 per 30-minute window, then one new crate per ~10 minutes.
   - Re-publishes of *existing* crates are effectively unthrottled.
   - On HTTP 429, cargo returns the exact reset time. Use `ScheduleWakeup` (Claude Code) and wait — do not retry sooner. The limit is account-wide across the registry.

5. **Version discipline:**
   - Bump only `[workspace.package].version` in root `Cargo.toml`; every crate inherits via `version.workspace = true`.
   - Intra-workspace deps keep `{ version = "X.Y.Z", path = "..." }` (both fields — crates.io rejects path-only).
   - Workspace-root `[workspace.dependencies]` internal refs (`turul-a2a-proto`, `turul-a2a-types`) also get bumped to the new version.

6. **Tag after successful publish** with annotated tags:
   - `git tag -a vX.Y.Z <release-bump-sha> -m "Release X.Y.Z — <one-liner>"` — **always pass the commit SHA explicitly**. Do not let `git tag` default to `HEAD`; other work may have landed between the publish walk and the tag (v0.1.4 landed on a post-release docs commit for exactly this reason).
   - `git push origin vX.Y.Z`
   - This matches the CHANGELOG release-link convention (`/releases/tag/vX.Y.Z`) and populates GitHub's Releases UI.

7. **Proto file location:** `crates/turul-a2a-proto/proto/` is the canonical location so `cargo package` includes it. A workspace-root `proto/` symlink preserves historical paths in ADRs and docs — do not replace it with a real directory.

8. **Commit the release-prep changes** before running `cargo publish` — cargo will complain about dirty state otherwise. Never use `--allow-dirty` on real publishes, only on `cargo package` dry-runs.

9. **Per-crate LICENSE files:** every publish crate has `LICENSE-APACHE` and `LICENSE-MIT` symlinks to `../../LICENSE-*`. When adding a new publish crate, add both symlinks or the `.crate` archive ships with only the SPDX identifier.

10. **Lambda example `doc = false`:** all three Lambda example crates declare `[[bin]] name = "bootstrap"` (cargo-lambda requirement). Each has `doc = false` on the bin target so `cargo doc --workspace` does not collide on output filenames.

## Architecture

### Crate Structure

- `turul-a2a-proto` — prost-generated types from `a2a.proto`. Build.rs generates via `prost-build` + `pbjson-build`. JSON serialization uses camelCase (proto JSON mapping) via pbjson.
- `turul-a2a-types` — Ergonomic Rust wrappers over proto types. Publishable, no server/storage/auth deps. `#[non_exhaustive]` on all public types. State machine enforcement on `TaskState`.
- `turul-a2a` — Server + storage + HTTP/JSON-RPC/SSE transports + auth middleware foundation. Feature-gated backends (in-memory default). `AgentExecutor` trait, `A2aMiddleware` trait, `A2aServer::builder()`.
- `turul-a2a-auth` — Concrete auth middleware: `BearerMiddleware` (JWT), `ApiKeyMiddleware`. Uses `turul-jwt-validator`.
- `turul-jwt-validator` — Local JWT validator with JWKS caching (design sourced from turul-mcp-oauth).
- `turul-a2a-client` — Independent A2A client: discovery, send, get, cancel, list, auth, tenant.
- `turul-a2a-aws-lambda` — Lambda adapter: thin wrapper over same Router, request/response only (ADR-008).

### Preludes

- `turul_a2a::prelude::*` — server/agent authoring: `A2aServer`, `AgentExecutor`, `ExecutionContext`, `A2aError`, `AgentCardBuilder`, `Task`, `Message`, `Part`, `Artifact`, `TaskState`, `TaskStatus`
- `turul_a2a_client::prelude::*` — client/caller: `A2aClient`, `ClientAuth`, `MessageBuilder`, `SseEvent`, `SseStream`, `A2aClientError`, `ListTasksParams`; under `--features grpc` also re-exports `A2aGrpcClient` and `GrpcStreamResponses` (ADR-014 §2.7)

These are separate by design. Only add types that are genuinely part of the common happy path — do not turn preludes into "export everything."

### Proto Build Pipeline

`proto/a2a.proto` → `prost-build` (generates Rust structs) + `pbjson-build` (generates serde impls with camelCase JSON) → `turul-a2a-proto` crate → wrapped by `turul-a2a-types`

Google well-known types (`google.protobuf.Struct`, `Value`, `Timestamp`) mapped to `pbjson_types` via `compile_well_known_types()` + `extern_path`.

### Known Exceptions

- **Push notification configs** use raw `turul_a2a_proto::TaskPushNotificationConfig` in storage traits and handler signatures. This is an intentional exception — push configs are simple CRUD with no state machine or invariants that warrant a wrapper. Keep this leakage isolated; do not let raw proto types spread into general handler/router code.
- **`last_chunk` on `append_artifact`** is transport metadata for SSE streaming. Storage does not persist completion state; the server layer forwards `last_chunk` to streaming subscribers.

### Server Wiring: Push Delivery Is Strict Opt-In (0.1.5+)

`.storage(storage)` wires storage traits only, even though all four first-party backends implement `A2aPushDeliveryStore` on the same struct (ADR-009 same-backend requirement). Push delivery is opt-in at **both** levels:

- **Storage**: `InMemoryA2aStorage::new().with_push_dispatch_enabled(true)` (and equivalents per backend) so atomic commits write the pending-dispatch marker.
- **Builder**: `.push_delivery_store(storage.clone())` to register the consumer.

The builder rejects inconsistent configurations (ADR-013 §4.3). Non-push adopters need neither call.

### Multi-Instance Streaming Limitation

- The in-process event broker (`TaskEventBroker`) provides local fanout for attached clients on the **same instance only**.
- This is not a Lambda-specific limitation — it affects any multi-instance deployment: Lambda, load-balanced binaries, ECS/Fargate, Kubernetes, rolling deploys.
- Shared task storage (DynamoDB, PostgreSQL) solves request/response correctness across instances but does NOT solve streaming coordination.
- D3 (future): durable event store with monotonic IDs + replay semantics. The in-process broker becomes a local optimization, not the source of truth. See ADR-008.

### Architecture Decision Records

Documented under `docs/adr/`:

- **ADR-001**: Proto-first architecture — prost + pbjson generation with ergonomic wrappers
- **ADR-002**: Wrapper boundary and validation — TryFrom/Deserialize enforcement of REQUIRED fields
- **ADR-003**: Storage trait design — tenant/owner scoping, parity tests, push config exception
- **ADR-004**: Error model — A2A error codes, HTTP/JSON-RPC mapping, google.rpc.ErrorInfo
- **ADR-005**: Dual transport — shared core handlers for HTTP+JSON and JSON-RPC
- **ADR-006**: SSE streaming — in-process broker, last_chunk as transport metadata, single-instance limitation
- **ADR-007**: Auth middleware — transport-level Tower layer, AuthIdentity enum, SecurityContribution, local JWT validator
- **ADR-008**: Lambda adapter — request/response only, streaming deferred to D3, authorizer anti-spoofing
- **ADR-009**: Durable event coordination — same-backend transaction atomicity, tenant-scoped, per-task monotonic sequences
- **ADR-010**: Executor `EventSink` — finer-grained streaming events from inside the executor; proto-only variant surface
- **ADR-011**: Push notification delivery — claim-based fan-out, retry horizon, SSRF, secret redaction
- **ADR-012**: Cancellation propagation — cross-instance cancel marker, supervisor sweep, same-backend requirement
- **ADR-013**: Lambda push-delivery parity — atomic pending-dispatch marker, causal no-backfill CAS, stream + scheduled recovery workers. §4.3 errata (0.1.5): `.storage()` does not auto-wire push delivery; explicit `.push_delivery_store(...)` required.
- **ADR-014**: gRPC transport — third thin adapter via tonic over the shared core handlers (ADR-005 extended). Feature-gated on `turul-a2a-proto/grpc`, `turul-a2a/grpc`, `turul-a2a-client/grpc`; default builds are tonic-free. `grpc-reflection` / `grpc-health` are separate opt-in sub-features. Streaming consumes the ADR-009 durable event store with `a2a-last-event-id` ASCII metadata for replay. Proto `tenant` field wins over `x-tenant-id` metadata (§2.4). Out of scope for `turul-a2a-aws-lambda` (§2.6).

For non-trivial architecture changes, the ADR should be accepted before implementation starts.

### Example and API Surface Policy

- **Examples must prefer wrapper/helper APIs over raw proto mutation.** Repeated `as_proto().clone()` + manual proto construction in examples is a design smell — it signals the wrapper layer is missing a helper that should be designed.
- If a simple example needs generated-proto editing to do normal work (e.g., complete a task, add an artifact), stop and evaluate whether `Task`, `Artifact`, or related types are missing a helper method.
- Raw proto access (`as_proto()`, `as_proto_mut()`) is an escape hatch, not the primary path for common operations.

### Comment and Docstring Style

Do not reference internal task or phase names (e.g. "phase A", "D.2", task numbers, issue identifiers) in code comments, docstrings, or committed artifacts. Those labels are planning scaffolding — they rot once the phase ships and leak implementation history into the public surface. Write comments that describe the invariant, the contract, or the "why" in timeless terms. References to ADRs and to upstream specs (A2A spec sections, proto line numbers) are fine and encouraged — they are durable external anchors. If a constraint is only meaningful relative to an in-flight refactor, put it in the plan or PR description, not in source.

### TDD Discipline

Tests are written from the A2A proto/spec FIRST, then implementation follows. If code disagrees with tests, re-check `proto/a2a.proto` before changing anything. Only change tests when the test is wrong relative to the normative source.

### Wire Format (from proto annotations)

- Send: `POST /message:send` (with `/{tenant}/message:send`)
- Stream: `POST /message:stream`
- Get task: `GET /tasks/{id=*}`
- Cancel: `POST /tasks/{id=*}:cancel`
- Subscribe: `GET /tasks/{id=*}:subscribe`
- Extended card: `GET /extendedAgentCard`
- Discovery: `GET /.well-known/agent-card.json`
- JSON-RPC: `POST /jsonrpc` (all 11 methods, PascalCase)
- TaskState: `TASK_STATE_SUBMITTED`, `TASK_STATE_WORKING`, etc.
- Error: TaskNotFoundError → 404/-32001, TaskNotCancelableError → 409/-32002, UnsupportedOperationError → 400/-32004, ContentTypeNotSupportedError → 415/-32005
- All A2A errors MUST include `google.rpc.ErrorInfo` with reason + domain

### Proto and Spec Compliance

Our vendored `proto/a2a.proto` is byte-identical to upstream `a2aproject/A2A/specification/a2a.proto`. Per spec §1.4, the proto is "the single authoritative normative definition" for data objects and messages.

**HTTP route patterns follow the proto's `google.api.http` annotations** (`/message:send`, `/tasks/{id=*}`, etc.).

**Spec-aligned behaviors:**
- SubscribeToTask: first event is Task object (spec §3.1.6), terminal tasks return UnsupportedOperationError
- Error codes: TaskNotCancelable=409, UnsupportedOperation=400, PushNotificationNotSupported=400
- Query params: camelCase (historyLength, pageSize, pageToken, contextId)
- contextId/taskId mismatch rejection on continuation (spec §3.4.3)

**Release scope.** turul-a2a is tested against the current upstream A2A v1.0 proto (`proto/a2a.proto`) and implements the core HTTP+JSON and JSON-RPC server surfaces covered by that vendored contract. Within those covered surfaces there are no known compliance gaps: ListTasks sorts by `updated_at DESC` across all four backends; pagination cursor encodes `updated_at|task_id` for stable iteration; the spec-compliance suite (`crates/turul-a2a/tests/spec_compliance.rs`) walks every core RPC on both transports.

**Out of scope for the framework release:**
- gRPC transport on AWS Lambda (ADR-014 §2.6: Lambda lacks persistent HTTP/2). The feature itself is fully wired for self-hosted targets (ECS / Fargate / AppRunner / Kubernetes / bare VMs) — see [ADR-014](docs/adr/ADR-014-grpc-transport.md) and `examples/grpc-agent`.
- Deployment-layer transport requirements: TLS 1.2+ (spec §7.1) and gzip (§11.4) are met at the reverse proxy / load balancer, not framework-enforced.
- Deeper per-skill security-scheme semantics (agent-level only; see "Deferred").

Before describing a release as "A2A v1.0 compliant", re-verify the vendored `proto/a2a.proto` SHA256 still matches `a2aproject/A2A:main/specification/a2a.proto` (AGENTS.md §55+§173).

### Completed

- **ADR-009 — Durable event coordination**: store is truth, broker is wake-up signal. Atomic task+event writes via `A2aAtomicStore`. Terminal replay, Last-Event-ID reconnection, cross-instance streaming all verified. Four parity-proven backends.
- **ADR-010 — Executor `EventSink`**: proto-variant-only surface; `emit_*`/`set_status` serialize per-sink.
- **ADR-011 — Push notification delivery**: `PushDispatcher` + `PushDeliveryWorker`, claim-based fan-out, bounded retry horizon (`push_claim_expiry > max_attempts * backoff_cap`), SSRF allowlist, secret redaction.
- **ADR-012 — Cancellation propagation**: cross-instance cancel marker + supervisor sweep; same-backend check on the builder.
- **ADR-013 — Lambda push-delivery parity**: atomic `a2a_push_pending_dispatches` marker, causal `latest_event_sequence` CAS, `LambdaStreamRecoveryHandler` (BatchItemFailures) + `LambdaScheduledRecoveryHandler` (EventBridge backstop).
- **ADR-014 — gRPC transport**: third thin adapter over the shared core handlers. All 11 RPCs (9 unary + 2 server-streaming) wired via tonic; Tower auth layer reuses `MiddlewareStack`; streaming feeds from the ADR-009 durable event store with `a2a-last-event-id` metadata resume; `tenant` proto field wins over `x-tenant-id` metadata. Feature-gated (`grpc` on `turul-a2a-proto` + `turul-a2a` + `turul-a2a-client`); default builds remain tonic-free. Example: `examples/grpc-agent` (server + CLI client). Not available under `turul-a2a-aws-lambda`.

### Deferred (ordered by priority)

1. **Skill-level `security_requirements`** — agent-level only for now
2. **Shared `turul-jwt-validator` extraction** — currently local, see ADR-007
