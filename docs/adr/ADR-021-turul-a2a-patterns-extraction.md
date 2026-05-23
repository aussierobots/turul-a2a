# ADR-021: Create `turul-a2a-patterns` Skill-Pattern Crate

- **Status:** Accepted
- **Date:** 2026-05-22
- **Depends on:** ADR-001 (proto-first), ADR-002 (wrapper boundary),
  ADR-015 (declaration-only precedent), ADR-020 (deferred-extraction
  precedent)
- **Spec reference:** `proto/a2a.proto` L224-242 (`Part` oneof —
  `text|raw|url|data` where `data: google.protobuf.Value`), L260-277
  (`Message`), L273-274 (`Message.extensions: repeated string`,
  attribution channel), L412 + L418-427 (`AgentCapabilities.extensions`
  / `AgentExtension`), L430-447 (`AgentSkill`), L642-651
  (`SendMessageRequest`); A2A spec
  https://a2a-protocol.org/latest/topics/extensions/ (activation via
  HTTP `A2A-Extensions` header)

## 1. Context

Adopters that build A2A agents on this framework consistently
reinvent the same small set of server-side patterns: a trait for
"something that runs when a skill is invoked," a registry that maps
skill ids to those handlers, a way to declare skill metadata in a
portable file, and a planner-friendly introspection struct over the
result. None of these patterns are normatively defined by the
current A2A proto,
none change what a client sees on the wire, and all are generic to
A2A agent authoring.

This ADR proposes a new workspace crate, `turul-a2a-patterns`, that
provides these patterns in framework-grade form. It also defines the
boundary between *patterns* (this crate) and *profile extensions*
(which belong elsewhere — see §1.1 and §5).

### 1.1 Pattern vs Profile vs Extension — taxonomy

A2A's spec and proto give four distinct surfaces that classify what
a `turul-a2a-*` crate can host:

| Bucket | Definition | Cited surface | Where it belongs |
|---|---|---|---|
| **Types** | Anything defined in the normative proto. | `proto/a2a.proto` (`AgentCard`, `AgentSkill`, `Message`, `Task`, `Part`, `AgentExtension`, …). | `turul-a2a-proto`, `turul-a2a-types`. |
| **Patterns** | Reusable abstractions for A2A skill authoring that **do not** change the wire contract: handler traits, registries, descriptors, progress-sink traits, manifest/prompt/schema helpers, agent role idioms. Usable by server runtime, by manifest-only tools (CLI / static analysers / build-time card generators), and by adopter code; never depends on a transport implementation. | This ADR's subject. | `turul-a2a-patterns` (this ADR). |
| **Profiles / extensions** | Things that **do** change wire behaviour. Declared via `AgentCapabilities.extensions` (proto L412) and *activated* by the client via the HTTP `A2A-Extensions` header. | Skill-invocation envelopes; method extensions; state-machine extensions. | Future separate crate (§5); not addressed today. |
| **Application** | Domain-specific patterns useful in one project but not framework-generic. | Anything bound to a particular industry, dataset, vendor, or workflow. | Stays in the adopter's own code; never in `turul-a2a-*`. |

The boundary that matters here: **patterns vs profiles**. Patterns
are local to one server runtime — their absence does not change what
a client sees on the wire. Profiles ARE observable on the wire;
their absence on the server changes what clients must do.

### 1.2 Activation lives in transport, not in helper crates

`Message.extensions` (proto L273-274, `repeated string`) is the
**attribution** channel — it lists URIs of extensions present or
contributed to *that one message*. It is NOT how a client requests
an extension be applied.

**Activation** is the HTTP `A2A-Extensions` header (A2A spec
§"Extensions"). The header value is a comma-separated list of
extension URIs the client intends to activate; the server's response
SHOULD echo activated URIs in the same header. Required extensions
the server cannot honour cause a failure; non-required extensions
are ignored.

Consequence — load-bearing for §2.4 and §5: the code that parses
`A2A-Extensions` off an inbound request, dispatches activated URIs
to the matching profile implementation, echoes activated URIs on the
response, and rejects unsupported `required` extensions belongs in
`turul-a2a`'s transport/router layer
(`crates/turul-a2a/src/router.rs` + gRPC/Lambda equivalents),
**not** in any helper crate. A future profile crate (§5) can own URI
constants, parameter schemas, and validators — but `turul-a2a` is
where the wire dispatch happens.

### 1.3 A2A proto reality (current vendored snapshot)

These proto facts — read from the proto file vendored in this repo
at the time of writing — shape what the patterns crate can and
cannot do. §7.1 re-confirms them against the proto-on-disk before
acceptance.

- `AgentSkill` (L430-447): eight fields — `id`, `name`, `description`,
  `tags`, `examples`, `input_modes`, `output_modes`,
  `security_requirements`. Discovery metadata only. No `parameters`,
  no `schema`, no `metadata` carrier. An adopter's runtime-planning
  needs (e.g. parameter schemas) cannot live inside `AgentSkill`.
- No `skill_id` field on `Message` (L260-277) or `SendMessageRequest`
  (L642-651). Any skill-routing convention is Turul-local until the
  upstream proto binds it. A skill *dispatcher* is therefore a
  *profile extension* (§5, §2.5), not a pattern.
- `Part.data` is `google.protobuf.Value` (L237), not `Struct`. There
  is no `DataPart` message type. Any future profile carrying
  structured per-skill data has to spec the `Value` shape explicitly.
- `AgentCapabilities.extensions` (L412) carrying `AgentExtension`
  (L418-427) is the **agent-level** declaration surface. Per-skill
  declaration must side-channel-key off `AgentSkill.id` from inside
  one of those agent-level extensions.

## 2. Decision

### 2.1 Create `turul-a2a-patterns` as an internal path-only workspace crate

Create `crates/turul-a2a-patterns/` with these `Cargo.toml`
properties:

- `publish = false` in the crate's own `Cargo.toml`.
- Crate inherits `version.workspace = true` (no workspace version
  drift).
- Listed in root `[workspace.dependencies]` as a path-only entry:
  `turul-a2a-patterns = { path = "crates/turul-a2a-patterns" }`
  (no `version = "X.Y.Z"` field — required only for publishable
  crates).
- Not in the publish dependency order (CLAUDE.md §"Release & Publish").
- MUST NOT become a `[dependencies]` entry of any *publishable*
  workspace crate while it is path-only. (Allowed as
  `[dev-dependencies]`.) Adding it to a publishable crate's
  `[dependencies]` triggers a publish-blocker for that crate and
  forces §4 gates to clear first.

The "patterns" name is deliberate. The crate hosts **skill-authoring
abstractions**, not the A2A proto's `AgentSkill` (8-field discovery
metadata). Naming the crate `turul-a2a-skills` would conflate the
two. The chosen name matches the established `turul-a2a-{role}`
convention.

### 2.2 Initial public surface

**Five primary surfaces + one Q5-resolved additive surface**
(`TerminalHook` was Open Question §9 Q5 at draft time and was
resolved-in-scope during Phase C; its binding contract is in §9 Q5).
Ordered by certainty:

1. **`SkillHandler` trait.** Async trait whose `run` method takes
   structured input parameters plus a progress event sink, and
   returns either structured output or a typed error. Trait-object-
   friendly. Couplings resolved per §2.3.

