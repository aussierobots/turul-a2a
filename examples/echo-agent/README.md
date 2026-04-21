# echo-agent

Minimal A2A v1.0 agent that echoes the caller's text parts back as an artifact.
The simplest possible demonstration of `AgentExecutor` + `A2aServer::builder()`.

## Run

```bash
cargo run -p echo-agent
# listens on http://0.0.0.0:3000
```

## Try it

```bash
# Agent card (public, no auth)
curl -s http://localhost:3000/.well-known/agent-card.json | jq .

# Send a message
curl -s http://localhost:3000/message:send \
  -H 'Content-Type: application/json' \
  -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"hello"}]}}'
```

## What it shows

- Implementing the `AgentExecutor` trait (`execute` + `agent_card`).
- Building an `AgentCard` with `AgentCardBuilder` / `AgentSkillBuilder`.
- Pushing a text artifact and transitioning the task to completed via
  `Task::push_text_artifact` + `Task::complete`.
- Default in-memory storage — fine for local demos, **not** for multi-instance
  deployments (see `CLAUDE.md` §"Multi-Instance Streaming Limitation").

## See also

- `examples/auth-agent` — same executor shape, plus API-key middleware.
- `examples/lambda-agent` — same executor running on AWS Lambda.
