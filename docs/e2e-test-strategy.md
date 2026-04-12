# E2E Test Strategy

## Path A: Rust Client + Rust Server (Primary Regression)

**Role**: Primary E2E regression gate. Runs in CI.

**How**: Real `A2aServer` on `localhost:0`, `A2aClient` over TCP. No mocks.

**12 tests**: discovery, send, get, not-found, cancel, list (empty + pagination), multi-turn, tenant isolation, API key auth (accepted + rejected), error envelope format.

**Location**: `crates/turul-a2a/tests/e2e_tcp_tests.rs`

## Path B: Go SDK Client + Rust Server (Primary External Interop)

**Role**: Primary external interoperability. Real SDK client, not raw HTTP.

**How**: Official Go A2A SDK (`a2aproject/a2a-go v2`, spec v1.0) discovers our agent card via `agentcard.DefaultResolver`, creates a client via `a2aclient.NewFromCard()`, and exercises CRUD operations over JSON-RPC transport.

**6 tests**: discovery, send_message, get_task, cancel_completed (error), list_tasks, get_not_found (error). All pass through the SDK's client abstraction layer.

**Version alignment**: Both target spec v1.0. AgentCard model, REST routes, JSON-RPC methods, and discovery all match between implementations.

**Location**: `interop/go-sdk/`
**Run**: `cd interop/go-sdk && TURUL_SERVER_URL=http://localhost:3000 go test -v`

## Path C: Python Interop Harness (Wire Format + SDK Compatibility Probe)

**Role**: Standing compatibility probe against the official Python SDK ecosystem.

Two tracks:

1. **JSON-RPC wire format interop** (6 tests, `httpx`): Proves our JSON-RPC wire format is consumable from Python. Not SDK-layer interop — raw HTTP POST to `/jsonrpc`.

2. **SDK compatibility probe** (1 test, `a2a-sdk`): `A2ACardResolver` fetch against our server. Currently **expected to skip** because:
   - `a2a-sdk==0.3.26` (latest PyPI) implements spec v0.3
   - Our server targets v1.0 (proto `lf.a2a.v1`)
   - SDK `AgentCard` model requires top-level `url` field not in v1.0 proto
   - SDK REST transport uses `/v1/` prefixed routes

This probe is a **permanent live signal** for ecosystem alignment. When the Python SDK ships v1.0-aligned models, the skip disappears and the test starts passing — no changes needed on our side.

**Location**: `interop/python-sdk/`
**Run**: `cd interop/python-sdk && uv sync && TURUL_SERVER_URL=http://localhost:3000 uv run pytest -v`

## Path D: Rust Client + Go SDK Server — Deferred

Feasible (Go SDK has runnable sample agents). Deferred until there's a specific need.

## Design Constraints

- External interop tests are **opt-in** — not in default `cargo test`
- Python and Go are NOT required runtime dependencies of the Rust crates
- Path A is the CI gate; Paths B and C are interop signals
- `uv` manages Python; system Go manages Go
