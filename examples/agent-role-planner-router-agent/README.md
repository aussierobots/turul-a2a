# Planner-Router Agent (generic, no LLM)

A worked example of two agent **role patterns** working together — **planner**
and **router** — implemented with deterministic Rust. No LLM, no Ollama, no
external services.

If you are new to A2A, start at the
[A2A specification](https://github.com/a2aproject/A2A); the proto-first
contract this repo implements lives in `proto/a2a.proto`.

## What this example demonstrates

The agent shows the **planner + router** idiom in its simplest possible form:

1. **Two manifest-backed skills** registered in an `InMemorySkillRegistry`
   via `SkillCard::parse` + `SkillRegistry::register_manifest`:
   - `add` — input `{a, b}`, output `{result}` — defined by
     `skills/add/SKILL.md`.
   - `concat` — input `{strings: [...]}`, output `{joined}` — defined by
     `skills/concat/SKILL.md`.
2. **A code-first planner** — a small Rust rules table inspects the inbound
   text and chooses one skill plus its parameters.
3. **A router** — the `AgentExecutor` calls `registry.handler(skill_id)` and
   runs the returned `SkillHandler`, bridging the framework `EventSink`
   through a local newtype that implements `SkillProgressSink`.
4. **Schema-validated dispatch** — each handler validates the planner's
   parameters against the manifest's `inputSchema` before doing work, and
   validates its output against `outputSchema` before emitting an artifact.

The wire shape is identical to the previous programmatic version: the same
`message:send` requests, the same `{"result": 8}` / `{"joined": "foo bar
baz"}` artifacts. The change is internal — the registry is now driven by
SKILL.md, not by inline Rust literals.

## Architecture sketch

```
                                         ┌─────────────────────────┐
                                         │ skills/add/SKILL.md     │
                                         │ skills/concat/SKILL.md  │
                                         └────────────┬────────────┘
                                                      │ SkillCard::parse
                                                      ▼
   client ──── POST /message:send ──▶ A2A agent (port 3012)
                                            │
                                            ▼
                                      ┌───────────┐
                                      │  planner  │  (Rust rules table)
                                      │ (code)    │  text ─▶ (skill_id, params)
                                      └─────┬─────┘
                                            ▼
                                      ┌───────────────┐
                                      │ SkillRegistry │   ◀── manifest-backed
                                      │  .handler(id) │
                                      └─────┬─────────┘
                                            ▼
                                      ┌───────────────┐
                                      │ SkillHandler  │  validate_input →
                                      │  .run(params) │  body →
                                      │               │  validate_output
                                      └─────┬─────────┘
                                            ▼
                                      Artifact emitted, task COMPLETED
```

## Prerequisites

- Rust toolchain (workspace's `rust-version`, see root `Cargo.toml`).
- No Ollama, no other agents, no databases — this example is fully offline.

## How to run the agent

```bash
cargo run -p agent-role-planner-router-agent
```

The agent binds to `0.0.0.0:3012` by default (override with `A2A_PORT`).

Inspect the agent card:

```bash
curl -s http://localhost:3012/.well-known/agent-card.json | jq '.skills[].id'
# "add"
# "concat"
```

## How to run each interop client

Three reference clients live under `examples/interop-clients/agent-role-planner-router/`,
one per SDK language. Each sends `"add 3 5"` then `"concat: foo bar baz"`.

### Rust

```bash
cargo run -p interop-agent-role-planner-router-rust
```

See [`examples/interop-clients/agent-role-planner-router/rust/`](../interop-clients/agent-role-planner-router/rust/).

### Python

```bash
cd examples/interop-clients/agent-role-planner-router/python
python -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
python main.py
deactivate
rm -rf .venv
```

See [`examples/interop-clients/agent-role-planner-router/python/`](../interop-clients/agent-role-planner-router/python/).

### Go

```bash
cd examples/interop-clients/agent-role-planner-router/go
go run ./cmd/probe
```

See [`examples/interop-clients/agent-role-planner-router/go/`](../interop-clients/agent-role-planner-router/go/).

## Expected request and response shape

### `add` skill

Request:

```json
{
  "message": {
    "messageId": "1",
    "role": "ROLE_USER",
    "parts": [{ "text": "add 3 5" }]
  }
}
```

Response artifact text (decoded JSON):

```json
{ "result": 8 }
```

### `concat` skill

Request:

```json
{
  "message": {
    "messageId": "2",
    "role": "ROLE_USER",
    "parts": [{ "text": "concat: foo bar baz" }]
  }
}
```

Response artifact text (decoded JSON):

```json
{ "joined": "foo bar baz" }
```

The planner's full rules table:

| Inbound text pattern             | Skill    | Params shape                          |
| -------------------------------- | -------- | ------------------------------------- |
| leading `add <a> <b>`            | `add`    | `{"a": <a>, "b": <b>}`                |
| infix `<a> + <b>` anywhere       | `add`    | `{"a": <a>, "b": <b>}`                |
| leading `concat:` or `join:`     | `concat` | `{"strings": [...split on ws...]}`    |
| (anything else)                  | `concat` | `{"strings": ["unrecognized:", txt]}` |

## Offline vs live requirements

This example is **fully offline**. There are no external dependencies — no
Ollama, no databases, no other agents, no DNS lookups. The two skills are
deterministic Rust functions; the SKILL.md manifests describe their schemas
and discovery metadata but contain no LLM prompts. `cargo test` is hermetic
and can run without network access.

## Why the planner stays code-first

Skills have an input/output JSON schema and a body that operates on inbound
parameters. The planner has neither: its input is **raw user text** and its
output is a **routing decision** (`(skill_id, params)`), not a structured
result the agent ships on the wire. SKILL.md is the wrong shape for the
planner — it would have no meaningful `inputSchema` or `outputSchema`.

Manifest backing is orthogonal to whether a component is the planner or a
dispatch target. In this example the planner remains code-first while every
dispatch target (skill) is manifest-backed. A different example could ship a
manifest-backed *advisor* skill that an LLM-powered planner consults — that
would still be manifest-backed dispatch, just with the planner role using a
skill rather than a code block.

## Troubleshooting

- **`Address already in use` on startup** — set `A2A_PORT` to an unused port
  (e.g. `A2A_PORT=3112 cargo run -p agent-role-planner-router-agent`) and
  update `A2A_BASE_URL` for the clients to match.
- **Planner does not match an input** — the default branch routes to
  `concat` with `["unrecognized:", "<text>"]` rather than erroring, so the
  agent always produces a structured artifact. Inspect the artifact text to
  confirm which rule matched.
- **Schema validation failure** — if you call the agent with custom JSON
  payloads instead of the planner-friendly text shapes above, the handler
  validates parameters against `inputSchema`. A failure surfaces as
  `SkillError::InvalidRequest` → A2A `InvalidRequest`. Check the manifest in
  `skills/<id>/SKILL.md` for the required fields.
- **`include_str!` panics at build time** — the SKILL.md paths are relative
  to `src/`. If you move the `skills/` directory, update the
  `include_str!("../skills/<id>/SKILL.md")` lines in `src/main.rs` to match.

## Smoke test

```bash
cargo test -p agent-role-planner-router-agent --tests
```

The smoke test spawns the binary on an isolated port and walks the two
primary planner paths end-to-end. Hermetic — no network access.

## References

- [ADR-021 §9 Q3](../../docs/adr/ADR-021-turul-a2a-patterns-extraction.md) —
  agent role idioms (planner, router, critic, …) ship as examples, not as
  framework types, until a generic interface emerges.
- [ADR-021 §2.2 item 3](../../docs/adr/ADR-021-turul-a2a-patterns-extraction.md) —
  SKILL.md as the manifest format consumed by `SkillCard::parse` and
  `SkillRegistry::register_manifest`.
- `examples/skill-manifest-ollama-agent` — companion example that drives a
  single manifest-backed skill with an optional Ollama dispatch path. The
  same `SkillRegistry`/`SkillHandler` shape is used here for two
  deterministic skills.