2. **`SkillRegistry` trait + `InMemorySkillRegistry` impl.** Maps
   `AgentSkill.id` to a registered `SkillHandler`. The in-memory
   implementation is the default; alternative implementations (e.g.
   dynamic discovery) are adopter-owned.

3. **`SkillCard` + SKILL.md manifest support.** SKILL.md is a
   portable skill manifest. The patterns crate's manifest support
   is **execution-model-agnostic**: LLM-backed skills are the most
   common case (frontmatter declares schemas + provider config,
   body holds the prompt template), but **non-LLM skills**
   (Rust-native handlers, deterministic logic, MCP-tool-backed
   skills, etc.) are first-class — they use the manifest for
   discovery metadata and I/O schemas only and ignore the
   LLM-specific sections.

   The format aligns with the broader industry SKILL.md convention
   (YAML frontmatter + Markdown body). The frontmatter splits into
   three sections with a deliberate boundary:

   - **`AgentSkill` discovery fields** (always supported, all eight
     proto fields). **Frontmatter casing: camelCase only**, matching
     the A2A JSON serialization (`id`, `name`, `description`,
     `tags`, `examples`, `inputModes`, `outputModes`,
     **`securityRequirements`**). Rationale: SKILL.md is a portable
     cross-tool artifact; camelCase round-trips with any A2A
     consumer that reads JSON-shape AgentCard documents. The Rust
     types use snake_case field names internally (standard Rust
     convention) with `#[serde(rename_all = "camelCase")]` on the
     struct, so the wire form is exactly one casing — camelCase.
     Snake_case manifests are **NOT** silently accepted via
     `#[serde(alias = …)]`. If a future need for snake_case
     acceptance arises (e.g. human-edited manifests), it ships as
     an explicit, documented opt-in with its own tests, not as a
     serde alias accident. Skill-level `securityRequirements`
     follow ADR-015 (declaration-only with truthfulness invariants
     validated at server build).

     **Shape of `securityRequirements` in SKILL.md (binding contract):**
     Each entry is a **scheme name string**, NOT the proto-shaped
     `SecurityRequirement { schemes: map<string, ScopesList> }`.
     SKILL.md form: `securityRequirements: ["bearer", "apiKey"]`.
     Mapping to the proto on AgentSkill generation: each scheme name
     becomes a `SecurityRequirement { schemes: { "<name>": [] } }`
     — an empty scopes list. Rationale: most SKILL.md authors don't
     need OAuth-style scopes per skill; the simpler list shape is
     readable, round-trips losslessly through serde, and keeps the
     manifest format close to A2A `AgentSkill.security_requirements`
     semantics (scheme-name reference into the agent-level
     `securitySchemes` catalogue, with scoping handled at the
     agent level per ADR-015). A future amendment may add a richer
     scope-aware manifest form
     (`securityRequirements: [{"oauth2": ["read", "write"]}]`)
     additively if adopter demand justifies it — that's a
     non-breaking extension because the simple list form remains
     valid.
   - **Provider-neutral execution metadata** (optional; meaningful
     for LLM-backed skills): input/output JSON Schemas plus a small
     fixed set of provider-neutral execution hints (e.g.
     `max_tokens`, `temperature`, `top_p`). The patterns crate
     defines and validates this contract. Non-LLM skills declare
     schemas here and omit the hints.
   - **Opaque `provider_config:` block** (optional; meaningful for
     LLM-backed skills): provider-specific options (model identifier
     strings, vendor-specific tuning parameters, API endpoint
     hints). The patterns crate does **not** interpret this block —
     adapter code in `examples/` or adopter code reads it. Keeping
     vendor knobs opaque preserves provider neutrality (§2.4).

   The patterns crate provides four generic helpers:

   - **Manifest parsing** — SKILL.md text → typed `SkillCard`
     (frontmatter + body + parsed schemas). Applies to all skills.
   - **AgentSkill generation** — extract the wire-discoverable
     subset (all eight discovery fields including
     `security_requirements`) into a `turul_a2a_proto::AgentSkill`
     for `AgentCard.skills`. Applies to all skills.
   - **Prompt rendering** — instantiate the Markdown body's
     template against an input-parameter object. LLM-backed skills
     consume the rendered prompt; non-LLM skills do not call this.
   - **I/O schema validation** — validate input against the
     manifest's input schema before invocation; validate output
     against the output schema after invocation. Applies to all
     skills. JSON Schema; validation helpers ship in this crate.

   **Trust model — security boundary.** The patterns crate treats
   SKILL.md as a **trusted static manifest + static prompt template**.
   There is no dynamic interpretation of either section. Concretely
   the parser MUST NOT:

   - Execute shell commands or evaluate code in any field.
   - Resolve `!include`-style directives or load arbitrary support
     files referenced from the manifest.
   - Interpret command-injection syntax (e.g. shell-backed context
     lines), MCP tool references, or any other dynamic semantics
     other manifest dialects may permit.
   - Make outbound network calls during parsing.

   Adopters needing richer manifest semantics (dynamic commands,
   MCP tool references, support-file loading, multi-file manifests)
   extend the format under a successor ADR — not by side-channel
   features in the parser.

   **Contracts** (load-bearing for stable tests; see §7.1
   sign-off):

   - **Template grammar**: minimal `{{ name }}` substitution
     supporting dotted paths (`{{ user.id }}`). No logic, no
     conditionals, no loops, no helpers. A literal `{{` in the
     prompt is escaped as `\{{`.
   - **Variable resolution**: missing variables produce a structured
     render error — never a silent empty substitution. (Silent
     empty-string substitution is a documented LLM-prompt
     correctness footgun.)
   - **JSON Schema dialect**: JSON Schema 2020-12 for local
     validation in the patterns crate. Compatible with major Rust
     validator crates; the form accepted by Ollama's
     structured-output API is one downstream consumer. Local
     validation in the patterns crate is **authoritative for the
     manifest contract** — provider adapters in `examples/` or
     adopter code MAY reject or down-convert schemas to whatever
     subset their specific provider supports, but they MUST NOT
     loosen what the patterns crate has already validated.
   - **Unsupported JSON Schema keywords**: strict reject at parse
     time, never silently ignored. Adopters get an explicit error.
   - **Error reporting**: structured (location + reason) for both
     schema validation and template rendering. Tests assert against
     the structured form, not against message strings.

   The model call itself is the adopter's responsibility. For
   LLM-backed skills the patterns crate provides everything around
   it (rendered prompt going in, validated structured output coming
   out). For non-LLM skills the patterns crate provides the
   discovery + I/O schema surface; the adopter implements
   `SkillHandler` directly. A reference example wiring this to
   Ollama is a Phase C deliverable (§7.3 step 6); see §2.4.

   The SKILL.md format itself is a Turul-local convention. See §9
   Q2 — `SkillCard` carries a collision risk with A2A `AgentSkill`;
   `SkillManifest` is the leading rename candidate given the
   broader (non-LLM-inclusive) framing.

