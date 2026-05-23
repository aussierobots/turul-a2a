# Critic Agent (generic, no LLM)

A worked example of the **critic / evaluator** agent role pattern with
deterministic Rust. No LLM, no Ollama, no external services.

## What is A2A?

A2A (Agent-to-Agent) is an open protocol for agents to discover each other,
exchange messages, and stream task progress. See
[a2a-protocol.org](https://a2a-protocol.org/) and the proto-first contract
in this repo at `proto/a2a.proto`.

## What this example demonstrates

The agent shows the **critic** idiom in its simplest possible form: given an
output and some criteria, return a verdict plus the reasons it passed or
failed. Two **manifest-backed** skills are registered:

1. **`validate_against_schema`** — `skills/validate_against_schema/SKILL.md`
   - Input: `{"value": <any>, "schema": <JSON Schema 2020-12>}`
   - Output: `{"valid": bool, "errors": [string]}`
   - Implementation: calls `turul_a2a_patterns::validate_json` and wraps the
     result. On `Ok` ⇒ `{"valid": true, "errors": []}`; on `Err` ⇒
     `{"valid": false, "errors": ["<first violation>"]}`.

2. **`check_invariants`** — `skills/check_invariants/SKILL.md`
   - Input: `{"value": <any>, "invariants": [{"name": str, "check": str, "args"?: <any>}]}`
   - Output: `{"verdict": "pass" | "fail", "failures": [{"name": str, "reason": str}]}`
   - Implementation: pure Rust over four invariant kinds.

   | `check`        | Applies to        | `args` shape        | Passes when                      |
   | -------------- | ----------------- | ------------------- | -------------------------------- |
   | `non_empty`    | any value         | `null` (ignored)    | value is not null / "" / [] / {} |
   | `min_length`   | string or array   | `{"min": <number>}` | `len(value) >= min`              |
   | `max_length`   | string or array   | `{"max": <number>}` | `len(value) <= max`              |
   | `contains`     | string or array   | `{"needle": <any>}` | substring / element membership   |

Both manifests carry their own input/output JSON Schemas; the registry
treats those as the single source of truth for the published `AgentSkill`
projection and the runtime params schema (ADR-021 §2.2 items 3 + 4).

## Architecture sketch

```text
┌────────┐    POST /message:send     ┌────────────────────────┐
│ Client │ ────────────────────────▶ │   A2A server / router  │
└────────┘                           └───────────┬────────────┘
                                                 │  Task + Message
                                                 ▼
                                     ┌────────────────────────┐
                                     │   CriticExecutor       │
                                     │   (AgentExecutor impl) │
                                     └───────────┬────────────┘
                                                 │
                                                 ▼
                              ┌────────────────────────────────┐
                              │  dispatch(text) → (skill_id,   │
                              │  params) text → JSON → kind →  │
                              │  validate_against_schema /     │
                              │  check_invariants /            │
                              │  fallback non_empty            │
                              └───────────┬────────────────────┘
                                          │
                                          ▼
              ┌──────────────────────────────────────────────────────┐
              │ InMemorySkillRegistry (manifest-backed)              │
              │   ├── validate_against_schema  ← SKILL.md            │
              │   └── check_invariants         ← SKILL.md            │
              └───────────┬──────────────────────────────────────────┘
                          │ SkillHandler.run(params, sink)
                          ▼
                 ┌────────────────────────┐
                 │ Deterministic Rust:    │
                 │ validate_json /        │
                 │ run_invariant table    │
                 └───────────┬────────────┘
                             │ structured JSON output
                             ▼
                  ctx.events.emit_artifact(...)
                             │
                             ▼
                  Artifact in Task response
```

## Why the critic role idiom stays code-first

Both skills are manifest-backed for discovery, schema, and routing — the
SKILL.md is the source of truth for what the agent advertises and what
inputs it accepts. The *deterministic verdict logic itself* stays in Rust
because every meaningful invariant (non-empty, length bounds, substring /
membership, JSON Schema 2020-12) is more concisely expressed as code than
as a prompt. There is nothing for an LLM to add to a `len(s) >= 5` check.

This split matches the planner-router sibling example: the framework
provides the manifest plumbing (`SkillCard::parse`, `register_manifest`,
the registry / handler / sink contracts), and the example contributes the
domain-specific *evaluator semantics* that an adopter would normally
customise. The patterns crate stays neutral on which checks an adopter
cares about. See
[ADR-021 §9 Q3](../../docs/adr/ADR-021-turul-a2a-patterns-extraction.md)
and [ADR-021 §2.2 item 3](../../docs/adr/ADR-021-turul-a2a-patterns-extraction.md).

## Why deterministic critics matter

Before reaching for "LLM as judge", many skill outputs can be critiqued by
rule: required fields present, lengths within bounds, enums valid, banned
substrings absent. Deterministic critics are cheap, reproducible, and
trivially testable. They make a good baseline before any LLM-backed
evaluator layers on top.

## Prerequisites

- Rust toolchain (workspace's `rust-version`, see root `Cargo.toml`).

## How to run the agent

```bash
cargo run -p agent-role-critic-agent
```

The agent binds to `0.0.0.0:3013` by default (override with `A2A_PORT`).

## Expected request and response shape

### `validate_against_schema`

Inbound message text is JSON:

```json
{"kind":"validate_against_schema","value":<any>,"schema":<JSON Schema 2020-12>}
```

Response artifact text (JSON):

```json
{"valid": <bool>, "errors": [<violation strings>]}
```

### `check_invariants`

Inbound message text is JSON:

```json
{
  "kind": "check_invariants",
  "value": <any>,
  "invariants": [{"name": <string>, "check": <kind>, "args": <object>}]
}
```

Response artifact text (JSON):

```json
{"verdict": "pass" | "fail", "failures": [{"name": <string>, "reason": <string>}]}
```

## Try it (curl)

Inspect the agent card:

```bash
curl -s http://localhost:3013/.well-known/agent-card.json | jq '.skills[].id'
# "validate_against_schema"
# "check_invariants"
```

### Validate a value against a schema (passing)

```bash
curl -s -X POST http://localhost:3013/message:send \
  -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"{\"kind\":\"validate_against_schema\",\"value\":{\"name\":\"ok\",\"count\":3},\"schema\":{\"type\":\"object\",\"properties\":{\"name\":{\"type\":\"string\"},\"count\":{\"type\":\"integer\"}},\"required\":[\"name\",\"count\"]}}"}]}}' \
  | jq '.task.artifacts[0].parts[0].text | fromjson'
# { "valid": true, "errors": [] }
```

### Validate a value against a schema (failing)

```bash
curl -s -X POST http://localhost:3013/message:send \
  -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"2","role":"ROLE_USER","parts":[{"text":"{\"kind\":\"validate_against_schema\",\"value\":{\"name\":\"missing\"},\"schema\":{\"type\":\"object\",\"required\":[\"name\",\"count\"]}}"}]}}' \
  | jq '.task.artifacts[0].parts[0].text | fromjson'
# {
#   "valid": false,
#   "errors": ["schema validation failed at #: \"count\" is a required property"]
# }
```

### Check invariants (all pass)

```bash
curl -s -X POST http://localhost:3013/message:send \
  -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"3","role":"ROLE_USER","parts":[{"text":"{\"kind\":\"check_invariants\",\"value\":[\"foo\",\"bar\",\"baz\"],\"invariants\":[{\"name\":\"is_non_empty\",\"check\":\"non_empty\"},{\"name\":\"min_3\",\"check\":\"min_length\",\"args\":{\"min\":3}},{\"name\":\"max_5\",\"check\":\"max_length\",\"args\":{\"max\":5}},{\"name\":\"has_bar\",\"check\":\"contains\",\"args\":{\"needle\":\"bar\"}}]}"}]}}' \
  | jq '.task.artifacts[0].parts[0].text | fromjson'
# { "verdict": "pass", "failures": [] }
```

### Check invariants (one fails)

```bash
curl -s -X POST http://localhost:3013/message:send \
  -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"4","role":"ROLE_USER","parts":[{"text":"{\"kind\":\"check_invariants\",\"value\":\"hi\",\"invariants\":[{\"name\":\"is_non_empty\",\"check\":\"non_empty\"},{\"name\":\"min_5\",\"check\":\"min_length\",\"args\":{\"min\":5}}]}"}]}}' \
  | jq '.task.artifacts[0].parts[0].text | fromjson'
# {
#   "verdict": "fail",
#   "failures": [{"name": "min_5", "reason": "length 2 is below minimum 5"}]
# }
```

## How to run each interop client

The repo ships three reference clients exercising the same wire contract.

### Rust

```bash
# terminal 1
cargo run -p agent-role-critic-agent
# terminal 2
cargo run -p interop-agent-role-critic-rust
```

### Python

```bash
cd examples/interop-clients/agent-role-critic/python
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
python main.py
```

### Go

```bash
cd examples/interop-clients/agent-role-critic/go
go run ./cmd/probe
```

All three send the same JSON-typed messages and print the resulting
artifact bodies.

## Offline vs live

This example is fully **offline**. No outbound HTTP, no LLM provider, no
auth dependency. The smoke test is hermetic.

## Failure modes

- **Missing `value` or `schema` / `invariants`** — the handler returns
  `SkillError::InvalidRequest`, mapped to A2A `InvalidRequest`.
- **Unknown invariant `check` kind** — surfaced as a normal failure entry
  (`{"name": <name>, "reason": "unknown invariant ..."}`) rather than
  aborting the call, so a typo in one invariant does not hide siblings.
- **Plain prose input** — the agent falls back to a single `non_empty`
  check so adopters always see a structured verdict.
- **Port already in use** — set `A2A_PORT` to an unused port.

## Troubleshooting

- `curl: (7) Failed to connect` — the agent has not started yet, or
  `A2A_PORT` is set to a different value than the one you're hitting.
- `agent did not become ready` in the smoke test — increase the wait
  budget or check that nothing else is bound to port 38013.
- `Manifest parse: ...` startup error — the on-disk `SKILL.md` files
  must round-trip through `SkillCard::parse`. The build-time embedded
  manifests run a parse test (`cargo test -p agent-role-critic-agent`)
  so a regression is caught before the binary ships.

## Smoke test

```bash
cargo test -p agent-role-critic-agent --tests
```

The smoke test spawns the binary on an isolated port (38013) and walks the
four critic paths end-to-end. Hermetic — no network access.

## References

- [ADR-021 §9 Q3](../../docs/adr/ADR-021-turul-a2a-patterns-extraction.md) — agent role patterns: generic-only, no vendor lock-in.
- [ADR-021 §2.2 item 3](../../docs/adr/ADR-021-turul-a2a-patterns-extraction.md) — SKILL.md manifest authoring shape.
- `examples/agent-role-planner-router-agent` — sibling example showing the
  planner + router idiom with the same `SkillRegistry`/`SkillHandler`
  shape.
- `examples/skill-manifest-ollama-agent` — manifest-driven dispatch with
  optional Ollama integration.
