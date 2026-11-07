# turul-a2a-patterns

Reusable abstractions for **A2A skill authoring** — handler trait,
registry, SKILL.md manifest loader, progress sink, terminal hook,
JSON Schema validation helper.

> **Status:** published on crates.io alongside the other `turul-a2a-*`
> crates (see
> [ADR-021](../../docs/adr/ADR-021-turul-a2a-patterns-extraction.md)).
> It depends on `turul-a2a-proto` and `turul-a2a-types` only — never on
> the `turul-a2a` server runtime.

## What it is

A foundational abstraction layer for writing A2A skills. It defines
**traits and types** that adopter agents and the framework's server
runtime both build on. It does **not** change the A2A wire contract;
extension machinery that *does* change the wire lives elsewhere (see
ADR-021 §1.2).

## What it is not

- Not a server. Not a transport. Not an LLM client. Not a profile
  extension. Not the `AgentSkill` proto type (that's `turul-a2a-proto`
  / `turul-a2a-types` — discovery metadata only).
- Not a bucket for every useful idiom. Per ADR-021 §2.2, additions to
  the public surface require an ADR amendment.

## Public surface

| Symbol | Purpose |
|---|---|
| `SkillHandler` | Async trait. Adopter impls write `async fn run(&self, params, sink) -> Result<Value, SkillError>`. `#[async_trait]`. |
| `SkillProgressSink` + `ProgressState` + `SinkError` | Trait the handler emits status updates and artifacts through during execution. `ProgressState` is `#[non_exhaustive]` and contains only **non-terminal** states (`Working`, `InputRequired`, `AuthRequired`) — terminal commits are framework-owned. `SinkError::{Closed, Backend(String)}` for best-effort failure semantics. `#[async_trait]`. |
| `SkillRegistry` + `InMemorySkillRegistry` | Maps `AgentSkill.id` → `SkillHandler`. In-memory impl is the default; alternative impls (e.g. dynamic discovery) are adopter-owned. |
| `SkillCard` + `ExecutionHints` | SKILL.md manifest type built from Agent Skills frontmatter plus `a2a-*` metadata keys and a markdown body. Constructors (no `Deserialize`): `SkillCard::from_embedded(dir_name, skill_md, &[(asset_path, asset_text)])` for `include_str!`-embedded skills, and `SkillCard::from_skill(&Skill, reader)` / `SkillCard::try_from(&skill)` for skills loaded from disk. They reject unknown `a2a-*` keys, unsupported schema keywords and uncompilable schemas. `validate_input` / `validate_output` use [`validate_json`]. `to_agent_skill()` projects to the proto `AgentSkill`. |
| `SkillDescriptor` | Introspection struct (`id`, `card`, `params_schema`). **Single source of truth** for the schema: for manifest-backed skills, derived from the manifest; for programmatic skills, supplied once at registration. Never adopter-fillable independently. |
| `SkillError` | Two variants only: `InvalidRequest(String)` (maps to A2A `InvalidRequest`) and `Internal(String)` (maps to A2A `Internal`). |
| `TerminalHook` + `SkillOutcome` | Post-execution observer fired by adopter dispatch code when `SkillHandler::run` returns. Best-effort (return type `()`). `SkillOutcome::{Success(&Value), Failure(&SkillError)}` (`#[non_exhaustive]`). Minimal generic variant only — richer extractor-registry / isolation semantics remain deferred (ADR-021 §9 Q5). `#[async_trait]`. |
| `validate_json(schema, instance)` | Public free helper. JSON Schema 2020-12 validation; strict-reject of unsupported keywords. Returns first violation as a structured `ValidationError`. |

## SKILL.md format