4. **`SkillDescriptor`.** Returned by `SkillRegistry::describe`,
   carries `id`, the `AgentSkill` discovery card, and `params_schema`
   (`Option<…>`). The schema field is **Turul-local runtime-planning
   metadata** for planners and routers to introspect skill parameters
   in process. It is NOT advertised on the wire, has no place in
   `AgentSkill`, and is not tied to any A2A extension URI.

   **Single source of truth — no drift.** `SkillDescriptor.params_schema`
   is NOT an independent adopter-fillable surface. For manifest-backed
   skills (registered via SKILL.md), `SkillRegistry` derives
   `params_schema` directly from the manifest's input schema (§2.2
   item 3) — they are identical by construction. For non-manifest
   skills (registered programmatically without a SKILL.md), the
   adopter supplies the schema once when registering; `params_schema`
   exposes that same value. The patterns crate guarantees: for any
   given registered skill, `params_schema` is exactly one schema (or
   `None` for schemaless skills), with no second authoritative
   surface. Adopters cannot set a `params_schema` that differs from
   the manifest's input schema.

   If a successor profile ADR (§2.5) chooses to publish schemas on
   the wire, that profile owns the wire shape;
   `SkillDescriptor.params_schema` remains an in-process planning
   helper sourced from the same manifest input schema, never a
   second source of truth.

5. **`SkillError` enum.** Two variants only: `InvalidRequest(String)`
   for client-shape errors that map to A2A `InvalidRequest`, and
   `Internal(String)` for server-side failures that map to A2A
   `Internal`. Adopters needing richer or application-specific error
   shapes wrap or extend in their own error type at the call site.

6. **`TerminalHook` trait + `SkillOutcome` enum** (Q5-resolved
   additive surface). Best-effort post-execution observer fired by
   adopter dispatch code when `SkillHandler::run` returns. Minimal
   generic variant only — no framework-level isolation / timeout /
   panic semantics in this initial form (those depend on the
   dispatcher decision, ADR-022). Binding contract in §9 Q5.

**Keep the public surface tight.** "Patterns" is not a bucket for
every useful idiom. Additions to the crate's public surface require
an amendment to this ADR or a successor ADR.

### 2.3 `SkillHandler` progress sink — `SkillProgressSink` trait

`SkillHandler::run` needs a way to emit per-task progress (status
updates, artifacts) during long-running skill executions. The
framework's existing `EventSink` (in `turul-a2a`) does this for
server-runtime adopters, but coupling `SkillHandler` directly to
`EventSink` would force every consumer of `turul-a2a-patterns` —
including manifest-only adopters that never run a server — to pull
in the full server runtime.

The patterns crate defines a minimal **`SkillProgressSink`** trait
covering the subset of progress-emission semantics skill handlers
actually need. `SkillHandler::run` takes `&dyn SkillProgressSink`.
The trait lives in `turul-a2a-patterns`; `turul-a2a-patterns` MUST
NOT depend on `turul-a2a` even transitively.

**Trait shape — binding contract.** The names, signatures, and
enum variants below are the contract Phase C implements and Phase A
tests assert against. Implementation may vary in private types,
backing storage, and async machinery beyond the boxed-future
strategy below, but the public surface in this sketch is binding —
deviations require an ADR amendment, not a code-only change.

**Revision (Wave 8 ergonomics pass).** The trait shape was amended
from a hand-rolled `Pin<Box<dyn Future + Send + 'a>>` (with a
`SinkFuture<'a, T>` alias) to `#[async_trait]` so adopter impls
read as standard `async fn`. The `SinkFuture` alias is removed —
no caller named it outside the boilerplate it tried to hide.
Object-safety is unchanged (the macro still emits boxed futures
behind the scenes) and is enforced by the same compile-time const
assertion. The same revision applies to `TerminalHook` in §9 Q5.

```rust
// crates/turul-a2a-patterns/src/sink.rs
/// Non-terminal task states a skill may emit during execution.
/// Terminal states (Completed, Failed, Canceled, Rejected) are
/// **not** representable here — they are framework-owned and
/// committed by the dispatcher based on the skill's `Result`.
#[non_exhaustive]
pub enum ProgressState {
    Working,
    InputRequired,
    AuthRequired,
}

/// Sink-side error variants. Skills receive these as `Err(...)`
/// from sink calls and decide whether to ignore, retry, or unwind.
#[non_exhaustive]
pub enum SinkError {
    /// The sink's underlying task is closed (terminal state already
    /// committed by the framework, or cancellation observed). Skill
    /// SHOULD treat this as cooperative cancellation and unwind —
    /// further sink calls will also return `Closed`.
    Closed,
    /// Transient backend failure (network blip, store throttle, CAS
    /// loss). Best-effort: skill MAY ignore, retry, or abort at its
    /// discretion. The framework itself logs and proceeds.
    Backend(String),
}

#[async_trait::async_trait]
pub trait SkillProgressSink: Send + Sync {
    async fn set_status(&self, state: ProgressState, message: Option<Message>)
        -> Result<(), SinkError>;
    async fn emit_artifact(&self, artifact: Artifact, append: bool, last_chunk: bool)
        -> Result<(), SinkError>;
    fn is_closed(&self) -> bool { false }
}
```

Contract details:

- **Async strategy**: `#[async_trait]` macro. Rationale: adopter
  ergonomics over zero-dep purity — `async fn` in the impl reads
  as standard Rust instead of leaking `Box::pin(async move { … })`
  boilerplate into every consumer. The macro produces an
  object-safe trait with `Send` on the returned future (the
  default `?Send` variant is explicitly NOT used here), so
  `tokio::spawn` over `&dyn SkillProgressSink` still works. AFIT
  was rejected because, on current stable, it doesn't let callers
  name the `Send` bound on the returned future, which would
  silently break spawning.
- **Send + Sync bounds**: `Send + Sync` on the trait (handlers cross
  `tokio::spawn` and are shared via `&dyn`); `async_trait` adds
  `+ Send` to the desugared future. `Sync` on the future is not
  required.
- **Lifetimes**: `async_trait` desugars `&self` async methods to a
  future that borrows `&self` for as long as needed, so
  framework-internal sinks holding `&Arc<…>` work without `'static`
  bounds. The previous explicit `'a` was incidental to the
  hand-rolled boxed-future shape and is no longer needed.
- **Sink failure semantics — best-effort**. A progress write failing
  (transient store throttle, network blip, CAS loss) is *not* a
  correctness problem for the skill's actual work. Skill authors
  receive the `Result` and may `?`-propagate if they want abort
  semantics; the framework itself logs and proceeds. The one signal
  worth structured handling is `SinkError::Closed`, which skills
  SHOULD treat as cooperative cancellation and unwind.
- **Terminal commits are NOT exposed — enforced by type.** The
  `set_status` signature accepts `ProgressState`, a non-exhaustive
  enum that contains **only** non-terminal states (`Working`,
  `InputRequired`, `AuthRequired`). Terminal states (`Completed`,
  `Failed`, `Canceled`, `Rejected`) are not constructible through
  this trait. They belong to the framework's dispatcher: the
  contract is skill returns `Result<Value, SkillError>`; the
  dispatcher translates `Ok` → artifact + complete, `Err` → fail.
  This type-level enforcement prevents skills from breaking the
  "one terminal commit per task" invariant; a runtime-reject
  alternative (a hypothetical `SinkError::Invalid` for terminal
  states) was rejected as strictly less safe.
