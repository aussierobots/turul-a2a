# Rust interop client — `skill-dispatch-profile-agent`

A minimal Rust client that exercises the `skill-dispatch-profile-agent` example using the in-workspace `turul-a2a-client` crate.

## What is A2A?

A2A (Agent-to-Agent) is an open protocol for inter-agent communication. Agents publish a self-describing AgentCard at `/.well-known/agent-card.json`. Clients then talk to them over HTTP+JSON (REST or JSON-RPC) or gRPC. This client uses the HTTP+JSON REST binding. See the [A2A v1.0 spec](https://a2aproject.github.io/A2A/v1.0/specification/).

## Which agent this calls

`examples/skill-dispatch-profile-agent` — an A2A agent that hosts two manifest-backed skills (`echo_loud`, `reverse`) and dispatches between them using the skill-invocation dispatcher profile.

## What this demonstrates

The dispatcher profile lets a single A2A endpoint expose multiple skills without changing the wire protocol. The client opts in per request by:

1. Setting an HTTP request header:

   ```
   A2A-Extensions: https://turul.dev/a2a/extensions/skill-invocation/v1
   ```

2. Stamping `Message.metadata` with two reserved keys:

   - `a2a.skillId` — string, the target `AgentSkill.id` (e.g. `"echo_loud"`).
   - `a2a.skillParams` — object, the structured input for that skill (validated against the skill's `inputSchema`).

The agent reads the metadata, routes to the registered skill handler, and emits the result as a Task artifact whose body matches the manifest's `outputSchema`. The server echoes the activated extension URI on the response.

This client runs the two-call sequence used by the Python and Go siblings:

| Call | `a2a.skillId` | `a2a.skillParams`     | Expected artifact body  |
| ---- | ------------- | --------------------- | ----------------------- |
| 1    | `echo_loud`   | `{"text":"hello"}`    | `{"shouted":"HELLO"}`   |
| 2    | `reverse`     | `{"text":"abc"}`      | `{"reversed":"cba"}`    |

## Run

Open two terminals at the repo root.

Terminal 1 — start the agent (default port 3015):

```bash
cargo run -p skill-dispatch-profile-agent
```

Terminal 2 — run the client:

```bash
cargo run -p interop-skill-dispatch-profile-rust
```

To target a different host/port, set `A2A_BASE_URL`:

```bash
A2A_BASE_URL=http://localhost:3015 cargo run -p interop-skill-dispatch-profile-rust
```

## Expected output

```
target: http://localhost:3015
profile: https://turul.dev/a2a/extensions/skill-invocation/v1
agent: Skill Dispatch Profile Agent v0.1.0
advertised extensions: ["https://turul.dev/a2a/extensions/skill-invocation/v1"]

--- dispatch request ---
skill_id=echo_loud
params={"text":"hello"}
--- dispatch response ---
kind=Task id=<uuid> state=Completed artifacts=1
artifact_text={"shouted":"HELLO"}
match=ok

--- dispatch request ---
skill_id=reverse
params={"text":"abc"}
--- dispatch response ---
kind=Task id=<uuid> state=Completed artifacts=1
artifact_text={"reversed":"cba"}
match=ok
```

## Transport used

REST `POST /message:send` (the proto's `google.api.http` annotation). `turul-a2a-client`'s `A2aClient::with_extensions(...)` attaches the `A2A-Extensions` request header on every outbound call; `MessageBuilder::metadata_json(HashMap<String, Value>)` builds `Message.metadata` from a flat `HashMap`. The framework also serves a JSON-RPC `POST /jsonrpc` route — both bindings are A2A v1.0-valid; this client picks REST.
