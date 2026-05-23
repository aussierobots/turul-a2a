# skill-manifest-ollama-agent

Reference example for ADR-021 Phase C. It exercises `turul-a2a-patterns`
end-to-end: a `SKILL.md` manifest declares one skill, the example agent
registers a handler for it, and an A2A server exposes the agent over
HTTP+JSON / JSON-RPC / SSE.

## What is A2A?

A2A is the [Agent-to-Agent Protocol](https://github.com/a2aproject/A2A),
a wire contract for one autonomous agent to call another. It defines an
HTTP+JSON surface (`/message:send`, `/message:stream`, …), a JSON-RPC
surface, a streaming SSE surface, and a normative discovery document
(`/.well-known/agent-card.json`). This workspace implements the v1.0
protocol in Rust; see the workspace `README.md` for the supported wire
surface.

## What this example demonstrates

```
SKILL.md (frontmatter + body)
   │
   │  SkillCard::parse                                (turul-a2a-patterns)
   ▼
SkillCard ──► to_agent_skill ──► AgentSkill (eight discovery fields)
   │                                  │
   │                                  ▼
   │                            AgentCardBuilder.skill(…)
   │
   ├─► input_schema  ──► validate_input  on every invocation
   ├─► output_schema ──► validate_output on every response
   ├─► body          ──► render_prompt   ({{ user.name }}, …)
   │
   ▼
InMemorySkillRegistry.register_manifest(card, handler)

AgentExecutor::execute   ──►   registry.handler(id).run(params, &sink)
                                              │
                                              ▼
                                  offline stub   OR   live Ollama
```

The skill itself, `greet`, takes `{user: {name}, style}` and returns
`{greeting}`. The dispatch — picking which handler to call when a
message arrives — is implemented by this example. ADR-021 §2.4
deliberately leaves a framework-level dispatcher for a future profile
extension.

## Prerequisites

- Rust 1.85+ (the workspace `rust-version`). Install via
  [rustup.rs](https://rustup.rs).
- A working `echo-agent`. Start with that to confirm your toolchain is
  healthy:
  ```sh
  cargo run -p echo-agent
  ```
  It listens on `:3000`. Stop it (`Ctrl-C`) before continuing.
- Optional (live mode only): a local [Ollama](https://ollama.com) install
  with a pulled model, e.g. `ollama pull llama3.1`.

## Run — offline (default)

```sh
cargo run -p skill-manifest-ollama-agent
```

Expected log lines:

```
Skill Manifest Ollama Agent listening on http://0.0.0.0:3010
Mode: offline-stub
Agent card: http://localhost:3010/.well-known/agent-card.json
```

In another shell, send a JSON-shaped greeting. Replace the message-body
text with a JSON string the manifest's `inputSchema` accepts:

```sh
curl -s -X POST http://localhost:3010/message:send \
  -H 'Content-Type: application/json' \
  -H 'a2a-version: 1.0' \
  -d '{
        "message": {
          "messageId": "1",
          "role": "ROLE_USER",
          "parts": [{ "text": "{\"user\":{\"name\":\"Ada\"},\"style\":\"formal\"}" }]
        }
      }'
```

The response contains an artifact whose text part is a JSON object
matching the manifest's `outputSchema`:

```json
{"greeting": "Good day, Ada! (offline stub)"}
```

The agent also accepts plain text — `{"text": "Ada"}` parses as
`{user: {name: "Ada"}}` and dispatches normally. The Rust client example
under `examples/clients/rust/` hard-codes `localhost:3000`, so if you
want to use it instead of `curl`, first run `echo-agent` to confirm the
client, then either start this example on `A2A_PORT=3000` (after killing
`echo-agent`) or use `curl` / the Python/Go clients for this example.

## Run — live Ollama (opt-in, manual)

Live mode flips on when either `OLLAMA_BASE_URL` is set or
`RUN_OLLAMA_SMOKE=1` is set. The handler POSTs the rendered prompt
plus the manifest's `outputSchema` (as Ollama's `format` field) to
`/api/chat` on the configured Ollama server, then validates the
returned JSON against the manifest's output schema before it leaves
the agent.

### Option A — `.env` file (recommended for repeated local dev)

The binary auto-loads `.env` from the current working directory at
startup. A `.env.example` is committed alongside this README; copy
it once and edit values for your setup:

```sh
cp .env.example .env
# Edit .env — pick one of:
#   OLLAMA_BASE_URL=http://localhost:11434         # local Ollama
#   OLLAMA_BASE_URL=http://<your-ollama-host>:11434  # LAN-resolvable Ollama
```

`.env` is gitignored — your local config never leaves your machine.
`.env.example` stays committed as documentation.

Then:

```sh
ollama serve                       # on the Ollama host (skip if remote)
ollama pull llama3.1               # one-time per host
cargo run -p skill-manifest-ollama-agent
```

### Option B — one-shot env override (CI / scripted runs)

```sh
ollama serve                       # in one shell
ollama pull llama3.1               # one-time
OLLAMA_BASE_URL=http://localhost:11434 \
  cargo run -p skill-manifest-ollama-agent
```

`RUN_OLLAMA_SMOKE=1` is accepted as a shorthand for
`OLLAMA_BASE_URL=http://localhost:11434`.

### What you should see

Startup log reads `Mode: live-ollama`. Send the same `curl` as the
offline section above; expect a `greeting` string produced by
`llama3.1`, validated against the manifest's output schema. If
validation fails (model returns non-JSON or JSON that doesn't match
the schema), the agent returns a Failed task with the validator's
location + reason — by design (§2.2 item 3 of ADR-021).

## Failure modes

| Symptom                                              | Cause                                                | Fix                                                                                          |
| ---------------------------------------------------- | ---------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| `address already in use` on startup                  | Another process bound `:3010`                        | Stop it, or `A2A_PORT=3011 cargo run -p skill-manifest-ollama-agent`                         |
| `LLM call failed: LLM transport error: …`            | Live mode, but Ollama not running / wrong URL        | Start `ollama serve`, confirm `curl http://localhost:11434/api/tags` works                   |
| `LLM call failed: LLM provider error: …`             | Ollama reached but returned non-2xx (e.g. model not pulled, bad payload) | Confirm the model is pulled (`ollama list`), check the `model` in `skills/demo/SKILL.md`'s `providerConfig` |
| `LLM call failed: LLM response did not satisfy output schema: …` | Model returned non-JSON despite `format`             | Try a different model (`llama3.1:8b-instruct`, etc.), or fall back to the offline mode       |
| `manifest parse error …`                             | `skills/demo/SKILL.md` edited into an invalid shape  | Re-read [the three-section frontmatter](#the-skillmd-manifest); restore the camelCase keys   |
| `inputSchema violation` on `/message:send`           | Sent JSON does not satisfy the manifest input schema | Match the schema, e.g. `{"user":{"name":"Ada"}}`                                             |

## The `ExampleProgressSink` bridge

The example wraps the framework's `EventSink` in a local struct so it
can implement the `SkillProgressSink` trait from `turul-a2a-patterns`:

```rust
struct ExampleProgressSink {
    event_sink: EventSink,
}

impl turul_a2a_patterns::SkillProgressSink for ExampleProgressSink { … }
```

The named `event_sink` field is deliberate — it tells the reader the
wrapped value's role at the call site (`self.event_sink.set_status(...)`),
which is the whole point of the example. A tuple newtype
(`struct ExampleProgressSink(EventSink)` with `self.0`) would compile
identically but discard that signal; the workspace's CLAUDE.md /
AGENTS.md rule requires the named-field shape in showcase examples.

Why the wrapper exists: `SkillProgressSink` lives in
`turul-a2a-patterns`, `EventSink` lives in `turul-a2a`, and the
example crate is a third crate. Rust's [orphan rule](https://doc.rust-lang.org/reference/items/implementations.html#orphan-rules)
forbids implementing an external trait for an external type from a
third crate, so the impl must hang off a local type.

Adopter consequence today: if you copy this example into your own
service repo, you also copy the wrapper. The two methods that matter
just delegate to the inner `EventSink`'s public API
(`set_status` / `emit_artifact`).

Adopter consequence after ADR-021 §4 gates clear and `turul-a2a-patterns`
flips to publishable: the newtype is no longer required, because
`turul-a2a` can then carry a direct `impl SkillProgressSink for
EventSink` (both trait and type are local to that crate). The example
keeps the newtype as documentation; new adopters can drop it.

## Typed input / output structs

The handler uses example-local `GreetInput { user: UserInput, style:
GreetingStyle }` and `GreetOutput { greeting: String }` structs instead
of digging through `serde_json::Value`. **SKILL.md is still the
authoritative contract** — the handler calls `card.validate_input`
before deserialising into `GreetInput`, and `card.validate_output`
after serialising `GreetOutput` back to JSON. The typed structs sit
between those calls; they are ergonomics, not the contract.

Four tests pin the structs to the manifest so struct/schema drift is
caught at `cargo test` time:

- `every_example_payload_deserialises_into_struct` — every payload
  shown in this README, in `tests/smoke.rs`, and in the manifest's
  `examples` block must `serde_json::from_value::<GreetInput>(_)`.
- `typed_input_serialises_to_schema_valid_json` — a sample
  `GreetInput` must serialise to JSON that satisfies the manifest's
  `inputSchema`.
- `typed_output_serialises_to_schema_valid_json` — same for
  `GreetOutput` and `outputSchema`.
- `manifest_validates_before_struct_deserialises` — proves the
  manifest catches constraints the typed struct alone cannot (e.g.
  `user.name` `minLength: 1`, which serde happily accepts as `""`).

If you add a new example payload anywhere in this crate, add it to
`EXAMPLE_PAYLOADS` in `src/main.rs` so the first test exercises it.

## The SKILL.md manifest

The canonical file is [`skills/demo/SKILL.md`](skills/demo/SKILL.md).
The frontmatter has three sections (ADR-021 §2.2 item 3):

1. **A2A discovery fields** — `id`, `name`, `description`, `tags`,
   `examples`, `inputModes`, `outputModes`, `securityRequirements`.
   All camelCase. These project onto an `AgentSkill` (eight fields) and
   appear on the agent card unchanged.

   **`securityRequirements` — reduced expressivity in v1 manifests
   (per ADR-021 §2.2 item 3).** In SKILL.md, this field is a list of
   scheme-name strings: `securityRequirements: ["bearer", "apiKey"]`.
   The A2A proto's `AgentSkill.security_requirements` is richer —
   each entry is a `SecurityRequirement { schemes: { name: [scopes] } }`
   permitting per-scheme OAuth-style scopes. The SKILL.md form
   collapses each entry to scheme-name-only; the patterns crate maps
   each name to `SecurityRequirement { schemes: { "<name>": [] } }`
   (empty scopes) when generating the AgentSkill. **What you lose:**
   per-skill OAuth scopes — if your skill needs `["read", "write"]`
   on an `oauth2` scheme, that doesn't fit the SKILL.md form yet and
   must be configured at the agent level (per ADR-015) or
   programmatically via `AgentSkillBuilder::security_requirements()`
   instead of through SKILL.md. **What you keep:** scheme-name
   *reference* into the agent-level `securitySchemes` catalogue,
   which is the common case. A richer SKILL.md form
   (`securityRequirements: [{"oauth2": ["read", "write"]}]`) can be
   added as a non-breaking extension if adopter demand justifies it
   — the simple list form remains valid.

2. **Provider-neutral execution metadata** — `inputSchema` and
   `outputSchema` (JSON Schema 2020-12), `executionHints` with
   `maxTokens` / `temperature` / `topP`. The schemas are Turul-local
   planning metadata: they are not on the A2A wire, but `turul-a2a-patterns`
   uses them to validate every invocation's input and output.

3. **Opaque `providerConfig:` block** — anything the patterns crate
   should NOT interpret. This example puts Ollama-specific keys here
   (`model`, `endpoint`, `format`, `options`). The handler reads from
   `card.provider_config` directly; the patterns crate treats the
   block as an opaque `serde_json::Value`.

## Tests

`cargo test -p skill-manifest-ollama-agent` runs unit tests for the
extractor / offline handler and an offline-mode smoke test that boots
the agent on a private port, asserts the manifest skill is on the agent
card, and walks one `/message:send` round-trip. There is no live-Ollama
test — that path is exercised manually by the steps under
[Run — live Ollama](#run--live-ollama-opt-in-manual).
