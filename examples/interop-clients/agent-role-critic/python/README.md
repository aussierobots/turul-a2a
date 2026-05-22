# Python interop client — `agent-role-critic-agent`

A minimal Python A2A client that calls **one specific agent in this
workspace**: `agent-role-critic-agent`. It uses the official Python SDK
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

- **Crate name:** `agent-role-critic-agent`
- **Source:** `examples/agent-role-critic-agent/`
- **Default port:** `3013`
- **AgentCard:** `http://localhost:3013/.well-known/agent-card.json`

The agent registers two deterministic, non-LLM skills:

- `validate_against_schema` — runs a JSON Schema 2020-12 validation.
  Returns `{valid: bool, errors: [string]}`.
- `check_invariants` — runs an invariant table (`non_empty`,
  `min_length`, `max_length`, `contains`) over a value. Returns
  `{verdict: "pass"|"fail", failures: [...]}`.

The agent's executor dispatches based on the inbound JSON payload's
`kind` field.

Start it:

```sh
cargo run -p agent-role-critic-agent
```

## What this client demonstrates

The client sends **two messages sequentially** (each is a single JSON
text part):

1. `validate_against_schema` with `value=42`, `schema={"type":"integer"}`.
   Expected artifact payload: `{"valid": true, "errors": []}`.
2. `check_invariants` with `value="hello"` and a two-entry invariant
   table (`non_empty` + `min_length=3`). Expected artifact payload:
   `{"verdict": "pass", "failures": []}`.

For each message the client iterates the streamed lifecycle events
(`SUBMITTED` → `WORKING` → `artifact_update` → `COMPLETED`).

## Run

In one terminal, start the agent:

```sh
cargo run -p agent-role-critic-agent
```

In another terminal, run the client:

```sh
cd examples/interop-clients/agent-role-critic/python
python -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
python main.py
```

To target a non-default URL:

```sh
A2A_BASE_URL=http://localhost:3013 python main.py
```

## Expected output (truncated)

```
=== AgentCard ===
  name      : Critic Agent
  version   : 0.1.0
  interface : binding='JSONRPC' url='http://localhost:3013/jsonrpc'

=== Sending: validate_against_schema ===
--- chunk #3 ---
artifact_update {
  artifact {
    name: "validate_against_schema"
    parts { text: "{\"errors\":[],\"valid\":true}" }
  }
  last_chunk: true
}
=== Done: 4 stream events ===

=== Sending: check_invariants ===
--- chunk #3 ---
artifact_update {
  artifact {
    name: "check_invariants"
    parts { text: "{\"failures\":[],\"verdict\":\"pass\"}" }
  }
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
