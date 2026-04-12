# Go SDK Interoperability Tests

Tests the official Go A2A SDK client (`a2aproject/a2a-go v2`) against the turul-a2a Rust server. This is the **primary external interoperability path** — both implementations target A2A spec v1.0.

## Prerequisites

- Go 1.24+ (`brew install go`)
- Running turul-a2a server (e.g., `cargo run -p echo-agent`)

## Run

```bash
# Terminal 1: start the echo agent
cargo run -p echo-agent

# Terminal 2: run Go interop tests
cd interop/go-sdk
TURUL_SERVER_URL=http://localhost:3000 go test -v
```

## What's tested

All tests use the official Go SDK client via `a2aclient.NewFromCard()` — real SDK interop, not raw HTTP.

| Test | SDK Method |
|------|-----------|
| Discover agent card | `agentcard.DefaultResolver.Resolve()` |
| Send message | `client.SendMessage()` |
| Get task | `client.GetTask()` |
| Cancel completed task (error) | `client.CancelTask()` |
| List tasks | `client.ListTasks()` |
| Get task not found (error) | `client.GetTask()` |

## Transport

The Go SDK selects transport based on `protocolBinding` from the agent card's `supportedInterfaces`. Our echo agent declares `JSONRPC`, so the SDK uses its JSON-RPC transport — posting JSON-RPC 2.0 payloads directly to the interface URL.

## Version Alignment

| Dimension | Our Server | Go SDK v2.2.0 |
|-----------|-----------|---------------|
| Spec version | v1.0 (proto `lf.a2a.v1`) | v1.0 |
| AgentCard model | `supported_interfaces[].url` | `SupportedInterfaces[].URL` |
| REST routes | `/message:send` (unprefixed) | `/message:send` (unprefixed) |
| JSON-RPC | `POST /jsonrpc` | `POST {interface.url}` |
| Protocol version | `1.0` | `1.0` |

Full alignment. No workarounds needed.
