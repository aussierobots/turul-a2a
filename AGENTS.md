# Repository Guidelines

## Current State

- The repository has a working Cargo workspace with 3 crates: `turul-a2a-proto`, `turul-a2a-types`, `turul-a2a`.
- 199 tests cover proto generation, types, storage, HTTP handlers, JSON-RPC dispatch, SSE streaming, and E2E scenarios.
- v0.1 scope: HTTP+JSON, JSON-RPC, SSE streaming, in-memory storage. Auth, client, Lambda, additional backends deferred.
- Architecture decisions are documented in `docs/adr/`.

## Source of Truth

When working on `turul-a2a`, use these authorities in this order:

1. The current A2A v1.x Protocol Buffers definition (`a2a.proto`) from the official A2A definitions page.
2. The current A2A specification and topic pages from `https://a2a-protocol.org/latest/`.
3. Local repository policy in this file.
4. Local repository guidance in `CLAUDE.md`.
5. Existing patterns from:
   - `/Users/nick/turul-mcp-framework`
   - `/Users/nick/gps-trust-auth`

If these conflict, the normative A2A proto wins for wire-level behavior.

## Default Role In This Repo

- Default stance: act as a critic and architecture reviewer first.
- Prioritize:
  - A2A spec compliance
  - architecture quality
  - boundary discipline
  - test quality
  - auth and deployment correctness
- Do not make implementation changes unless the user explicitly asks for code changes.
- For review requests, lead with concrete findings, not summaries.

## Non-Negotiable A2A Rules

### Proto-First

- The A2A proto is normative. Do not hand-author wire contracts as the primary source of truth.
- Prefer a proto-first architecture:
  - generate core protocol types from `a2a.proto`
  - add ergonomic wrapper/conversion layers only where justified
  - keep generated and public wrapper boundaries explicit
- If a hand-written type differs from proto naming, cardinality, enum values, or route shape, treat that as a compliance bug.

### Version Hygiene

- Always verify the current A2A version before making protocol claims.
- Do not mix older `v0.1`/`v0.2`/`v0.3` field names or routes into v1.x planning.
- Common drift risks that must be checked explicitly:
  - `.well-known/agent-card.json` vs older `.well-known/agent.json`
  - `/extendedAgentCard` vs older authenticated card paths
  - `/message:send` and `/message:stream` vs older `/messages`
  - `TASK_STATE_SUBMITTED` vs older `PENDING`
  - `history` vs `messages`
  - `message_id` / `messageId` vs ad hoc `id`
  - `Part` oneof `text` / `raw` / `url` / `data` vs stale discriminator models

### Wire Truthfulness

- JSON-RPC method names, HTTP routes, query fields, and JSON field casing must match the current A2A definitions exactly.
- Do not "normalize" the protocol into cleaner local names at the wire boundary.
- Wrapper APIs may be ergonomic internally, but boundary codecs must remain exact.

## Architecture Guardrails

### Keep Core Publishable

- The core protocol/types crate must remain publishable independently.
- Do not couple the publishable core to:
  - GPS Trust private crates
  - auth middleware
  - storage backends
  - AWS Lambda runtime

### Preferred Boundary Split

Mirror the good parts of `/Users/nick/turul-mcp-framework`, but do not force a one-to-one crate explosion before it is earned.

Preferred separation:

1. Protocol/types layer
2. Storage abstraction layer
3. Server/transport layer
4. Optional auth integration layer
5. Optional client layer
6. Optional Lambda adapter

Rules:

- Keep protocol concerns out of storage and transport crates.
- Keep auth optional and middleware-based.
- Keep deployment adapters thin.
- Avoid baking GPS Trust-specific assumptions into general-purpose crates.

### Storage Pattern

Storage should follow the trait-plus-backends pattern used in `/Users/nick/turul-mcp-framework`:

- in-memory
- SQLite
- PostgreSQL
- DynamoDB

Requirements:

- one authoritative trait surface
- parity tests shared across backends
- feature-gated backend dependencies
- clear error model
- explicit pagination contract

Do not add backend-specific behavior to the public semantics unless the protocol requires it.

### Auth Pattern

