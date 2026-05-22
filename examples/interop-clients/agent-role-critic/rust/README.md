# Rust interop client — `agent-role-critic-agent`

A minimal Rust client that exercises the `agent-role-critic-agent` example using the in-workspace `turul-a2a-client` crate.

## What is A2A?

A2A (Agent-to-Agent) is an open protocol for inter-agent communication. Agents publish a self-describing AgentCard at `/.well-known/agent-card.json`. Clients then talk to them over HTTP+JSON (REST or JSON-RPC) or gRPC. This client uses the HTTP+JSON REST binding. See the [A2A v1.0 spec](https://a2aproject.github.io/A2A/v1.0/specification/).

## Which agent this calls

`examples/agent-role-critic-agent` — a deterministic critic/evaluator example. Dispatches inbound JSON on its `kind` field to one of two skills:

- `validate_against_schema` — runs JSON Schema 2020-12 validation, returns `{valid, errors}`.
- `check_invariants`        — runs a deterministic invariant table (`non_empty`, `min_length`, `max_length`, `contains`), returns `{verdict, failures}`.

## What this demonstrates

The client sends two sequential JSON-shaped messages:

1. `{"kind":"validate_against_schema","value":42,"schema":{"type":"integer"}}` → `{"valid":true,"errors":[]}`.
2. `{"kind":"check_invariants","value":"hello world","invariants":[...]}` → `{"verdict":"pass","failures":[]}`.

Each agent response arrives as a completed `Task` carrying one artifact whose payload is the critic verdict.

## Run

Open two terminals at the repo root.

Terminal 1 — start the agent (default port 3013):

```bash
cargo run -p agent-role-critic-agent
```

Terminal 2 — run the client:

```bash
cargo run -p interop-agent-role-critic-rust
```

To target a different host/port, set `A2A_BASE_URL`:

```bash
A2A_BASE_URL=http://localhost:3013 cargo run -p interop-agent-role-critic-rust
```

## Expected output

```
target: http://localhost:3013
agent: Critic Agent v0.1.0
--- SendMessage request ---
text={"kind":"validate_against_schema","schema":{"type":"integer"},"value":42}
--- SendMessage response ---
kind=Task id=<uuid> state=Completed artifacts=1
artifact_text={"errors":[],"valid":true}
--- SendMessage request ---
text={"invariants":[{"check":"non_empty","name":"ne"},{"args":{"needle":"world"},"check":"contains","name":"has_world"}],"kind":"check_invariants","value":"hello world"}
--- SendMessage response ---
kind=Task id=<uuid> state=Completed artifacts=1
artifact_text={"failures":[],"verdict":"pass"}
```

JSON object-field ordering varies because `serde_json::Value` serialises alphabetically. The fields themselves are stable.

## Transport used

REST `POST /message:send` (the proto's `google.api.http` annotation). `turul-a2a-client` selects this transport directly rather than reading the agent card's `supportedInterfaces[0].protocolBinding`. The framework's `A2aServer` also serves a JSON-RPC `POST /jsonrpc` route — the Python and Go interop clients in this repo pick that one. Both are A2A v1.0-valid.
