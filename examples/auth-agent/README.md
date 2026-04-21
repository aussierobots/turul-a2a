# auth-agent

Like `echo-agent`, but gated by an API-key middleware. Demonstrates
`A2aMiddleware` wiring, the `AuthIdentity` surfaced via `ExecutionContext`,
and automatic population of the agent card's `securitySchemes`.

## Run

```bash
cargo run -p auth-agent
# listens on http://0.0.0.0:3001
```

Configured demo keys (see `src/main.rs`):

| Key              | Owner   |
| ---------------- | ------- |
| `demo-key-alice` | `alice` |
| `demo-key-bob`   | `bob`   |

## Try it

```bash
# Agent card is public — auth schemes are auto-populated from middleware
curl -s http://localhost:3001/.well-known/agent-card.json | jq .securitySchemes

# No auth → 401
curl -i http://localhost:3001/message:send \
  -H 'Content-Type: application/json' \
  -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"hi"}]}}'

# With auth → 200, executor echoes caller identity
curl -s http://localhost:3001/message:send \
  -H 'Content-Type: application/json' \
  -H 'a2a-version: 1.0' \
  -H 'X-API-Key: demo-key-alice' \
  -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"hi"}]}}'
```

## What it shows

- `ApiKeyMiddleware` from `turul-a2a-auth` with a `StaticApiKeyLookup`.
- Reading the authenticated identity via `ExecutionContext::owner` /
  `ExecutionContext::tenant` inside the executor.
- Agent cards remain publicly fetchable even when the rest of the API requires
  auth — per spec §5.

## See also

- ADR-007 (Auth middleware) for design rationale.
- `turul-a2a-auth` crate for `BearerMiddleware` (JWT) as an alternative.
