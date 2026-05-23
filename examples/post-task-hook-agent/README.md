# Post-Task Hook Agent (generic, no LLM)

A worked example of the **post-task observation seam** that fires after a
skill returns. Two manifest-backed skills, one `TerminalHook` impl, no LLM,
no external services.

## What is A2A?

A2A (Agent-to-Agent) is an open protocol for agents to discover each other,
exchange messages, and stream task progress. See
[a2a-protocol.org](https://a2a-protocol.org/) and the proto-first contract
in this repo at `proto/a2a.proto`.

## What this example demonstrates

The agent exposes two **manifest-backed** skills:

1. **`count`** — `skills/count/SKILL.md`
   - Input: `{"n": <integer>}`
   - Output: `{"squared": <integer>}`
   - Implementation: pure Rust (`n * n`). The SKILL.md drives both the
     advertised `AgentSkill` and the input/output JSON Schema validation.

2. **`metrics`** — `skills/metrics/SKILL.md`
   - Input: `{}` (no input)
   - Output: `{"success": <int>, "failure": <int>, "last": <string|null>}`
   - Implementation: reads a shared in-memory counter populated by the
     hook (below). Read-only; the skill does not mutate the counter.

Layered on top is a `TerminalHook` — the patterns crate trait that runs
*after* `SkillHandler::run` returns, with either
`SkillOutcome::Success(&Value)` or `SkillOutcome::Failure(&SkillError)`.

The example's `RecordingHook` increments `success` / `failure` and stores a
short summary string. The next inbound `metrics` call surfaces that
counter so a caller can observe the hook actually fired.

### Hook is not a skill

The hook is **not** a skill. It does not appear in the agent card. It does
not show up in `SkillRegistry::list()`. SKILL.md files describe what the
agent advertises; `TerminalHook` is an orthogonal observation seam from the
patterns crate (ADR-021 §9 Q5). Adopters use it for counters, structured
audit logs, metric export, or post-run cleanup — anything that is "fire and
forget" relative to the response that goes back to the caller.

`turul-a2a-patterns` provides the `TerminalHook` trait. This example
contributes one concrete impl (`RecordingHook`) in `src/main.rs`. The
trait IS the abstraction; the impl is replaceable.

## Architecture sketch

```text
┌────────┐    POST /message:send     ┌────────────────────────┐
│ Client │ ────────────────────────▶ │   A2A server / router  │
└────────┘                           └───────────┬────────────┘
                                                 │  Task + Message
                                                 ▼
                                     ┌────────────────────────┐
                                     │   HookAgent            │
                                     │   (AgentExecutor impl) │
                                     └───────────┬────────────┘
                                                 │
                                                 ▼
                              ┌────────────────────────────────┐
                              │  plan(text) → (skill_id,       │
                              │  params)                       │
                              │    "count <n>"  → count        │
                              │    "metrics"    → metrics      │
                              │    other        → count(err)   │
                              └───────────┬────────────────────┘
                                          │
                                          ▼
              ┌──────────────────────────────────────────────────────┐
              │ InMemorySkillRegistry (manifest-backed)              │
              │   ├── count    ← skills/count/SKILL.md               │
              │   └── metrics  ← skills/metrics/SKILL.md             │
              └───────────┬──────────────────────────────────────────┘
                          │ handler.run(params, sink)
                          ▼
                 ┌────────────────────────┐
                 │ Returns Result<Value,  │
                 │ SkillError>            │
                 └───────────┬────────────┘
                             │
                             ▼
              ┌─────────────────────────────────────────┐
              │ hook.on_terminal(skill_id, outcome)     │
              │   → RecordingHook updates counter       │  (orthogonal
              │   → no influence on response            │   to skills)
              └───────────┬─────────────────────────────┘
                          │
                          ▼
                  ctx.events.emit_artifact(...)
                          │
                          ▼
              Artifact in Task response

       Later call: "metrics" → MetricsSkill reads same counter
                          → returns { success, failure, last }
```

## Why the hook pattern stays code-first

The skills are manifest-backed (SKILL.md) so they are discoverable and
schema-validated. The **hook** stays in Rust because the patterns crate's
`TerminalHook` trait IS the abstraction — there is nothing for a manifest
to describe about an observation seam beyond "this fires after `run`
returns". An adopter who wants different observation semantics writes a
different `TerminalHook` impl; an adopter who wants different skills edits
the manifest. Those two surfaces are deliberately separate.

For the same reason, framework-side timeout / panic isolation around the
hook is **not** part of this example or the patterns crate today. ADR-021
§9 Q5 ships the minimal generic variant and parks the deeper isolation
discussion in ADR-022.

## Hook safety

`TerminalHook` is best-effort by contract — the patterns crate documents
that hook failures or hangs MUST NOT abort the surrounding execution. The
patterns crate itself **does not** wrap the hook in a timeout or
panic-isolated task; that is left to the adopter. This example awaits the
hook future inline, which is the simplest case (and is fine for cheap
in-memory work).

If a real adopter has hooks that may block or panic, two common patterns:

```rust
// Bound runtime — drop slow hooks rather than stalling the request.
let _ = tokio::time::timeout(
    std::time::Duration::from_millis(50),
    hook.on_terminal(skill_id, outcome),
).await;

// Or fire-and-forget — the response returns immediately, hook runs in
// the background. Note: panics inside `spawn` are caught by tokio, not
// propagated to the caller.
let h = hook.clone();
let id = skill_id.to_string();
tokio::spawn(async move { h.on_terminal(&id, owned_outcome).await; });
```

The `tokio::spawn` shape needs `'static` lifetimes, so the outcome must be
cloned/owned before the spawn — that's a real design constraint the simple
inline shape does not have.

## Prerequisites

- Rust toolchain (workspace's `rust-version`, see root `Cargo.toml`).
- No external services. No environment variables required.

## How to run the agent

```bash
cargo run -p post-task-hook-agent
```

The agent binds to `0.0.0.0:3014` by default (override with `A2A_PORT`).

## Expected request and response shape

The agent accepts plain-text messages and maps them to a skill via the
planner. The wire envelope is the standard A2A `message:send` shape; the
`parts[0].text` field carries the trigger text.

### `count` (success)

Send `"count 3"`:

```bash
curl -s -X POST http://localhost:3014/message:send \
  -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"count 3"}]}}' \
  | jq '.task.artifacts[0].parts[0].text | fromjson'
# { "squared": 9 }
```

Internally the planner sees `"count <n>"`, parses `3` as an integer, and
builds `{"n": 3}`. The handler validates against the input schema, returns
`{"squared": 9}`, and the response artifact carries that JSON verbatim.
The `TerminalHook` fires with `SkillOutcome::Success(&{"squared": 9})` and
the counter's `success` field bumps by 1.

### `count` (failure)

Send `"count three"` — `"three"` does not parse as an integer, so the
planner forwards `{"n": "three"}`. The input schema rejects it as a string
instead of an integer; the handler returns `SkillError::InvalidRequest`,
which the executor surfaces as a `Task` in `TASK_STATE_FAILED` with the
validator message in `status.message.parts[0].text`:

```bash
curl -s -X POST http://localhost:3014/message:send \
  -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"err","role":"ROLE_USER","parts":[{"text":"count three"}]}}' \
  | jq '.task | {state: .status.state, error: .status.message.parts[0].text}'
# {
#   "state": "TASK_STATE_FAILED",
#   "error": "executor error: Invalid request: inputSchema violation: schema validation failed at /n: \"three\" is not of type \"integer\""
# }
```

The hook fires with `SkillOutcome::Failure(&err)` and the counter's
`failure` field bumps by 1. The hook does not change the response — the
caller still sees the failed task with its error message.

### `metrics`

Send `"metrics"`:

```bash
curl -s -X POST http://localhost:3014/message:send \
  -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"m","role":"ROLE_USER","parts":[{"text":"metrics"}]}}' \
  | jq '.task.artifacts[0].parts[0].text | fromjson'
# { "success": 1, "failure": 1, "last": "err(count): ..." }
```

After three successful `count` calls and zero failures:

```json
{ "success": 3, "failure": 0, "last": "ok(count): {\"squared\":9}" }
```

## How to run each interop client

The repo ships three reference clients exercising the same wire contract.
Each sends `"count 3"` three times then `"metrics"` and prints the
artifact bodies.

### Rust

```bash
# terminal 1
cargo run -p post-task-hook-agent
# terminal 2
cargo run -p interop-post-task-hook-rust
```

### Python

```bash
cd examples/interop-clients/post-task-hook/python
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
python main.py
```

### Go

```bash
cd examples/interop-clients/post-task-hook/go
go run ./cmd/probe
```

All three send the same payloads and print the resulting artifact bodies.

## Offline vs live

This example is fully **offline**. No outbound HTTP, no LLM provider, no
auth dependency. The smoke test is hermetic.

## Failure modes

- **Non-integer `n`** — schema validation fails inside the handler;
  `SkillError::InvalidRequest` flows back as an A2A error. The hook still
  fires with `Failure`, so the `metrics` counter records it.
- **Bind-address already in use** — change `A2A_PORT`.
- **Concurrent counter reads under heavy load** — `AtomicU64` is
  `Relaxed`, so reads may observe slightly stale values. That is fine for
  a demo counter and is documented as a known trade-off.
- **Manifest parse error at startup** — the embedded `SKILL.md` files must
  round-trip through `SkillCard::parse`. The build-time embedded
  manifests run a parse test (`cargo test -p post-task-hook-agent`) so a
  regression is caught before the binary ships.

## Troubleshooting

- `curl: (7) Failed to connect` — the agent has not started yet, or
  `A2A_PORT` is set to a different value than the one you're hitting.
- `agent did not become ready` in the smoke test — increase the wait
  budget or check that nothing else is bound to port 38014.
- `metrics` keeps returning zeros — confirm you sent a `count <n>` call
  first; the counter only moves when the hook observes an outcome.

## Smoke test

```bash
cargo test -p post-task-hook-agent --tests
```

The smoke test spawns the binary on isolated ports (38014-38016) and walks
the success, failure, and repeated-call paths end-to-end. Hermetic — no
network access.

## References

- [ADR-021 §9 Q5](../../docs/adr/ADR-021-turul-a2a-patterns-extraction.md) — TerminalHook minimal-generic variant (no extractor-registry, no framework-side isolation in v1).
- [ADR-021 §2.2 item 3](../../docs/adr/ADR-021-turul-a2a-patterns-extraction.md) — SKILL.md manifest authoring shape.
- [ADR-022](../../docs/adr/ADR-022-skill-invocation-dispatcher-profile.md) — Skill-invocation dispatcher profile; owns the deeper isolation / extractor-registry questions parked from ADR-021.
- `crates/turul-a2a-patterns/src/hook.rs` — the `TerminalHook` /
  `SkillOutcome` trait surface.
- `examples/agent-role-planner-router-agent` — sibling example with the
  same `SkillRegistry`/`SkillHandler` shape, without a hook.
