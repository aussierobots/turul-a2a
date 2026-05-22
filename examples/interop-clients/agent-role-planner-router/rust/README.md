# Rust interop client — `agent-role-planner-router-agent`

A minimal Rust client that exercises the `agent-role-planner-router-agent` example using the in-workspace `turul-a2a-client` crate.

## What is A2A?

A2A (Agent-to-Agent) is an open protocol for inter-agent communication. Agents publish a self-describing AgentCard at `/.well-known/agent-card.json`. Clients then talk to them over HTTP+JSON (REST or JSON-RPC) or gRPC. This client uses the HTTP+JSON REST binding. See the [A2A v1.0 spec](https://a2aproject.github.io/A2A/v1.0/specification/).

## Which agent this calls

`examples/agent-role-planner-router-agent` — a deterministic planner+router example. A small rules table inspects inbound text and dispatches to one of two registered skills:

- `add`     — sums two numbers (e.g. `"add 3 5"` or `"7 + 12"`).
- `concat`  — joins an array of strings (e.g. `"concat: foo bar baz"`).

## What this demonstrates

This client sends two sequential messages:

1. `"add 3 5"` — planner routes to `add`, agent returns `{"result":8}` as an artifact named `add`.
2. `"concat: foo bar baz"` — planner routes to `concat`, agent returns `{"joined":"foo bar baz"}` as an artifact named `concat`.

## Run

Open two terminals at the repo root.

Terminal 1 — start the agent (default port 3012):

```bash
cargo run -p agent-role-planner-router-agent
```

Terminal 2 — run the client:

```bash
cargo run -p interop-agent-role-planner-router-rust
```

To target a different host/port, set `A2A_BASE_URL`:

```bash
A2A_BASE_URL=http://localhost:3012 cargo run -p interop-agent-role-planner-router-rust
```

## Expected output

```
target: http://localhost:3012
agent: Planner-Router Agent v0.1.0
--- SendMessage request ---
text="add 3 5"
--- SendMessage response ---
kind=Task id=<uuid> state=Completed artifacts=1
artifact_text={"result":8}
--- SendMessage request ---
text="concat: foo bar baz"
--- SendMessage response ---
kind=Task id=<uuid> state=Completed artifacts=1
artifact_text={"joined":"foo bar baz"}
```

## Transport used

REST `POST /message:send` (the proto's `google.api.http` annotation). `turul-a2a-client` selects this transport directly rather than reading the agent card's `supportedInterfaces[0].protocolBinding`. The framework's `A2aServer` also serves a JSON-RPC `POST /jsonrpc` route — the Python and Go interop clients in this repo pick that one. Both are A2A v1.0-valid.