SKILL.md follows the [Agent Skills](https://agentskills.io) spec. The
format and its loader are owned by the external `turul-agent-skills`
crate (`load(dir)`, `load_str(dir_name, text)`, and the
`turul-skills validate <dir>` CLI). The frontmatter has only `name`
(must equal the directory name, lowercase-hyphen), `description` and
`metadata` (string to string). The body is an opaque template string
consumed by prompt rendering (or human-readable instructions, for
non-LLM skills). A2A fields live in `metadata` under `a2a-*` keys:

```markdown
---
name: greet
description: Greet a named user in a chosen style.
metadata:
  a2a-examples: '["Greet Ada in a formal tone","Casually greet Grace"]'
  a2a-input-modes: '["text/plain","application/json"]'
  a2a-input-schema: assets/input.schema.json
  a2a-max-tokens: '128'
  a2a-name: Greet
  a2a-output-modes: '["application/json"]'
  a2a-output-schema: assets/output.schema.json
  a2a-provider-config: assets/provider-config.json
  a2a-tags: '["demo","greeting","ollama"]'
  a2a-temperature: '0.2'
  a2a-top-p: '0.9'
---
You are a courteous assistant. Greet the user named "{{ user.name }}" in a {{ style }} tone.

Respond with a single JSON object matching the output schema, e.g. `{"greeting": "..."}`.
```

| `metadata` key | Value | Maps to |
|---|---|---|
| `a2a-id` | string | wire skill id when it differs from `name` |
| `a2a-name` | string | display name |
| `a2a-tags`, `a2a-examples`, `a2a-input-modes`, `a2a-output-modes`, `a2a-security-requirements` | compact JSON array string; empty list = key omitted | matching `AgentSkill` fields |
| `a2a-max-tokens`, `a2a-temperature`, `a2a-top-p` | decimal string | `ExecutionHints` |
| `a2a-input-schema`, `a2a-output-schema`, `a2a-provider-config` | relative path to a JSON file under `assets/` | JSON Schema 2020-12 input / output schema; opaque provider config |

Unknown `a2a-*` keys are rejected; other metadata keys are ignored.
Errors are `ManifestError::Parse { location: "metadata.<key>" | "SKILL.md", .. }`
and `ManifestError::Schema { location: "metadata.a2a-input-schema" | "metadata.a2a-output-schema", .. }`.

The patterns crate **does not interpret** the provider config. Adopter
code reads it to drive the actual model call. For non-LLM skills
(Rust-native handlers, MCP-tool-backed, deterministic logic), omit the
execution and provider keys and use the manifest only for discovery
metadata + I/O schemas.

## Trust model — security boundary

The loader treats SKILL.md as a **trusted static manifest + static
prompt template**. Concretely it MUST NOT:

- Execute shell commands or evaluate code in any field.
- Resolve `!include`-style directives or load arbitrary support files.
  Asset paths must be relative and inside the skill directory (no `..`,
  no absolute paths), and nothing is read other than the files named by
  `a2a-input-schema`, `a2a-output-schema` and `a2a-provider-config`.
- Interpret command-injection syntax or MCP tool references.
- Make outbound network calls during parsing.

Adopters that need richer manifest semantics extend the format under
a successor ADR — not by side-channel features in the loader.

## Contracts (load-bearing for stable tests)

- **Template grammar**: minimal `{{ name }}` substitution; dotted paths
  (`{{ user.id }}`); no logic / conditionals / loops; literal `{{`
  escaped as `\{{`.
- **Variable resolution**: missing variables produce a structured
  `RenderError`. Never silent empty substitution.
- **JSON Schema dialect**: JSON Schema 2020-12 (strict).
- **Unsupported JSON Schema keywords**: rejected at load time, not
  silently ignored.
- **Error reporting**: structured (`location`, `reason`) for both
  schema validation and template rendering.

## Example use

```rust
use std::sync::Arc;
use async_trait::async_trait;
use serde_json::{json, Value};
use turul_a2a_patterns::{
    InMemorySkillRegistry, ProgressState, SkillCard, SkillError,
    SkillHandler, SkillProgressSink, SkillRegistry,
};

struct Greet { card: SkillCard }

#[async_trait]
impl SkillHandler for Greet {
    async fn run(
        &self,
        params: Value,
        sink: &dyn SkillProgressSink,
    ) -> Result<Value, SkillError> {
        // Validate input against the manifest's input schema.
        self.card.validate_input(&params)
            .map_err(|e| SkillError::InvalidRequest(e.to_string()))?;

        // (Optional) signal progress.
        let _ = sink.set_status(ProgressState::Working, None).await;

        // Compute the result.
        let name = params.pointer("/user/name").and_then(Value::as_str).unwrap_or("friend");
        let out = json!({ "greeting": format!("Hello, {name}!") });

        // Validate output against the manifest's output schema.
        self.card.validate_output(&out)
            .map_err(|e| SkillError::Internal(e.to_string()))?;

        Ok(out)
    }
}

async fn build_agent() -> Result<Arc<dyn SkillRegistry>, Box<dyn std::error::Error>> {
    let card = SkillCard::from_embedded(
        "greet",
        include_str!("../skills/greet/SKILL.md"),
        &[
            ("assets/input.schema.json", include_str!("../skills/greet/assets/input.schema.json")),
            ("assets/output.schema.json", include_str!("../skills/greet/assets/output.schema.json")),
            ("assets/provider-config.json", include_str!("../skills/greet/assets/provider-config.json")),
        ],
    )?;
    let handler = Arc::new(Greet { card: card.clone() });

    let registry = Arc::new(InMemorySkillRegistry::default());
    registry.register_manifest(card, handler).await?;
    Ok(registry)
}
```

Working end-to-end examples live in
[`examples/skill-manifest-ollama-agent`](../../examples/skill-manifest-ollama-agent/),
[`examples/agent-role-planner-router-agent`](../../examples/agent-role-planner-router-agent/),
[`examples/agent-role-critic-agent`](../../examples/agent-role-critic-agent/),
and [`examples/post-task-hook-agent`](../../examples/post-task-hook-agent/).
Each shows the bridge-adapter pattern:

```rust
struct ExampleProgressSink {
    event_sink: EventSink,
}

#[async_trait]
impl SkillProgressSink for ExampleProgressSink {
    async fn set_status(/* ... */) -> Result<(), SinkError> {
        self.event_sink.set_status(/* ... */).await /* ... */
    }
    // emit_artifact + is_closed delegate to self.event_sink likewise
}
```

The named-field shape (`event_sink:`, not `.0`) is a CLAUDE.md /
AGENTS.md rule for showcase examples: the wrapped framework value
is part of the lesson, so the field name should make that obvious
at the call site. Orphan-rule rationale for needing a local wrapper
at all: `SkillProgressSink` lives in this crate, `EventSink` lives
in `turul-a2a`, and the example crate is a third crate — a direct
`impl SkillProgressSink for EventSink` from the example is illegal
Rust.

## Module layout

```
src/
├── lib.rs          # crate docs + pub use re-exports
├── error.rs        # SkillError, SinkError, ManifestError, RenderError, ValidationError
├── sink.rs         # SkillProgressSink, ProgressState
├── handler.rs      # SkillHandler trait
├── registry.rs     # SkillRegistry, InMemorySkillRegistry, SkillDescriptor
├── manifest.rs     # SkillCard, ExecutionHints, SKILL.md loader + AgentSkill projection
├── template.rs     # prompt rendering
├── schema.rs       # validate_json, Draft-2020-12 strict-keyword check
└── hook.rs         # TerminalHook, SkillOutcome
```

## Testing

```bash
cargo test -p turul-a2a-patterns      # projection, registry, sink and hook tests
```

## Not in scope

- **Dispatcher** (skill-invocation profile extension) — wire-affecting;
  belongs in a future profile-extension crate per ADR-021 §5. See
  [ADR-022](../../docs/adr/ADR-022-skill-invocation-dispatcher-profile.md).
- **LLM client** — orthogonal concern (cadence/audience/deps differ).
  See [ADR-023](../../docs/adr/ADR-023-llmclient-abstraction.md).
- **Agent role abstractions** (planner, router, coordinator, critic,
  gateway/facade) — the patterns crate ships **zero** role types today.
  The example agents under `examples/agent-role-*/` are reference
  implementations, not framework types (ADR-021 §9 Q3).
- **Provider-specific code** (Ollama / OpenAI / Anthropic adapters) —
  lives in example crates; never in this crate.

## See also

- [ADR-021](../../docs/adr/ADR-021-turul-a2a-patterns-extraction.md) — this crate's design record (Accepted).
- [ADR-022](../../docs/adr/ADR-022-skill-invocation-dispatcher-profile.md) — Proposed dispatcher profile extension.
- [ADR-023](../../docs/adr/ADR-023-llmclient-abstraction.md) — Proposed LlmClient abstraction.
- [`examples/interop-clients/CLIENT_MATRIX.md`](../../examples/interop-clients/CLIENT_MATRIX.md) — cross-language interop evidence (Python / Go / Rust × 4 showcase agents).

## License

MIT OR Apache-2.0, matching the rest of the `turul-a2a` workspace.
