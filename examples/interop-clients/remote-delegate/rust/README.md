# Rust A2A interop client — `remote-delegate-agent`

Third-party Rust client built on `turul-a2a-client`. Talks to the
delegate; the delegate forwards to the upstream
`skill-manifest-ollama-agent`. Two A2A hops; the upstream is
invisible at this client's wire boundary.

## Run

```bash
cargo run -p skill-manifest-ollama-agent          # upstream :3010
cargo run -p remote-delegate-agent                # delegate :3016
cargo run -p interop-remote-delegate-rust         # this client
```

Override target via `A2A_BASE_URL=http://...:3016 cargo run ...`.

## Expected output

```
target: http://localhost:3016
delegate AgentCard: Remote Delegate Agent v0.1.0 (skills=["delegate"])
--- Send (chain: client → delegate → upstream) ---
payload: {"style":"formal","user":{"name":"Ada"}}
task: id=… state=Completed artifacts=1
artifact: {"greeting":"Good day, Ada! (offline stub)"}
=== OK: two-hop chain returned the upstream's artifact ===
```

The `offline stub` marker proves the chain reached the upstream
agent's offline-mode greeting handler. This client only ever spoke
to the delegate.

## Wire path

- Uses `turul-a2a-client`'s REST surface (`/message:send`).
- The delegate's AgentCard advertises `streaming: false`; the response
  is a buffered Task with all artifacts attached.
- The artifact body propagates verbatim from upstream; the delegate
  rewraps with a fresh local `artifact_id`.

## What this client does NOT exercise

Streaming, auth forwarding, error mapping. See
`remote-delegate-agent/README.md` for the full delegate contract;
this is a happy-path interop probe.
