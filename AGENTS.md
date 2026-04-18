# Repository Guidelines

## Current State

- 9 crates in workspace, 3 example agents, 10 ADRs.
- **Strong for** (server deployment):
  - Durable event coordination: atomic task+event writes with terminal-preservation CAS on all four storage backends (in-memory, SQLite, PostgreSQL, DynamoDB).
  - Multi-instance request/response correctness and cross-instance streaming subscription via shared backend. Verified by `tests/distributed_tests.rs` and `tests/d3_streaming_tests.rs`.
  - Executor `EventSink` with spawn-and-track, two-deadline blocking timeout, and cross-instance cancellation propagation via the in-process cancel-marker poller (ADR-010, ADR-012).
  - Auth middleware (Bearer JWT + API Key), client library.
- **Lambda adapter** supports request/response and buffered-streaming SSE against the shared event store. Same-invocation cancellation works. The Lambda adapter does **not** run the persistent cross-instance cancel-marker poller that the server runtime uses, so cross-instance cancellation propagation to a live executor on a different warm Lambda invocation is not currently supported; see `crates/turul-a2a-aws-lambda/src/lib.rs` module docs for the exact scope.
- Compliance and maturity claims still require a fresh upstream-spec diff per the Version Hygiene and Compliance Audit Gate sections before new public claims land.

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
- Before claiming A2A compliance, protocol completeness, or "latest spec" support, perform a fresh diff between the vendored `proto/a2a.proto` and the current upstream A2A proto/spec revision being targeted.
- If the repo is intentionally pinned to an older upstream revision, document that explicitly and do not describe the implementation as compliant with the latest spec.
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

### Compliance Audit Gate

- A vendored proto is not, by itself, proof of current upstream compliance.
- Before approving work that is described as "A2A compliant", "spec complete", or equivalent, verify whether the vendored proto/spec snapshot still matches the latest official upstream revision the repo claims to target.
- If drift exists, classify it explicitly:
  - intentional version pin
  - local bug relative to the pinned revision
  - latest-upstream compliance gap
- Do not collapse those categories into one generic "spec bug" bucket.

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

## Example and API Surface Rules

- Examples and user-facing sample code must prefer wrapper/task helper APIs over direct `as_proto().clone()` mutation.
- If a common operation (complete a task, add an artifact, set status) requires manual proto construction in an example, that is a signal that the wrapper surface is missing a helper method. Design the helper first, then update the example.
- Repeated raw proto surgery in examples or tests is a review blocker — it teaches users the wrong pattern and bypasses the wrapper boundary the project claims to maintain.
- Raw proto access is an escape hatch for unusual cases, not the primary development path.

## Documentation Rules

- Keep `AGENTS.md` as the repo policy authority for agent behavior in this repo.
- Keep `CLAUDE.md` concise and operational.
- If both files exist and conflict, `AGENTS.md` wins.
- Do not document speculative endpoints or field names as if implemented.
- Code comments and rustdoc must describe the stable behavior, invariant, or public contract of the code as it exists. Do not mention internal implementation phases, task numbers, review cycles, temporary branch state, or agent workflow labels in code comments or doc strings.
- If a behavior is temporary while a feature is being built, keep that explanation in planning docs, ADR amendments, issues, or commit messages. In source comments, state the current contract plainly and add a TODO only when it names the concrete missing behavior, not the internal phase that will deliver it.
- Architectural decisions made during planning or implementation should be captured later as ADRs under `docs/adr/`.
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
