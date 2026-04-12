# A2A Python SDK Interoperability Tests

Tests the official A2A Python SDK client (`a2a-sdk`) against the turul-a2a Rust server.

## Prerequisites

- Python 3.10+
- [uv](https://docs.astral.sh/uv/)
- A running turul-a2a server (e.g., `cargo run -p echo-agent`)

## Setup

```bash
cd interop/python-sdk
uv sync
```

## Run

Start the Rust server first:

```bash
# Terminal 1: start the echo agent
cargo run -p echo-agent
```

```bash
# Terminal 2: run the Python interop tests
cd interop/python-sdk
TURUL_SERVER_URL=http://localhost:3000 uv run pytest -v
```

## What's tested

**JSON-RPC wire format interop** (httpx — 6 tests):
- Agent card discovery (`GET /.well-known/agent-card.json`)
- Send message via JSON-RPC (`SendMessage`)
- Get task via JSON-RPC (`GetTask`)
- Cancel task via JSON-RPC (`CancelTask`)
- List tasks via JSON-RPC
- Error handling (not found, not cancelable)

**SDK compatibility probe** (a2a-sdk — 1 test, currently expected to skip):
- `A2ACardResolver` agent card fetch and validation
- Skips because SDK v0.3 AgentCard model doesn't match v1.0 proto (see below)

## Version Alignment

**Installed**: `a2a-sdk==0.3.26` (latest PyPI, implements A2A spec v0.3)
**Our server**: proto package `lf.a2a.v1` (matches upstream tag `v1.0.0`)

The Python SDK and our server target **different spec versions**. Specific mismatches:

| Dimension | Our Server (v1.0 proto) | Python SDK (v0.3) |
|-----------|------------------------|-------------------|
| AgentCard URL | `supported_interfaces[].url` | Top-level `url` (required) |
| REST routes | `/message:send` | `/v1/message:send` |
| Data models | Proto-generated (prost+pbjson) | Hand-written Pydantic |

This is ecosystem version drift, not a server bug. The SDK compatibility probe
test will start passing when the Python SDK ships v1.0-aligned models — no
changes needed on our side.

## Transport

The JSON-RPC tests use raw `httpx` POST to `/jsonrpc` because the SDK's client
transport layer doesn't align with our v1.0 routes. JSON-RPC is the common
wire format both agree on at the request/response level.
