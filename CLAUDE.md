# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Turul-a2a is a Rust implementation of the A2A (Agent-to-Agent) Protocol v1.0. Licensed under GPLv3.

**Proto-first architecture**: Types are generated from the normative `proto/a2a.proto` (package `lf.a2a.v1`) using `prost` + `pbjson`, then wrapped in ergonomic Rust types.

## Build & Development Commands

```bash
cargo build --workspace              # Build all crates
cargo check --workspace              # Type-check
cargo test --workspace               # Run all 202 tests
cargo test -p turul-a2a-proto        # Proto generation + wire format (26 tests)
cargo test -p turul-a2a-types        # Types + state machine (54 tests)
cargo test -p turul-a2a              # Server + storage + handlers + auth
cargo test -p turul-a2a-auth         # Auth middleware (API key, Bearer)
cargo test -p turul-jwt-validator    # JWT validator
cargo test -p turul-a2a --test e2e_tests    # E2E scenarios (12 tests)
cargo test -p turul-a2a --test sse_tests    # SSE transport (10 tests)
cargo test -p turul-a2a --test jsonrpc_tests # JSON-RPC dispatch (19 tests)
cargo clippy --workspace             # Lint
cargo fmt --all                      # Format
cargo run -p echo-agent              # Run echo agent example on :3000
```

**All crate dependencies MUST use `workspace = true`** — versions are managed in root `Cargo.toml` `[workspace.dependencies]`. This includes dev-dependencies. Never put a version number in a crate's own `Cargo.toml` — add the dependency to the workspace root first, then reference it with `{ workspace = true }` in the crate.

## Git Conventions

- Commit messages: succinct, no Co-Authored-By attribution
- Do not push unless explicitly asked

## Architecture

### Crate Structure

- `turul-a2a-proto` — prost-generated types from `a2a.proto`. Build.rs generates via `prost-build` + `pbjson-build`. JSON serialization uses camelCase (proto JSON mapping) via pbjson.
- `turul-a2a-types` — Ergonomic Rust wrappers over proto types. Publishable, no server/storage/auth deps. `#[non_exhaustive]` on all public types. State machine enforcement on `TaskState`.
- `turul-a2a` — Server + storage + HTTP/JSON-RPC/SSE transports + auth middleware foundation. Feature-gated backends (in-memory default). `AgentExecutor` trait, `A2aMiddleware` trait, `A2aServer::builder()`.
- `turul-a2a-auth` — Concrete auth middleware: `BearerMiddleware` (JWT), `ApiKeyMiddleware`. Uses `turul-jwt-validator`.
- `turul-jwt-validator` — Local JWT validator with JWKS caching (design sourced from turul-mcp-oauth).

### Proto Build Pipeline

`proto/a2a.proto` → `prost-build` (generates Rust structs) + `pbjson-build` (generates serde impls with camelCase JSON) → `turul-a2a-proto` crate → wrapped by `turul-a2a-types`

Google well-known types (`google.protobuf.Struct`, `Value`, `Timestamp`) mapped to `pbjson_types` via `compile_well_known_types()` + `extern_path`.

### Phase 2 Exceptions (Temporary)

- **Push notification configs** use raw `turul_a2a_proto::TaskPushNotificationConfig` in storage traits and handler signatures. This is an intentional exception — push configs are simple CRUD with no state machine or invariants that warrant a wrapper. Keep this leakage isolated; do not let raw proto types spread into general handler/router code.
- **`last_chunk` on `append_artifact`** is transport-level metadata for SSE streaming. Storage passes it through but does not persist completion state in v0.1. The server layer forwards it to streaming subscribers.

### Future Streaming Architecture Note

- The current in-process event broker is only valid for a single server instance.
- Before claiming production-ready streaming for multi-instance deployments or AWS Lambda, add a design task for cross-instance stream delivery and replay.
- Candidate approaches include a shared event log (Redis, DynamoDB, EventBridge).

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

For non-trivial architecture changes, the ADR should be accepted before implementation starts.

### Example and API Surface Policy

- **Examples must prefer wrapper/helper APIs over raw proto mutation.** Repeated `as_proto().clone()` + manual proto construction in examples is a design smell — it signals the wrapper layer is missing a helper that should be designed.
- If a simple example needs generated-proto editing to do normal work (e.g., complete a task, add an artifact), stop and evaluate whether `Task`, `Artifact`, or related types are missing a helper method.
- Raw proto access (`as_proto()`, `as_proto_mut()`) is an escape hatch, not the primary path for common operations.

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
- Error: TaskNotFoundError → 404/-32001, TaskNotCancelableError → 409/-32002, ContentTypeNotSupportedError → 415/-32005
- All A2A errors MUST include `google.rpc.ErrorInfo` with reason + domain

### Deferred

- Client library (`turul-a2a-client`)
- AWS Lambda adapter (`turul-a2a-aws-lambda`)
- Additional storage backends (SQLite, PostgreSQL, DynamoDB)
- gRPC transport (feature-gated in `turul-a2a-proto`)
- Skill-level `security_requirements` (agent-level only for now)
- Shared `turul-jwt-validator` extraction (currently local, see ADR-007)
