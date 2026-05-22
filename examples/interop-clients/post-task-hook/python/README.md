# Python interop client — `post-task-hook-agent`

A minimal Python A2A client that calls **one specific agent in this
workspace**: `post-task-hook-agent`. It uses the official Python SDK
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

- **Crate name:** `post-task-hook-agent`
- **Source:** `examples/post-task-hook-agent/`
- **Default port:** `3014`
- **AgentCard:** `http://localhost:3014/.well-known/agent-card.json`

The agent registers two deterministic skills:

- `count` — `{n: number}` → `{squared: n*n}`
- `metrics` — returns the post-task hook's outcome counter.

After each skill call the executor fires a `TerminalHook` that records
success/failure plus a short summary of the last outcome. The `metrics`
skill exposes that counter so callers can observe the hook actually
ran.

Start it:

```sh
cargo run -p post-task-hook-agent
```

## What this client demonstrates

The client sends **four messages sequentially**:

1. Plain text `"count 3"` — expected artifact: `{"squared": 9}`.
2. Same again (call 2/3).
3. Same again (call 3/3).
4. Plain text `"metrics"` — expected artifact:
   `{"success": 3, "failure": 0, "last": "ok(count): {\"squared\":9}"}`.

The `metrics` payload is observable evidence that the hook fired
three times after the `count` calls.

For each message the client iterates the streamed lifecycle events
(`SUBMITTED` → `WORKING` → `artifact_update` → `COMPLETED`).

## Run

In one terminal, start the agent:

```sh
cargo run -p post-task-hook-agent
```

In another terminal, run the client:

```sh
cd examples/interop-clients/post-task-hook/python
python -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
python main.py
```

To target a non-default URL:

```sh
A2A_BASE_URL=http://localhost:3014 python main.py
```

## Expected output (truncated)

```
=== AgentCard ===
  name      : Post-Task Hook Agent
  version   : 0.1.0
  interface : binding='JSONRPC' url='http://localhost:3014/jsonrpc'

=== Sending: 'count 3' (expects {"squared": 9} (1/3)) ===
--- chunk #3 ---
artifact_update {
  artifact { name: "count" parts { text: "{\"squared\":9}" } }
  last_chunk: true
}
=== Done: 4 stream events ===

... (2/3 and 3/3 produce the same artifact) ...

=== Sending: 'metrics' (expects {...}) ===
--- chunk #3 ---
artifact_update {
  artifact {
    name: "metrics"
    parts {
      text: "{\"failure\":0,\"last\":\"ok(count): {\\\"squared\\\":9}\",\"success\":3}"
    }
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
