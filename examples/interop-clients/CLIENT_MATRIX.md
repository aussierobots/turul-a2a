# Cross-Language Interop Client Matrix

This directory contains **interop probe clients** — small, self-contained
external clients that call the example A2A agents in this workspace as if
they were third-party consumers. They are NOT framework client SDKs;
they are evidence that A2A's cross-language claim works in practice.

## Layout

```
examples/interop-clients/
├── CLIENT_MATRIX.md                          ← this file
├── <agent-name>/
│   ├── python/   ← Python client using a2a-sdk 1.0.2
│   ├── go/       ← Go client using a2aproject/a2a-go/v2 v2.3.1
│   └── rust/     ← Rust client using turul-a2a-client
```

## The matrix — agent × language × transport

Each cell records the **transport the SDK selected** and the **manual
smoke status**. The framework's `A2aServer` serves both REST and JSON-RPC
surfaces; each SDK picks based on the AgentCard's `supportedInterfaces[]`.

| Agent | Python (`a2a-sdk` 1.0.2) | Go (`a2aproject/a2a-go/v2` v2.3.1) | Rust (`turul-a2a-client`) |
|---|---|---|---|
| **`skill-manifest-ollama-agent`** (port 3010, offline mode) | ✓ JSON-RPC `/jsonrpc`, `SendStreamingMessage` (SSE) | ✓ JSON-RPC `/jsonrpc`, `SendMessage` | ✓ REST `/message:send` |
| **`agent-role-planner-router-agent`** (port 3012) | ✓ JSON-RPC `/jsonrpc`, `SendStreamingMessage` (SSE) | ✓ JSON-RPC `/jsonrpc`, `SendMessage` | ✓ REST `/message:send` |
| **`agent-role-critic-agent`** (port 3013) | ✓ JSON-RPC `/jsonrpc`, `SendStreamingMessage` (SSE) | ✓ JSON-RPC `/jsonrpc`, `SendMessage` | ✓ REST `/message:send` |
| **`post-task-hook-agent`** (port 3014) | ✓ JSON-RPC `/jsonrpc`, `SendStreamingMessage` (SSE) | ✓ JSON-RPC `/jsonrpc`, `SendMessage` | ✓ REST `/message:send` |
| **`skill-dispatch-profile-agent`** (port 3015) — exercises the ADR-022 dispatcher profile via `A2A-Extensions` header + `Message.metadata["a2a.skillId"]` | ✓ JSON-RPC `/jsonrpc` + `A2A-Extensions` header + metadata-keyed routing | ✓ JSON-RPC `/jsonrpc` + `A2A-Extensions` header + metadata-keyed routing | ✓ REST `/message:send` + `A2A-Extensions` header (via `A2aClient::with_extensions`) + metadata-keyed routing |

**15 cells, 15 manually verified.** Each ✓ corresponds to a smoke run
that started the agent on its default port, sent the agent's actual
payload, and observed the expected response shape end-to-end. All
four showcase agents are manifest-backed; the fifth
(`skill-dispatch-profile-agent`) additionally advertises the
skill-invocation profile extension URI in its AgentCard and
exercises the framework's `A2A-Extensions` header dispatch — all
three clients confirmed the URI is echoed in the response header
on every call. Evidence trail is in each client's README
"Expected output" section.

## Per-agent payloads

| Agent | What clients send | Expected response artifact |
|---|---|---|
| `skill-manifest-ollama-agent` | JSON text `{"user":{"name":"Ada"},"style":"formal"}` → skill `greet` | `greeting`: `{"greeting":"…"}` (offline stub or live Ollama response) |
| `agent-role-planner-router-agent` | `"add 3 5"` then `"concat: foo bar baz"` | `{"result":8}` then `{"joined":"foo bar baz"}` |
| `agent-role-critic-agent` | JSON `validate_against_schema` (value+schema) + `check_invariants` (value+invariants) | `{"valid":true,"errors":[]}` + `{"verdict":"pass","failures":[]}` |
| `post-task-hook-agent` | `"count 3"` three times then `"metrics"` | `{"squared":9}` thrice + `{"success":3,"failure":0,"last":"…"}` |

## Wire-contract notes

- **JSON casing:** camelCase throughout per proto-JSON mapping (`messageId`, `contextId`, `taskId`, `protocolBinding`).
- **Role enum:** proto-string form `"ROLE_USER"`, not lowercase `"user"`.
- **Discovery route:** all three clients fetch `GET /.well-known/agent-card.json` identically; differences appear only on the message-send hop.
- **Transport choice driver:** Python and Go select JSON-RPC because the example agents' AgentCard advertises `supportedInterfaces[0].protocolBinding = "JSONRPC"`. Rust uses REST because `turul-a2a-client` targets the proto-defined REST routes directly. Both surfaces are spec-valid in A2A v1.0; both are served by the same `A2aServer`.

## Live Ollama

NOT exercised by the matrix. Live mode is opt-in via `OLLAMA_BASE_URL` /
`RUN_OLLAMA_SMOKE=1` env vars and is documented in
`examples/skill-manifest-ollama-agent/README.md`. `cargo test --workspace`
stays hermetic.

## What this proves

The matrix is the framework's **cross-language interop evidence** required
by ADR-021 §7.3 step 8. It demonstrates that:

1. A Turul A2A agent's `AgentCard` is readable by independent third-party SDKs.
2. Both transport surfaces (REST + JSON-RPC) work against the same server.
3. The framework's wire shape matches the A2A v1.0 spec well enough that
   external SDKs round-trip messages without framework-side patches.

If a new example agent is added under `examples/`, this matrix should grow
a new row with at least one cell manually verified.
