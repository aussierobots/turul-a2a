# Go A2A interop client — `remote-delegate-agent`

Third-party Go client built on `a2aproject/a2a-go/v2` v2.3.1. Talks
to the delegate, which forwards to the upstream
`skill-manifest-ollama-agent`. Two A2A hops; the upstream is
invisible from this client's wire view.

## Run

```bash
# 1. Upstream
cargo run -p skill-manifest-ollama-agent
# binds :3010

# 2. Delegate
cargo run -p remote-delegate-agent
# binds :3016 → forwards to :3010

# 3. This client
cd examples/interop-clients/remote-delegate/go
go build -o probe ./cmd/probe
./probe
```

Override target via `A2A_BASE_URL=http://...:3016 ./probe`.

## Expected output

```
a2a-go protocol version: 1.0
target: http://localhost:3016
--- Delegate AgentCard (what THIS client sees) ---
Name:    Remote Delegate Agent
Version: 0.1.0
Skills:  delegate
--- Send (chain: client → delegate → upstream) ---
payload: {"user":{"name":"Ada"},"style":"formal"}
artifact: {"greeting":"Good day, Ada! (offline stub)"}
=== OK: two-hop chain returned the upstream's artifact ===
```

The `offline stub` marker proves the chain reached the upstream
agent's offline-mode greeting handler — this client only ever spoke
to the delegate.

## Wire path

- Delegate AgentCard advertises `supportedInterfaces[0].protocolBinding=JSONRPC`.
- The SDK selects JSON-RPC and POSTs to `/jsonrpc`.
- `SendMessage` returns a buffered Task (delegate advertises
  `streaming: false`; v1 of the delegate does not pass SSE through).
- Artifact body is the upstream's response, re-wrapped with a fresh
  local `artifact_id` by the delegate.

## What this client does NOT exercise

Streaming, auth forwarding, error mapping. See `remote-delegate-agent/README.md`
for the full contract; this is a happy-path interop probe.
