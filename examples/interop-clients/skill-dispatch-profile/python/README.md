# Python interop client — `skill-dispatch-profile-agent`

A minimal Python A2A client that exercises the **skill-invocation
dispatcher profile** against a specific agent in this workspace:
`skill-dispatch-profile-agent`. It uses the official Python SDK
[`a2a-sdk==1.0.2`](https://pypi.org/project/a2a-sdk/) plus a small amount
of `httpx` glue.

## What is A2A?

A2A (Agent-to-Agent) is an open protocol that lets agents discover one
another and exchange messages over a small, well-defined wire surface
(JSON-RPC over HTTP, REST over HTTP, gRPC). Each agent publishes an
**AgentCard** at `/.well-known/agent-card.json` describing its name,
version, capabilities, and which transports it supports. For the full
specification see
[`a2aproject/A2A`](https://github.com/a2aproject/A2A).

## Which agent this calls

- **Crate name:** `skill-dispatch-profile-agent`
- **Source:** `examples/skill-dispatch-profile-agent/`
- **Default port:** `3015`
- **AgentCard:** `http://localhost:3015/.well-known/agent-card.json`
- **Skills:** `echo_loud` (uppercases input), `reverse` (reverses input)

Start it:

```sh
cargo run -p skill-dispatch-profile-agent
```

## What this client demonstrates

A2A v1.0 has no normative "send this message to skill X" field on
`Message`. The Turul framework defines an opt-in extension that fills
that gap with a small wire convention:

| Layer | What the client does |
| --- | --- |
| HTTP request header | `A2A-Extensions: https://turul.dev/a2a/extensions/skill-invocation/v1` |
| Application payload | `Message.metadata["a2a.skillId"]` = `<skill-id>` |
| Application payload | `Message.metadata["a2a.skillParams"]` = `<input object>` |
| HTTP response header (server echo) | `A2A-Extensions: https://turul.dev/a2a/extensions/skill-invocation/v1` |

The client makes two calls in sequence:

1. `echo_loud` with `{"text": "hello"}`  → artifact `{"shouted":"HELLO"}`
2. `reverse`   with `{"text": "abc"}`   → artifact `{"reversed":"cba"}`

It also asserts that at least one response carried back the
`A2A-Extensions` header echoing the profile URI.

### How the header is attached

`a2a-sdk` 1.0.2 builds its `Client` over an `httpx.AsyncClient` passed
via `ClientConfig(httpx_client=...)`. We attach the profile URI as a
**default header** on that shared client, so every outbound request
(agent-card fetch, `SendStreamingMessage`, etc.) carries it:

```python
async with httpx.AsyncClient(
    timeout=30.0,
    headers={"A2A-Extensions": PROFILE_URI},
    event_hooks={"response": [capture_response]},
) as http:
    factory = ClientFactory(ClientConfig(httpx_client=http))
    client = factory.create(card)
```

The `event_hooks` callback captures response headers so we can verify
the server's echo.

### How `Message.metadata` is set

In `a2a-sdk` 1.0.2, `Message` is a protobuf-generated type and its
`metadata` field is a `google.protobuf.Struct`. We build it with
`google.protobuf.json_format.ParseDict`:

```python
from google.protobuf.json_format import ParseDict
from google.protobuf.struct_pb2 import Struct

md = Struct()
ParseDict({"a2a.skillId": "echo_loud", "a2a.skillParams": {"text": "hello"}}, md)
msg = Message(message_id=..., role=Role.ROLE_USER, parts=[Part(text="dispatch")], metadata=md)
```

## Run

In one terminal, start the agent:

```sh
cargo run -p skill-dispatch-profile-agent
```

In another terminal, run the client:

```sh
cd examples/interop-clients/skill-dispatch-profile/python
python -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
python main.py
```

To target a non-default URL:

```sh
A2A_BASE_URL=http://localhost:3015 python main.py
```

## Expected output (truncated)

```
=== AgentCard ===
  name        : Skill Dispatch Profile Agent
  version     : 0.1.0
  extensions  : ['https://turul.dev/a2a/extensions/skill-invocation/v1']

=== Call 1: echo_loud ===
  metadata    : a2a.skillId="echo_loud" a2a.skillParams={"text":"hello"}
  artifact    : {"shouted":"HELLO"}
  echoed hdr  : https://turul.dev/a2a/extensions/skill-invocation/v1

=== Call 2: reverse ===
  metadata    : a2a.skillId="reverse" a2a.skillParams={"text":"abc"}
  artifact    : {"reversed":"cba"}
  echoed hdr  : https://turul.dev/a2a/extensions/skill-invocation/v1

=== OK: both artifacts matched and A2A-Extensions echo observed ===
```

## Failure modes

| Symptom | Likely cause |
| --- | --- |
| `agent does not advertise <profile-uri>` | Wrong port, or the agent build is missing the extension declaration. |
| Task moves to `Failed` with "Skill dispatch requires Message.metadata..." | The `a2a.skillId` key did not reach the server. Verify the `Struct` was attached to `Message.metadata`. |
| `unknown skill id` | Typo in `a2a.skillId` — must be exactly `echo_loud` or `reverse`. |
| `inputSchema violation` | `a2a.skillParams` shape does not satisfy the skill's manifest schema. |
| `echoed hdr: None` for every call | The `A2A-Extensions` request header was not attached to the httpx client. |

## Transport used

JSON-RPC over `POST /jsonrpc`. The AgentCard advertises
`supportedInterfaces[0].protocolBinding=JSONRPC` and the Python SDK
selects that transport. Because the SDK sets
`Accept: text/event-stream`, the wire JSON-RPC `method` is
`SendStreamingMessage` (the Python call is `client.send_message(...)`).
The `A2A-Extensions` header rides on top of that transport — it is
orthogonal to the JSON-RPC envelope.

## Files

- `main.py` — the client (`asyncio` + `httpx` + `a2a-sdk`).
- `requirements.txt` — pinned `a2a-sdk==1.0.2` and `httpx`.
- `pyproject.toml` — PEP 621 metadata with the same pins.
- `.gitignore` — keeps `.venv/`, `__pycache__/`, etc. out of git.