- **Object safety**: enforced by a compile-time assertion in the
  crate:

  ```rust
  const _: fn() = || {
      fn assert<T: ?Sized + SkillProgressSink>() {}
      assert::<dyn SkillProgressSink>();
  };
  ```

  Required by §4.4 test coverage.

**Bridge to `turul-a2a` — where it lives, and why:**

A direct `impl SkillProgressSink for EventSink` cannot live in the
example crate during Phase C: Rust's orphan rule forbids
implementing an external trait (`SkillProgressSink` — in
`turul-a2a-patterns`) for an external type (`EventSink` — in
`turul-a2a`) from a third crate (the example). The compiler rejects
this.

Phase C uses a **local newtype adapter** in the example crate:

```rust
// In examples/skill-manifest-ollama-agent/src/main.rs (or sink.rs):
struct ExampleProgressSink(EventSink);

#[async_trait::async_trait]
impl turul_a2a_patterns::SkillProgressSink for ExampleProgressSink {
    async fn set_status(&self, state: ProgressState, message: Option<Message>)
        -> Result<(), SinkError>
    {
        // Map ProgressState → TaskState, delegate to self.0 (EventSink).
        …
    }
    async fn emit_artifact(&self, artifact: Artifact, append: bool, last_chunk: bool)
        -> Result<(), SinkError>
    {
        // Delegate to self.0.append_artifact(…) / emit_artifact(…).
        …
    }
}
```

`ExampleProgressSink` is local to the example crate, so the impl is
legal. The example's dispatch code wraps the framework-provided
`EventSink` in `ExampleProgressSink` before calling
`handler.run(params, &example_progress_sink)`. The example
demonstrates the full wiring — adopters copying the example see
both the bridge pattern AND the requirement for downstream code to
use newtypes (because orphan rules apply equally to *any* third
party impl).

The bridge `impl SkillProgressSink for EventSink` directly (no
newtype) becomes legal **only inside `turul-a2a` itself**, because
`EventSink` is local to `turul-a2a`. That direct impl is added to
`turul-a2a` when §4 gates clear and `turul-a2a-patterns` flips to
publishable; both ship together in the §4-clearance release. Until
then the direct impl is impossible regardless of which crate tries
to write it.

**Adopter consequences (during Phase C, before §4 clears):**

- **Manifest-only consumers** (CLI tools, static analysers,
  build-time AgentCard generators, IDE tooling) consume
  `turul-a2a-patterns` from inside the workspace via path-dep. No
  server runtime needed.
- **Server-runtime use** is workspace-internal only: examples,
  tests, and downstream adopters consuming via workspace path. Real
  cross-repo server adopters wait for §4 to clear.
- **Test harnesses** construct stub `SkillProgressSink` impls
  directly — no `turul-a2a` runtime, no bridge required.

**Alternatives rejected:**

- **Generics on `SkillHandler`** (`<S: SkillProgressSink>`). Breaks
  trait-object friendliness, which `SkillRegistry` requires for
  heterogeneous registration. `&dyn SkillProgressSink` keeps
  `SkillHandler` object-safe.
- **Move `EventSink` to `turul-a2a-types`**. `EventSink` is a
  server-runtime concept (per-task streaming sink); moving it would
  require its own ADR and a `turul-a2a-types` minor bump for no
  benefit.
- **Move `SkillProgressSink` trait to `turul-a2a-types`** (the
  diamond pattern). Rejected: ADR-002 constrains `-types` to
  "ergonomic Rust wrappers over proto types" plus the `TaskState`
  state machine. A behavior trait emitting proto-defined types is
  not a wrapper; adding it would require an ADR-002 amendment.
  `state_machine.rs` is not a precedent — it's pure validation
  helpers over a proto enum, not a runtime trait.
- **Co-publish patterns + turul-a2a immediately** to dissolve the
  publish constraint. Rejected: discards §4's load-testing
  discipline (the precedent set by ADR-020); claims a crates.io
  name and commits an API surface before two real adopters have
  load-tested it. Pre-1.0 SemVer is *not* a substitute for the §4
  gate; advertised crates create implicit stability expectations
  regardless of the `0.x.y` prefix.

### 2.4 Out of scope

The following are deliberately not in `turul-a2a-patterns`:

- **Skill-invocation dispatcher.** The current A2A proto has no
  normative `skill_id` binding on `Message` (§1.3). Any dispatcher convention
  is *wire-affecting* — choosing how a request targets a skill
  changes what clients must send. That places it in the
  profile-extensions bucket, not the patterns bucket. A successor
  ADR will spec the dispatcher under the four-point profile-extension
  contract in §2.5, and the activation hook lives in `turul-a2a`'s
  transport layer per §1.2.
- **LLM client abstractions** (e.g. an `LlmClient` trait with
  per-provider adapters). Orthogonal concern: different audience
  (non-A2A consumers exist), different cadence (LLM provider churn
  is faster than A2A), different dependency surface
  (streaming/tokenization/HTTP/auth specific to each vendor). A
  separate ADR decides whether a `turul-llm-client` crate exists at
  all. A **reference example** under `examples/` wiring
  `SkillCard` manifest support (§2.2 item 3) to Ollama — the natural
  first choice given its structured-output API — is a **Phase C
  deliverable** (§7.3 step 6), not part of the patterns crate's
  public surface. Per §2.2 item 3, the patterns crate's JSON Schema
  validation is authoritative; the Ollama adapter in the example may
  down-convert schemas to whatever subset its provider accepts. The patterns crate itself stays
  provider-neutral; Ollama-specific knobs live in the example's
  `provider_config:` block per §2.2 item 3.
- **Application-specific patterns.** Anything tied to a particular
  industry, dataset, vendor metadata namespace, message-bus topology,
  or downstream sink belongs in adopter code. The patterns crate is
  for idioms that show up across A2A agents regardless of domain.
- **Agent role idioms** (planner, router, coordinator, critic,
  gateway/facade). These ARE generic patterns conceptually, and the
  framework intends to provide them — but only with generic
  implementations. The framework will not host role idioms that
  hard-code a specific LLM vendor, tokenizer, or transport. See §9
  Q3 for future scope.

### 2.5 Four-point spec for any successor profile extension

Any successor ADR proposing a wire-affecting profile (e.g. the
deferred skill-invocation dispatcher) MUST specify all four points:

1. **Declaration**: an `AgentExtension { uri, description, required,
   params }` entry in `AgentCapabilities.extensions` (proto L412,
   L418-427). The URI is Turul-owned (e.g.
   `https://turul.dev/a2a/extensions/skill-invocation/v1`).
2. **Activation**: the convention is applied when the client sends
   the request with `A2A-Extensions: <uri>` HTTP header (A2A spec
   §"Extensions"). The server's response SHOULD echo activated URIs.
3. **Request shape**: how the profile's data travels — `Part.data`
   (proto L237, `google.protobuf.Value`), `Message.metadata`
   (`Struct`, L272), or a text-part envelope. Pick one and document.
4. **Rejection**: required extensions the server cannot honour cause
   a documented A2A error; non-required extensions are ignored.

