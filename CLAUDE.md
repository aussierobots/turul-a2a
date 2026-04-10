# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Turul-a2a is a Rust implementation of the A2A (Agent-to-Agent) Protocol v1.0. Licensed under GPLv3.

**Proto-first architecture**: Types are generated from the normative `proto/a2a.proto` (package `lf.a2a.v1`) using `prost` + `pbjson`, then wrapped in ergonomic Rust types.

## Build & Development Commands

```bash
cargo build                        # Build all crates
cargo check --workspace            # Type-check entire workspace
cargo test --workspace             # Run all tests
cargo test -p turul-a2a-proto      # Test proto generation
cargo test -p turul-a2a-types      # Test types + state machine
cargo clippy --workspace           # Lint
cargo fmt --all                    # Format code
```

All crate dependencies MUST use `workspace = true` — versions are managed in root `Cargo.toml` `[workspace.dependencies]`.

## Git Conventions

- Commit messages: succinct, no Co-Authored-By attribution
- Do not push unless explicitly asked

## Architecture

### Crate Structure

- `turul-a2a-proto` — prost-generated types from `a2a.proto`. Build.rs generates via `prost-build` + `pbjson-build`. JSON serialization uses camelCase (proto JSON mapping) via pbjson.
- `turul-a2a-types` — Ergonomic Rust wrappers over proto types. Publishable, no server/storage/auth deps. `#[non_exhaustive]` on all public types. State machine enforcement on `TaskState`.
- `turul-a2a` — Server + storage + HTTP/JSON-RPC/SSE transports. Feature-gated backends (in-memory default). AgentExecutor trait for user-defined agent logic.

### Proto Build Pipeline

`proto/a2a.proto` → `prost-build` (generates Rust structs) + `pbjson-build` (generates serde impls with camelCase JSON) → `turul-a2a-proto` crate → wrapped by `turul-a2a-types`

Google well-known types (`google.protobuf.Struct`, `Value`, `Timestamp`) mapped to `pbjson_types` via `compile_well_known_types()` + `extern_path`.

### Phase 2 Exceptions (Temporary)

- **Push notification configs** use raw `turul_a2a_proto::TaskPushNotificationConfig` in storage traits and handler signatures. This is an intentional exception — push configs are simple CRUD with no state machine or invariants that warrant a wrapper. Keep this leakage isolated; do not let raw proto types spread into general handler/router code.
- **`last_chunk` on `append_artifact`** is transport-level metadata for Phase 3 SSE streaming. Storage passes it through but does not persist completion state in v0.1. The server layer should forward it to streaming subscribers, not model it in storage.

### Future Streaming Architecture Note

- The current in-process event broker is only valid for a single server instance.
- Before claiming production-ready streaming for multi-instance deployments or AWS Lambda, add a design task for cross-instance stream delivery and replay.
- Open question: where are stream events stored or coordinated when the instance that created them is not the one serving `:stream` or `:subscribe`?
- Minimum requirements for that future task:
  - durable or shared event coordination across instances
  - replay support for reconnects and missed events
  - ordering guarantees per task
  - explicit decision on whether Lambda supports true SSE directly or requires an alternative adapter path
- Candidate approaches include a shared event log or broker such as Redis, DynamoDB-backed event storage, EventBridge, or another external pub/sub mechanism. Do not assume the in-memory broker can be stretched to Lambda or horizontal scale.

### Architecture Decision Records

Documented under `docs/adr/`:

- **ADR-001**: Proto-first architecture — prost + pbjson generation with ergonomic wrappers
- **ADR-002**: Wrapper boundary and validation — TryFrom/Deserialize enforcement of REQUIRED fields
- **ADR-003**: Storage trait design — tenant/owner scoping, parity tests, push config exception
- **ADR-004**: Error model — A2A error codes, HTTP/JSON-RPC mapping, google.rpc.ErrorInfo
- **ADR-005**: Dual transport — shared core handlers for HTTP+JSON and JSON-RPC
- **ADR-006**: SSE streaming — in-process broker, last_chunk as transport metadata, single-instance limitation

For non-trivial architecture changes, the ADR should be accepted before implementation starts.

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
- TaskState: `TASK_STATE_SUBMITTED`, `TASK_STATE_WORKING`, etc.
- Error: TaskNotCancelableError → 409, ContentTypeNotSupportedError → 415
- All errors MUST include `google.rpc.ErrorInfo` with reason + domain
