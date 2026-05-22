# Rust interop client — `post-task-hook-agent`

A minimal Rust client that exercises the `post-task-hook-agent` example using the in-workspace `turul-a2a-client` crate.

## What is A2A?

A2A (Agent-to-Agent) is an open protocol for inter-agent communication. Agents publish a self-describing AgentCard at `/.well-known/agent-card.json`. Clients then talk to them over HTTP+JSON (REST or JSON-RPC) or gRPC. This client uses the HTTP+JSON REST binding. See the [A2A v1.0 spec](https://a2aproject.github.io/A2A/v1.0/specification/).

## Which agent this calls

`examples/post-task-hook-agent` — a deterministic agent that fires a `TerminalHook` after every skill call. The hook records each outcome into an in-memory counter so callers can verify it actually fired. Two skills are registered:

- `count`   — `{"n": <number>}` → `{"squared": n*n}`.
- `metrics` — returns the hook-recorded counter snapshot `{success, failure, last}`.

The agent's planner accepts plain-text `count <number>` and the keyword `metrics`.

## What this demonstrates

The client sends four sequential messages:

1. `"count 3"` (× 3) — each call routes to `count`, returns `{"squared":9}`, and trips the hook on the success branch.
2. `"metrics"` — routes to `metrics`, returns the hook's running counter so you can see `success=3, failure=0`.

## Run

Open two terminals at the repo root.

Terminal 1 — start the agent (default port 3014):

```bash
cargo run -p post-task-hook-agent
```

Terminal 2 — run the client:

```bash
cargo run -p interop-post-task-hook-rust
```

To target a different host/port, set `A2A_BASE_URL`:

```bash
A2A_BASE_URL=http://localhost:3014 cargo run -p interop-post-task-hook-rust
```

The agent's counter is in-process state. Restarting the agent resets it. The client also sends the four messages within a single run, so its `metrics` call always sees three preceding `count` successes (plus the hook for `metrics` itself, which fires after the artifact is read so it does not appear in the same snapshot).

## Expected output

```
target: http://localhost:3014
agent: Post-Task Hook Agent v0.1.0
--- SendMessage request ---
text="count 3"
--- SendMessage response ---
kind=Task id=<uuid> state=Completed artifacts=1
artifact_text={"squared":9}
... (two more identical count blocks) ...
--- SendMessage request ---
text="metrics"
--- SendMessage response ---
kind=Task id=<uuid> state=Completed artifacts=1
artifact_text={"failure":0,"last":"ok(count): {\"squared\":9}","success":3}
```

## Transport used

REST `POST /message:send` (the proto's `google.api.http` annotation). `turul-a2a-client` selects this transport directly rather than reading the agent card's `supportedInterfaces[0].protocolBinding`. The framework's `A2aServer` also serves a JSON-RPC `POST /jsonrpc` route — the Python and Go interop clients in this repo pick that one. Both are A2A v1.0-valid.