The activation dispatch hook (parse, echo, reject) lives in
`turul-a2a`'s transport layer per §1.2 — **not** in
`turul-a2a-patterns`.

### 2.6 Scope table

| Component | In `turul-a2a-patterns` | Public 0.1.x release | Notes |
|---|---|---|---|
| `SkillHandler` trait + `SkillProgressSink` trait | yes | when §4 gates clear | Per §2.3: `SkillHandler::run` takes `&dyn SkillProgressSink`. During Phase C the example crate provides a **newtype adapter bridge** (`struct ExampleProgressSink(EventSink)` + `impl SkillProgressSink for ExampleProgressSink`) — a direct `impl SkillProgressSink for EventSink` from the example would violate Rust's orphan rule (both trait and type external to the example). The direct impl on `EventSink` is added inside `turul-a2a` only after §4 gates clear (§4.1 publish action), because `EventSink` is local to `turul-a2a` there. |
| `SkillRegistry` + `InMemorySkillRegistry` | yes | when §4 gates clear | |
| `SkillCard` + SKILL.md manifest helpers (parsing, AgentSkill generation, prompt rendering, schema validation) — supports both LLM-backed and non-LLM skills | yes | when §4 gates clear | Eight-field AgentSkill discovery (incl. `security_requirements` per ADR-015). Trust model and contracts in §2.2 item 3. Naming candidate in §9 Q2 — `SkillManifest` is the leading rename given the execution-model-agnostic scope. |
| `SkillDescriptor` | yes | when §4 gates clear | Per §2.2(4): includes `params_schema` as Turul-local planning metadata, not on the wire. **Single source of truth (no drift)**: for manifest-backed skills, derived from the manifest input schema; for programmatic skills, supplied once at registration. Never a second adopter-fillable surface. |
| `SkillError` (two variants) | yes | when §4 gates clear | per §2.2(5) |
| Skill-invocation dispatcher | **no** | requires successor profile ADR per §2.4, §2.5 | Wire-affecting; not a pattern. |
| `LlmClient` / vendor adapters | **no** | separate ADR per §2.4 | Orthogonal concern. |
| Agent role idioms (planner, coordinator, critic, …) | **no, not initial** | future amendment | §9 Q3. Generic implementations only. |
| Application-specific patterns | **no** | no | Out of scope by definition. |
| Test coverage | yes — required from day 1 | required | TDD per CLAUDE.md. |
| Cross-client wire-interop verification (Python / Go / Rust under `examples/clients/`) | Phase C gate per §7.3 step 8 | required | Three client crates + `CLIENT_MATRIX.md`. Python uses `a2a-sdk` 1.0 (PyPI; verified 1.0.2); Go uses official `a2aproject/a2a-go/v2` (verified v2.3.1); Rust uses `turul-a2a-client`. Hermetic CI (no live Ollama). Stop-and-amend conditions documented in §7.3 step 8. |

## 3. Non-Goals

- Defining a normative skill-invocation wire format. Out of scope
  until A2A v1.x binds `skill_id` on `Message` or §2.5 ships via a
  successor ADR.
- Hosting an `LlmClient` abstraction. Separate ADR.
- Hosting any application-specific patterns.
- Modifying any existing `turul-a2a` / `turul-a2a-types` /
  `turul-a2a-proto` types. The new workspace member is purely
  additive.
- Publishing the new crate. Path-only workspace dep until §4 gates
  clear.

## 4. Pre-publish gates

> **Status update (2026-05-23):** ADR-021 §4 gates explicitly
> overridden as part of the 0.1.26 release. The crate flips from
> `publish = false` to publishable; the workspace dep declaration
> gains a `version` field; `.claude/agents/adr-review.md` no longer
> BLOCKERs the `publish = false` invariant.
>
> Rationale for the override (chosen over a strict gate-by-gate
> read of §4):
>
> - **ADR-022 Accepted with `A2A-Extensions` activation wired into
>   `turul-a2a`'s transport** — §4 gate #2 (dispatcher decision)
>   cleanly satisfied.
> - **ADR-023 / ADR-024 / ADR-025 in place** — the LLM-client
>   abstraction, the typed-handler design sketch, and the deferred
>   composition patterns each have their own ADR. The surface
>   `turul-a2a-patterns` exposes is no longer the only place an
>   adopter has to reason about where to put what.
> - **Five showcase examples + 18-cell interop matrix** —
>   `skill-manifest-ollama-agent`, `agent-role-planner-router-agent`,
>   `agent-role-critic-agent`, `post-task-hook-agent`, and
>   `skill-dispatch-profile-agent` collectively exercise every §2.2
>   public surface end-to-end across Python / Go / Rust clients.
>   This is treated as the practical equivalent of the §4 gate #1
>   "two real adopters" requirement: not two external adopters, but
>   five in-workspace consumers with cross-language clients
>   verifying the wire contract.
> - **§4 gate #3 (test coverage)** — 26 tests across 8 test files
>   cover the five §2.2 public surfaces. Verified clean at release
>   time.
> - **§4 gate #4 (downstream impact survey)** — no formal survey
>   was executed; the crate has never been published, so there is
>   no downstream to migrate. The override acknowledges this gate
>   is not strictly satisfied and accepts the consequence: future
>   `0.1.x → 0.2.0` breakage will be visible in the changelog and
>   adopter migration will happen at that boundary, not now.
>
> The original §4 text below is preserved as historical context.

These gates flip the crate from path-only to publishable. Each is
independently observable; none is satisfied by this ADR.

1. **Two real adopters.** At least two non-toy projects use the
   crate's surfaces. The crate name and API contract should be
   load-tested by more than one consumer before crates.io
   registration (which is irreversible — yank only hides).
2. **Dispatcher decision made.** Either (a) successor profile ADR
   Accepted per §2.4/§2.5 with `A2A-Extensions` activation wired into
   `turul-a2a`'s transport, or (b) dispatcher stays in adopter code
   permanently and the published crate exposes no dispatcher surface.
3. **Test coverage for the §2.2 public surfaces.** TDD from day 1
   covering all five surfaces, including `SkillHandler` under §2.3's
   `SkillError` shape, the four `SkillCard` manifest helpers
   (parsing, AgentSkill generation, prompt rendering, schema
   validation), and a static object-safety assertion on
   `SkillProgressSink` (per §2.3 contract).
4. **Downstream impact survey.** Per ADR-020 precedent: enumerate
   first-party consumers and migration cost before flipping this ADR
   from Proposed to Accepted-with-publish.

The `LlmClient` decision (whether `turul-llm-client` exists as a
separate crate) is **NOT** a §4 gate. With the execution-model-
agnostic manifest framing (§2.2 item 3), `turul-a2a-patterns` does
not call models and is unaffected by whether `LlmClient` ships. The
question is informational; tracked as §8 trigger context, not a
publish prerequisite.

### 4.1 Post-gate publish action (not a gate)

When all four gates above are independently observed as cleared, a
**single release event** delivers all of the following together —
this is the *action* the gates unlock, not a fifth gate:

- `turul-a2a-patterns` `Cargo.toml` flips `publish = false` →
  `publish = true` (`version` already inherited from
  `[workspace.package]` per §2.1, no change needed in the crate
  itself).
