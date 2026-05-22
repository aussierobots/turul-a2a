# Rust interop client — `skill-manifest-ollama-agent`

A minimal Rust client that exercises the `skill-manifest-ollama-agent` example using the in-workspace `turul-a2a-client` crate.

## What is A2A?

A2A (Agent-to-Agent) is an open protocol for inter-agent communication. Agents publish a self-describing AgentCard at `/.well-known/agent-card.json`. Clients then talk to them over HTTP+JSON (REST or JSON-RPC) or gRPC. This client uses the HTTP+JSON REST binding. See the [A2A v1.0 spec](https://a2aproject.github.io/A2A/v1.0/specification/).

## Which agent this calls

`examples/skill-manifest-ollama-agent` — an A2A agent driven by a `SKILL.md` manifest. The bundled `greet` skill takes `{user.name, style}` and returns `{greeting: "<text>"}`. Defaults to an offline stub so the round-trip is hermetic.

## What this demonstrates

- Sending the manifest's documented input as a single JSON-text part: `{"user":{"name":"Ada"},"style":"formal"}`.
- Receiving a completed `Task` carrying a `greeting` artifact whose payload is the structured `{"greeting": "..."}` output validated against the manifest's `outputSchema`.

The agent's offline stub returns `{"greeting":"Good day, Ada! (offline stub)"}` for the `formal` style.

## Run

Open two terminals at the repo root.

Terminal 1 — start the agent (default port 3010):

```bash
cargo run -p skill-manifest-ollama-agent
```

Terminal 2 — run the client:

```bash
cargo run -p interop-skill-manifest-ollama-rust
```

To target a different host/port, set `A2A_BASE_URL`:

```bash
A2A_BASE_URL=http://localhost:3010 cargo run -p interop-skill-manifest-ollama-rust
```

## Expected output

```
target: http://localhost:3010
agent: Skill Manifest Ollama Agent v0.1.0
--- SendMessage request ---
text={"style":"formal","user":{"name":"Ada"}}
--- SendMessage response ---
kind=Task id=<uuid> state=Completed artifacts=1
artifact_text={"greeting":"Good day, Ada! (offline stub)"}
```

## Transport used

REST `POST /message:send` (the proto's `google.api.http` annotation). `turul-a2a-client` selects this transport directly rather than reading the agent card's `supportedInterfaces[0].protocolBinding`. The framework's `A2aServer` also serves a JSON-RPC `POST /jsonrpc` route — the Python and Go interop clients in this repo pick that one. Both are A2A v1.0-valid.
