# Python A2A interop client — `remote-delegate-agent`

Third-party Python client (uses `a2a-sdk` 1.0.2). Talks to the
delegate; the delegate then forwards to the upstream
`skill-manifest-ollama-agent`. **Two A2A hops** are exercised by a
single call.

## Run

In three separate terminals:

```bash
# 1. Upstream — the agent the delegate forwards to.
cargo run -p skill-manifest-ollama-agent
# binds :3010, offline-stub mode by default

# 2. Delegate — points at the upstream above.
cargo run -p remote-delegate-agent
# binds :3016, forwards to http://localhost:3010

# 3. This client.
cd examples/interop-clients/remote-delegate/python
python -m venv .venv && . .venv/bin/activate
pip install -r requirements.txt
python main.py
```

Override the delegate's URL via `A2A_BASE_URL=http://...:3016 python main.py`.

## Expected output

```
=== Delegate AgentCard (what THIS client sees) ===
  name        : Remote Delegate Agent
  version     : 0.1.0
  skills      : ['delegate']

=== Send (chain: client → delegate → upstream) ===
  payload     : {"user":{"name":"Ada"},"style":"formal"}
  artifact    : {"greeting":"Good day, Ada! (offline stub)"}

=== OK: two-hop chain returned the upstream's artifact ===
```

The "offline stub" marker in the artifact body **proves the chain
reached the upstream**: the client only knows about the delegate;
the upstream is invisible at the wire boundary. Yet the artifact
body is the upstream agent's offline-stub greeting, round-tripped
through the delegate's artifact re-emit (with a fresh local
`artifact_id` — the body is preserved verbatim).

## What this client demonstrates

- **A2A composes naturally over A2A.** The same `a2a-sdk` code that
  talks to a single-agent `skill-manifest-ollama-agent` talks to the
  delegate identically. The delegation is invisible at the wire.
- **AgentCard surface stays minimal.** The delegate advertises one
  skill (`delegate`), not the upstream's `greet`. Adopters who want
  to expose the *upstream's* skills through the delegate would set
  that up explicitly (out of scope for this example).
- **Artifact propagation is verbatim.** The delegate strips the
  upstream's `artifact_id` (it owns its own task lifecycle) but
  preserves every `Part`. Caller payloads (`Ada`, `formal`) survive
  the hop.

## What this client does NOT exercise

- Streaming (`/message:stream` SSE). The delegate's v1 advertises
  `streaming: false`; this client uses the buffered path.
- Auth forwarding. The delegate does not propagate caller credentials
  (confused-deputy avoidance). Adding an `Authorization` header to
  the client request changes nothing about what the delegate sends
  upstream.
- Error mapping. Happy path only. See the delegate's `tests/smoke.rs`
  + unit tests for the §4 error table.