- Root `[workspace.dependencies]` entry gains the `version` field
  so it becomes
  `turul-a2a-patterns = { version = "X.Y.Z", path = "crates/turul-a2a-patterns" }`
  (publishable crates require an explicit version even when path
  is also present; cargo uses the path locally and the version on
  crates.io). The CLAUDE.md `{ workspace = true }` rule is
  unchanged.
- `turul-a2a` `[dependencies]` gains
  `turul-a2a-patterns = { workspace = true }` per CLAUDE.md
  workspace-dependency house rule.
- `turul-a2a` source gains `impl SkillProgressSink for EventSink`
  (legal because `EventSink` is local to `turul-a2a`). The
  newtype adapter in the example crate (`ExampleProgressSink`,
  per §2.3, §7.3 step 6) is kept as documentation but is no longer
  load-bearing.
- Both crates publish in the same release per CLAUDE.md
  §"Release & Publish" dependency order (`turul-a2a-patterns`
  before `turul-a2a`).

`CHANGELOG.md` records the release with explicit reference to the
§4 gates having cleared and the orphan-rule bridge migration.

## 5. Future profile/extensions crate — triggers and naming

A separate profile/extensions crate is approved **in principle**
when ALL of:

1. The successor dispatcher ADR is **Accepted** with §2.5's
   four-point profile spec.
2. At least **two** declared profile extensions exist to inhabit the
   crate.
3. At least one of those profiles has a second adopter (mirrors §4.1).

**Where the *first* profile lives (single-profile case):** **NOT**
in `turul-a2a-patterns` — putting a wire-affecting profile inside the
patterns crate contradicts the patterns/wire boundary set in §1.2. Place it in one of:

- **`turul-a2a`** itself, alongside the transport-layer activation
  hook it requires (§1.2). A `pub(crate)` module with rustdoc
  flagging it as a candidate for extraction is fine.
- **Adopter code**, if the profile is not yet general enough for
  `turul-a2a` itself.

Either placement keeps the URI/schema next to the activation code
that consumes it. Promotion to the new crate happens at the §5-gate
moment in a single ADR-and-commit pair.

Name candidates for the future crate, ranked:

| Candidate | Rationale | Risk |
|---|---|---|
| `turul-a2a-profiles` | Matches A2A spec's "profile extension" classification. | "Profiles" is overloaded in identity / security contexts. |
| `turul-a2a-extensions` | Matches proto field name `AgentCapabilities.extensions`. | Collision risk: adopters may read it as "helpers for the `AgentExtension` type". |
| `turul-a2a-wire-ext` | Honest about wire impact. | Abbreviation breaks the `turul-a2a-{role}` convention. |

The successor ADR makes the final naming call. `turul-a2a-profiles`
is the current preference.

## 6. Workspace role and adopter mental model

`turul-a2a-patterns` is a **foundational abstraction crate**. State
this in the crate's `lib.rs` rustdoc:

> `turul-a2a-patterns` defines reusable abstractions for A2A skill
> authoring — the `SkillHandler` trait, `SkillRegistry`, `SkillCard`
> (SKILL.md) helpers, and the `SkillProgressSink` trait. (`SkillCard`
> may be renamed to `SkillManifest` at implementation time; see
> ADR-021 §9 Q2.) It depends only on `turul-a2a-proto` and
> `turul-a2a-types`, never on the server runtime. It does NOT
> change the A2A wire contract; profile/extension machinery lives
> elsewhere (see ADR-021 §1.2, §2.4, §5).

Adopter mental model — four workspace roles:

- **Wire-type crates** define what travels on the wire:
  `turul-a2a-proto`, `turul-a2a-types`, and a future profile crate
  (URI constants + schemas).
- **Abstraction/foundation crates** define trait-based patterns
  adopters and transports both build on: `turul-a2a-patterns`.
  Depends on wire-type crates only; never on wire-I/O.
- **Wire-I/O crates** implement transport: `turul-a2a` (server
  router; owns `A2A-Extensions` activation per §1.2),
  `turul-a2a-client`, `turul-a2a-aws-lambda`. Depend on wire-type
  crates today; `turul-a2a` gains a dependency on
  `turul-a2a-patterns` and adds
  `impl SkillProgressSink for EventSink` only **after §4 gates
  clear** (per §2.3). During Phase C, example crates use local
  newtype adapters (e.g. `struct ExampleProgressSink(EventSink)`)
  because Rust orphan rules forbid the direct impl from a third
  crate.
- **Auxiliary / cross-cutting crates** layer above transport for
  concerns like authentication: `turul-a2a-auth`. Depends on
  `turul-a2a`.

Allowed dep direction (lower → higher): proto/types →
patterns → turul-a2a / client / aws-lambda → auth / examples /
applications. Anything else (e.g. patterns → turul-a2a) is
forbidden per §2.3.

## 7. From Proposed to Accepted to implemented

This ADR moves through three phases, each gated. Per CLAUDE.md
guidance ("the ADR should be accepted before implementation starts"),
acceptance and implementation are **separate commits**.

### 7.1 Phase A — Acceptance gates (no implementation code)

**A2A Spec Compliance Agent:**

- Re-confirm vendored `proto/a2a.proto` SHA256 matches upstream
  `a2aproject/A2A:main/specification/a2a.proto`.
- Re-verify §1.2 / §1.3 / §2.5 proto-and-spec claims: no `skill_id`
  on `Message`; `Part.data` is `google.protobuf.Value`; `AgentSkill`
  has no schema field; `AgentExtension` is at `AgentCapabilities`,
  not per-skill; `A2A-Extensions` HTTP header is the activation
  channel, `Message.extensions` is attribution.
- Sign-off statement: "ADR-021 is spec-truthful as of proto SHA256
  `<hash>`."

**Test Writer Agent:**

- Produces a red-phase test suite for the §2.2 public surfaces
  (including the four `SkillCard` manifest helpers from §2.2 item 3:
  parsing, AgentSkill generation, prompt rendering, schema
  validation). The suite compiles on a sketch branch and fails
  against `main` (the surfaces do not exist yet). Tests live in
  `crates/turul-a2a-patterns/tests/` once the crate exists; before
  then they are reviewed in branch form alongside this ADR.

### 7.2 Phase B — Acceptance commit

When both §7.1 sign-offs land:

- Single commit on `main`:
  `docs(adr): accept ADR-021 (turul-a2a-patterns)`.
- Flips this ADR's Status: Proposed → Accepted.
- No code, no `Cargo.toml` changes, no new directory. The acceptance
  commit is documentation-only and captures the design lock.

### 7.3 Phase C — Implementation (post-acceptance)

A sequence of commits after Phase B. All steps below are
Phase C deliverables — none is optional or follow-up.

1. Create `crates/turul-a2a-patterns/` (`publish = false`,
   `version.workspace = true`).
2. Add path-only `[workspace.dependencies]` entry.
3. Apply the `lib.rs` rustdoc preamble from §6.
4. Implement the five surfaces from §2.2 with §2.3 coupling.
5. Land the §7.1 red-phase test suite under
   `crates/turul-a2a-patterns/tests/` and drive it green.
