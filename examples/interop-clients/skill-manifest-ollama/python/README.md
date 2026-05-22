# Python interop client — `skill-manifest-ollama-agent`

A minimal Python A2A client that calls **one specific agent in this
workspace**: `skill-manifest-ollama-agent`. It uses the official Python SDK
[`a2a-sdk==1.0.2`](https://pypi.org/project/a2a-sdk/).

## What is A2A?

A2A (Agent-to-Agent) is an open protocol that lets agents discover one
another and exchange messages over a small, well-defined wire surface
(JSON-RPC over HTTP, REST over HTTP, gRPC). Each agent publishes an
**AgentCard** at `/.well-known/agent-card.json` describing its name,
version, capabilities, and which transports it supports. For the full
specification see
[`a2aproject/A2A`](https://github.com/a2aproject/A2A).

## Which agent this calls

- **Crate name:** `skill-manifest-ollama-agent`
- **Source:** `examples/skill-manifest-ollama-agent/`
- **Default port:** `3010`
- **AgentCard:** `http://localhost:3010/.well-known/agent-card.json`

The agent loads a `SKILL.md` manifest at startup, registers a single skill
called `greet`, and validates inbound parameters against the manifest's
JSON Schema. In offline mode (the default) it returns a deterministic stub
greeting; with `OLLAMA_BASE_URL` set, it forwards to a local Ollama instance.

Start it in offline mode:

```sh
cargo run -p skill-manifest-ollama-agent
```

## What this client demonstrates

The client sends one message containing a JSON-encoded `greet` request:

```json
{"user":{"name":"Ada"},"style":"formal"}
```

Expected artifact name: `greeting`. Expected artifact payload (offline
stub):

```json
{"greeting":"Good day, Ada! (offline stub)"}
```

The client iterates the streamed lifecycle events (`SUBMITTED` →
`WORKING` → `artifact_update` → `COMPLETED`) and prints each chunk.

## Run

In one terminal, start the agent:

```sh
cargo run -p skill-manifest-ollama-agent
```

In another terminal, run the client:

```sh
cd examples/interop-clients/skill-manifest-ollama/python
python -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
python main.py
```

To target a non-default URL:

```sh
A2A_BASE_URL=http://localhost:3010 python main.py
```

## Expected output (truncated)

```
=== AgentCard ===
  name       : Skill Manifest Ollama Agent
  version    : 0.1.0
  interface  : binding='JSONRPC' url='http://localhost:3010/jsonrpc'

=== Sending `greet` request ===
  payload    : {"user":{"name":"Ada"},"style":"formal"}
  message_id : <hex>

=== Stream events ===
--- chunk #1 ---
status_update { ... status { state: TASK_STATE_SUBMITTED } }
--- chunk #2 ---
status_update { ... status { state: TASK_STATE_WORKING } }
--- chunk #3 ---
artifact_update {
  artifact {
    name: "greeting"
    parts { text: "{\"greeting\":\"Good day, Ada! (offline stub)\"}" }
  }
  last_chunk: true
}
--- chunk #4 ---
status_update { ... status { state: TASK_STATE_COMPLETED } }

=== Done: 4 stream events received ===
```

## Transport used

JSON-RPC over `POST /jsonrpc`. The AgentCard advertises
`supportedInterfaces[0].protocolBinding=JSONRPC` and the Python SDK selects
that transport. Because the SDK sets `Accept: text/event-stream`, the wire
JSON-RPC `method` is `SendStreamingMessage` (the Python call is
`client.send_message(...)`). The response is an SSE stream of JSON-RPC
envelopes carrying lifecycle events.

## Files

- `main.py` — the client (`asyncio` + `httpx` + `a2a-sdk`).
- `requirements.txt` — pinned `a2a-sdk==1.0.2`.
- `pyproject.toml` — PEP 621 metadata with the same pin.
- `.gitignore` — keeps `.venv/`, `__pycache__/`, etc. out of git.
