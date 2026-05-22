# Python interop client — `agent-role-planner-router-agent`

A minimal Python A2A client that calls **one specific agent in this
workspace**: `agent-role-planner-router-agent`. It uses the official
Python SDK [`a2a-sdk==1.0.2`](https://pypi.org/project/a2a-sdk/).

## What is A2A?

A2A (Agent-to-Agent) is an open protocol that lets agents discover one
another and exchange messages over a small, well-defined wire surface
(JSON-RPC over HTTP, REST over HTTP, gRPC). Each agent publishes an
**AgentCard** at `/.well-known/agent-card.json` describing its name,
version, capabilities, and which transports it supports. For the full
specification see
[`a2aproject/A2A`](https://github.com/a2aproject/A2A).

## Which agent this calls

- **Crate name:** `agent-role-planner-router-agent`
- **Source:** `examples/agent-role-planner-router-agent/`
- **Default port:** `3012`
- **AgentCard:** `http://localhost:3012/.well-known/agent-card.json`

The agent registers two deterministic, non-LLM skills:

- `add` — `{a: number, b: number}` → `{result: number}`
- `concat` — `{strings: string[]}` → `{joined: string}`

A small planner inspects inbound text and decomposes it into a
`(skill_id, params)` plan; the router then dispatches via a
`SkillRegistry`.

Start it:

```sh
cargo run -p agent-role-planner-router-agent
```

## What this client demonstrates

The client sends **two messages sequentially**:

1. Plain text `"add 3 5"` — the planner picks the `add` skill. Expected
   artifact payload: `{"result": 8}`.
2. Plain text `"concat: foo bar baz"` — the planner picks the `concat`
   skill. Expected artifact payload: `{"joined": "foo bar baz"}`.

For each message the client iterates the streamed lifecycle events
(`SUBMITTED` → `WORKING` → `artifact_update` → `COMPLETED`).

## Run

In one terminal, start the agent:

```sh
cargo run -p agent-role-planner-router-agent
```

In another terminal, run the client:

```sh
cd examples/interop-clients/agent-role-planner-router/python
python -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
python main.py
```

To target a non-default URL:

```sh
A2A_BASE_URL=http://localhost:3012 python main.py
```

## Expected output (truncated)

```
=== AgentCard ===
  name      : Planner-Router Agent
  version   : 0.1.0
  interface : binding='JSONRPC' url='http://localhost:3012/jsonrpc'

=== Sending: 'add 3 5' (expects {"result": 8}) ===
--- chunk #3 ---
artifact_update {
  artifact { name: "add" parts { text: "{\"result\":8}" } }
  last_chunk: true
}
=== Done: 4 stream events ===

=== Sending: 'concat: foo bar baz' (expects {"joined": "foo bar baz"}) ===
--- chunk #3 ---
artifact_update {
  artifact { name: "concat" parts { text: "{\"joined\":\"foo bar baz\"}" } }
  last_chunk: true
}
=== Done: 4 stream events ===
```

## Transport used

JSON-RPC over `POST /jsonrpc`. The AgentCard advertises
`supportedInterfaces[0].protocolBinding=JSONRPC` and the Python SDK
selects that transport. Because the SDK sets
`Accept: text/event-stream`, the wire JSON-RPC `method` is
`SendStreamingMessage` (the Python call is `client.send_message(...)`).
The response is an SSE stream of JSON-RPC envelopes carrying lifecycle
events.

## Files

- `main.py` — the client (`asyncio` + `httpx` + `a2a-sdk`).
- `requirements.txt` — pinned `a2a-sdk==1.0.2`.
- `pyproject.toml` — PEP 621 metadata with the same pin.
- `.gitignore` — keeps `.venv/`, `__pycache__/`, etc. out of git.