6. **Reference example deliverable.** Add a new **workspace
   package** at `examples/skill-manifest-ollama-agent/`
   (`publish = false`). This is its **own crate**, not part of a
   shared "examples" mega-crate; the repo's convention is one
   workspace package per example (`examples/echo-agent`,
   `examples/auth-agent`, `examples/grpc-agent`,
   `examples/lambda-agent`). Provider-specific dependencies — Ollama
   HTTP client, anything Ollama-side — live in **this example's**
   `Cargo.toml`, not in `turul-a2a-patterns`.

   Recommended directory shape:

   ```
   examples/skill-manifest-ollama-agent/
     Cargo.toml          # publish = false
     README.md           # how to run; OLLAMA_BASE_URL / RUN_OLLAMA_SMOKE docs
     skills/demo/SKILL.md  # a real manifest file using the §2.2 item 3 shape
     src/main.rs         # A2A agent binary; registers the skill via SkillRegistry
     tests/smoke.rs      # offline-by-default smoke test
   ```

   The example demonstrates the full pattern end-to-end:

   - A real `SKILL.md` file using the §2.2 item 3 three-section
     frontmatter (discovery fields, provider-neutral execution
     metadata, opaque `provider_config:` block).
   - Parsing the manifest via the patterns crate; generating the
     `AgentSkill` for `AgentCard.skills`.
   - **The progress-sink bridge** lives in this example crate
     during Phase C (per §2.3). Because Rust orphan rules forbid
     implementing an external trait for an external type, the
     example uses a local newtype: `struct ExampleProgressSink(EventSink)`
     with `impl SkillProgressSink for ExampleProgressSink`. The
     example's dispatch wraps the framework-supplied `EventSink` in
     this newtype before calling the skill handler. When §4 gates
     clear, `turul-a2a` adds the direct
     `impl SkillProgressSink for EventSink` (legal because
     `EventSink` is local to `turul-a2a`) and the example's newtype
     becomes optional documentation.
   - An A2A agent binary that registers the skill via
     `SkillRegistry`.
   - On invocation: render the prompt template against the input
     parameters, validate input against the manifest's input schema,
     call Ollama with the rendered prompt and JSON Schema for
     structured output, validate the response against the output
     schema, return the structured result through A2A.

   **Test gating — load-bearing for hermetic CI.** Default
   `cargo test --workspace` (the §7.4 gate every other crate
   passes) MUST NOT require a live Ollama instance. The example's
   `tests/smoke.rs` runs the offline surfaces by default — manifest
   parsing, `AgentSkill` generation, prompt rendering, input/output
   schema validation, the A2A registration handshake — using
   recorded fixtures or a stubbed model adapter where needed. The
   **live** Ollama call is opt-in, gated on either
   `OLLAMA_BASE_URL` being set or `RUN_OLLAMA_SMOKE=1`. The
   example's `README.md` documents both modes.

   This example is the framework's load-bearing demonstration that
   the patterns crate's surfaces are usable end-to-end. Without it,
   the §2.2 surface contract is theoretical.
7. **Downstream-adopter migration verification.** A private
   downstream adopter migrates at least one of its existing
   manifest/parser-glue skills onto `turul-a2a-patterns` and
   confirms the integration builds and runs. The verification is
   recorded (commit message, branch reference, or short migration
   note in the adopter's tree) but the adopter is **not named** in
   any framework artifact (ADR, commit message, CHANGELOG).
   Phrasing on the framework side: "ADR-021 Phase C verified by a
   private downstream adopter." This is a Phase C completion
   criterion, distinct from §4.1's "two adopters" publish gate.
8. **Cross-client wire-interoperability verification.** Add three
   client workspace packages under `examples/clients/` plus an
   interoperability matrix document. This is a **binding Phase C
   gate** — the framework's value claim of cross-language agent
   talk is theoretical without it.

   **Required client crates:**

   - `examples/clients/python/` — Python client built on the A2A
     SDK v1.0 (`a2a-sdk` package). **Verify the SDK's current API
     against authoritative sources** (Context7 / official docs /
     PyPI) BEFORE writing client code. Record the verified SDK
     version in the client's README.
   - `examples/clients/go/` — Go client. **Research result (resolved
     during Phase C):** the official `github.com/a2aproject/a2a-go/v2`
     SDK ships A2A v1.0 Spec-compliant; v2.3.1 was released
     2026-05-13 by the A2A Project org (same upstream that owns the
     normative proto this workspace vendors). The Go client uses
     that SDK — no hand-roll. The "hand-roll fallback" path remains
     in this ADR's stop-conditions (below) only as a guard against a
     future SDK going stale, not as the current implementation
     strategy.
   - `examples/clients/rust/` — Rust client built on
     `turul-a2a-client` (already in the workspace).

   **Required scenarios for every client (all three):**

   - `GET /.well-known/agent-card.json` — agent-card fetch (all
     three clients use the same discovery route, per proto
     `google.api.http` annotations; camelCase JSON per proto JSON
     mapping).
   - **Message send** — each client posts via the transport its
     SDK selects from the `AgentCard.supportedInterfaces[]`
     advertisement. A2A v1.0 permits **multiple transport
     surfaces** for the same operation; the framework's `A2aServer`
     serves both REST and JSON-RPC at the same time. Clients differ:
     - **Python (`a2a-sdk` 1.0.2)** selects JSON-RPC and posts to
       the card-advertised `/jsonrpc` with a JSON-RPC envelope
       (`SendStreamingMessage` when `Accept: text/event-stream`).
     - **Go (`a2aproject/a2a-go/v2`)** selects JSON-RPC and posts to
       `/jsonrpc` with `SendMessage` (non-streaming by default).
     - **Rust (`turul-a2a-client`)** posts REST `POST /message:send`
       per the proto `google.api.http` annotation.
     
     The cross-client gate verifies that the framework's
     `A2aServer` handles BOTH transport surfaces correctly and that
     each client's SDK round-trips its preferred transport without
     wire-contract changes. Response shape per transport is whatever
     the spec defines: `SendMessageResponse` containing `Message` /
     `Task` per the Life-of-a-Task taxonomy (REST), or JSON-RPC
     `{"result": …}` framing carrying the same payload (JSON-RPC),
     or SSE-chunked JSON-RPC for streaming. See
     `examples/CLIENT_MATRIX.md` for the per-client breakdown.
   - Run against **two example servers**:
     - **(a)** an existing basic example such as `echo-agent` (the
       canonical "framework still works" smoke).
     - **(b)** `skill-manifest-ollama-agent` in **offline mode
       only**, i.e. without a live Ollama instance.

   **Hermetic-CI invariant — load-bearing.** Cross-client tests
   MUST NOT require a live Ollama (or any other external network
   dependency). Default `cargo test --workspace` and the §7.4 gate
   stay hermetic. The live-Ollama smoke from §7.3 step 6 remains
   env-gated (`OLLAMA_BASE_URL` / `RUN_OLLAMA_SMOKE=1`) and is
   never exercised by cross-client tests.

   **`examples/CLIENT_MATRIX.md` — required deliverable.**
   Documents (a) each client's underlying SDK (or "hand-rolled, no
   1.0 SDK") with version, (b) which scenarios each client covers,
   (c) any A2A wire-contract details the client demonstrates
   (routes, fields, error model). Acts as the cross-language
   interop reference adopters consult before adding a new client.

   Each client crate ships a **novice-focused README** per the
   Phase C documentation pass: introduces A2A briefly, lists
   prerequisites, shows expected output, names common failure
   modes (e.g. server not running, wrong port).

   **STOP CONDITIONS — surface for ADR amendment, do NOT
   workaround silently:**

   - Python `a2a-sdk` 1.0 is not actually published, or its
     current API is incompatible with the wire shape ADR-021
     assumes. Surface the incompatibility before committing
     client code.
   - Go has no 1.0-class SDK AND the hand-roll surfaces an
     ambiguous or missing wire-contract field. Surface for review.
   - Any client's agent-card fetch or `message:send` requires a
     **wire-contract change to `turul-a2a`** to succeed. That
     indicates the framework, not the client, is wrong — stop and
     amend the framework's wire surface (or the proto if upstream
     has drifted) before continuing.