Auth should be evaluated in two separate roles:

1. Can `gps-trust-auth` serve as the Authorization Server for A2A deployments?
2. Can `gps-trust-jwt-validator` or a similar validator support A2A resource-server middleware cleanly?

Rules:

- Keep auth out of the core crate.
- Do not assume GPS Trust auth is appropriate for all downstream users.
- If a GPS Trust dependency is introduced, isolate it to an opt-in auth crate.
- Preserve a path for non-GPS-Trust adopters.

## Testing Policy

### Test-First

- Tests derived from the spec and proto come first.
- Do not change tests to fit buggy implementation behavior.
- If code and tests disagree, re-check the current A2A proto and spec before changing either.

### Required Test Layers

Any serious implementation plan should include:

1. Unit tests for wire types and enum/value mapping
2. State-machine tests for task lifecycle transitions
3. Storage parity tests across all backends
4. HTTP binding tests
5. JSON-RPC binding tests
6. Streaming/SSE tests
7. Auth scoping tests
8. End-to-end compliance scenarios

### Compliance Focus

Tests should verify protocol behavior, not just Rust internals:

- exact route shapes
- exact JSON field names
- exact enum/string mapping
- pagination fields and limits
- task history semantics
- streaming event ordering
- auth-required and input-required behavior
- truthful capability advertisement

## Review Discipline

When reviewing plans, docs, or code in this repo, always check:

### A2A Compliance

- Is the plan based on the current v1.x definitions, not stale examples?
- Are the HTTP bindings taken from proto annotations?
- Are JSON names derived from proto JSON mapping?
- Are task states current and complete?
- Are `Part` semantics current and complete?
- Are well-known discovery paths current?

### Architecture

- Is the protocol/types crate independent?
- Are storage abstractions backend-neutral?
- Is auth optional?
- Is Lambda an adapter, not a second server implementation?
- Is the plan minimal enough for the repo’s current bootstrap state?

### Best Practices

- No unnecessary new abstraction layers
- No speculative crates without a clear boundary
- No private GPS Trust coupling in public core APIs
- No docs that claim behavior without tests

## Planning Rules

- Plans must separate:
  - mandatory now
  - optional later
  - deferred future work
- Do not present a large multi-crate shape as mandatory if a smaller initial cut can preserve the same public architecture.
- If the spec offers multiple bindings, explicitly state which bindings are in scope for the first release and which are deferred.
- Because proto is normative, plans must state how generated types are managed:
  - generated directly as public API
  - or wrapped behind a stable public layer

## Guidance For This Repo Specifically

- Use `/Users/nick/turul-mcp-framework` as the reference for:
  - storage traits/backends
  - builder ergonomics
  - middleware boundaries
  - Lambda adapter layering
  - parity-test patterns
- Use `/Users/nick/gps-trust-auth` as the reference for:
  - OAuth 2.1 Authorization Server constraints
  - JWT issuer/audience discipline
  - JWKS validation expectations
  - separation between AS and RS responsibilities
- Do not assume MCP semantics transfer directly to A2A:
  - A2A tasks are the state carrier
  - transport/session assumptions differ
  - task lifecycle and message model differ materially

## Commands

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Documentation Rules

- Keep `AGENTS.md` as the repo policy authority for agent behavior in this repo.
- Keep `CLAUDE.md` concise and operational.
- If both files exist and conflict, `AGENTS.md` wins.
- Do not document speculative endpoints or field names as if implemented.
- Architectural decisions made during planning or implementation should be captured later as ADRs under `docs/ADR/`.
- For meaningful architecture changes, require the ADR to be written and accepted before implementation proceeds.

## Final Review Standard

Before approving any implementation plan for `turul-a2a`, the plan should satisfy all of the following:

- proto-first compliance is explicit
- stale pre-v1 A2A names are removed
- core crate remains publishable and auth-free
- storage pattern matches the Turul backend discipline
- auth integration is optional and isolated
- local binary and AWS Lambda deployment paths are both addressed
- unit, parity, streaming, auth, and e2e compliance tests are part of the plan
- tests are written to the protocol, not retrofitted to implementation