9. Run §7.4 gate.
10. Bump version per §7.5; update `CHANGELOG.md`.
11. Single commit (or short sequence):
    `feat(patterns): add turul-a2a-patterns skill-pattern crate (ADR-021)`
    plus
    `feat(examples): add skill-manifest-ollama-agent reference example (ADR-021)`
    plus
    `feat(examples): add cross-client interop verification (ADR-021)`.

### 7.4 Test Executor gate

- `cargo test --workspace`
- `cargo test -p turul-a2a-patterns`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`
- `cargo check --workspace` (no `--features grpc`) — must stay
  tonic-free (ADR-014 §2.2).

Reports pass/fail. Does not modify source. Does not decide scope.

### 7.5 Version bump

- **Patch bump** when implementation lands (Phase C). The change is
  additive (new internal workspace member); no existing publishable
  crate's API changes. Matches CLAUDE.md SemVer rule (patch =
  compatible runtime; no contract change in any published crate).
- **`CHANGELOG.md` entry required** per user's global versioning rule.
  The entry names the new workspace member, its `publish = false`
  status, and the ADR-021 reference.
- A future **minor bump** applies when the crate flips to publishable
  per §4 gates — that *is* a contract change (new published API).
- A future **major bump** is not anticipated; the crate is pre-1.0
  during its path-only lifetime.

## 8. Triggers for revisiting

- Upstream A2A proto adds `skill_id` to `Message` → §2.4 reopens;
  dispatcher becomes viable as a single ADR rather than a profile
  extension.
- A second adopter requests reuse → §4.1 satisfied; begin
  publish-readiness workflow.
- A2A community converges on a competing skill-invocation convention
  (or comparable agent-pattern conventions outside A2A) → evaluate
  whether to align or front-run.
- A second LLM-provider adopter requests a unified client surface →
  triggers the separate `LlmClient` ADR; does not affect this one.
- A generic implementation of an agent role idiom (planner,
  coordinator, critic, …) becomes available without vendor lock-in
  → amend §2.2 to expand the public surface (§9 Q3).

## 9. Open Questions (do not block Proposed → Accepted)

- **Q1**: Should `turul-a2a-patterns::prelude::*` re-export a
  curated subset of `turul-a2a-types`? A prelude over `SkillHandler`,
  `SkillRegistry`, `SkillCard` is likely the right shape; defer
  pending adopter feedback.
- **Q2**: Should the type currently named `SkillCard` be renamed at
  implementation time to reduce collision risk with A2A `AgentSkill`?
  Given the manifest's execution-model-agnostic framing in §2.2
  item 3 (first-class for both LLM-backed and non-LLM skills),
  **`SkillManifest`** is the leading candidate — it describes
  purpose (a manifest that declares and configures a skill,
  whatever its execution model), matches the industry SKILL.md
  convention, and avoids the AgentSkill collision. Alternatives:
  `SkillMarkdown` (describes format only, but file extension
  isn't necessarily `.md`), `SkillFile` (too generic). Decision
  lives at Phase C (§7.3); the chosen name is recorded in the
  implementation commit message.
- **Q3**: Which agent role idioms (planner, router, coordinator,
  critic, gateway/facade) make sense to ship in
  `turul-a2a-patterns`, and when? **Today the answer is "none yet":
  the patterns crate ships ZERO role abstractions as v1 public API.**
  Planner, router, critic, post-task-hook patterns are demonstrated
  via `examples/agent-role-planner-router-agent/`,
  `examples/agent-role-critic-agent/`, and
  `examples/post-task-hook-agent/` — adopter-visible reference
  implementations only, NOT framework crate types. Each example is
  self-contained code that uses `SkillRegistry` + `SkillHandler`
  + (optionally) `TerminalHook` to *demonstrate* the role pattern;
  no `Planner` / `Router` / `Critic` trait exists in
  `turul-a2a-patterns`. Future amendments may promote role
  abstractions into the patterns crate if (a) a generic
  implementation emerges that is not bound to a specific LLM vendor,
  tokenizer, or downstream sink, AND (b) at least two adopters need
  the same shape. The patterns crate will not host vendor-coupled
  role implementations.
- **Q4**: When the successor dispatcher profile ADR is drafted, does
  its extension URI carry the version in the path (`/v1`) or in
  `AgentExtension.description`? Defer.
- **Q5 (resolved in scope; richer variant deferred)**: Post-commit
  hook patterns split into two variants. The **minimal generic
  variant** — a `TerminalHook` trait + `SkillOutcome` enum, fired by
  adopter dispatch code when `SkillHandler::run` returns, no
  framework-level isolation/timeout semantics — IS in Phase C scope
  for `turul-a2a-patterns` and ships with this ADR. The **richer
  variant** — extractor registries with framework-level
  isolation/timeout/panic-bound semantics — remains parked: its
  general usefulness depends on the framework owning a dispatcher
  (§2.4), and dispatcher ownership is the subject of the successor
  profile-extension ADR. Reopen the richer variant once the
  dispatcher question is settled.

  **Minimal variant contract (binding):**

  - `TerminalHook: Send + Sync` declared with `#[async_trait]` and
    one method
    `async fn on_terminal<'a>(&self, skill_id: &'a str, outcome: SkillOutcome<'a>)`.
    (Revision: amended from a hand-rolled
    `Pin<Box<dyn Future<Output = ()> + Send + 'a>>` to `async fn`
    in the Wave 8 ergonomics pass — see §2.3 revision note.
    Rationale and trade-offs are identical to the `SkillProgressSink`
    decision.)
  - `SkillOutcome<'a>` is `#[non_exhaustive]` with `Success(&'a Value)` and `Failure(&'a SkillError)`.
  - Hooks are **best-effort observers** — the return type is `()`;
    hook failures MUST NOT abort the surrounding execution. Adopters
    that need timeout/panic isolation wrap the hook call themselves
    (`tokio::time::timeout`, `tokio::task::spawn`); the patterns crate
    does NOT impose those semantics in the minimal variant.
  - Object-safety verified by a compile-time `const _` assertion in
    the crate. The dispatcher-owned variant with framework isolation
    is a future amendment, gated on the successor dispatcher ADR.
