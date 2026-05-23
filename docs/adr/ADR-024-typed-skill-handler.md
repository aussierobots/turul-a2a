# ADR-024: TypedSkillHandler trait sketch

- **Status:** Proposed
- **Date:** 2026-05-23
- **Implements:** nothing — this ADR is a design sketch, NOT a
  commitment. No code lands while this ADR remains Proposed.
- **Depends on:** ADR-021 (patterns crate scope), ADR-022 (current
  single-skill dispatcher placement).

## 1. Context

`turul-a2a-patterns` ships `SkillHandler::run(&self, params: serde_json::Value, sink: &dyn SkillProgressSink) -> Result<serde_json::Value, SkillError>`.
The `Value` plumbing is generic and proto-truthful — `Message.metadata`,
`Part.data`, and JSON Schema 2020-12 are all `Value`-shaped at the
wire boundary — but inside a handler that already knows its skill's
schema, the indirection is noise:

```rust
let user_name = params
    .get("user")
    .and_then(|u| u.get("name"))
    .and_then(|n| n.as_str())
    .unwrap_or("friend");
let style = params
    .get("style")
    .and_then(|s| s.as_str())
    .unwrap_or("casual");
```

The `skill-manifest-ollama-agent` example was migrated in this same
release (the slice that produced this ADR) to use typed `GreetInput`
/ `GreetOutput` structs, with four schema round-trip tests that pin
the structs to `SKILL.md`. The result: handlers shrink, the lesson
the example teaches is the *pattern*, not the JSON-walking, and a
new skill author has a working template to copy.

**One example is not enough to commit to a framework primitive.**
The point of this ADR is to keep the design surface visible
without baking it into `turul-a2a-patterns` before the pattern is
proven across multiple skills.

## 2. Sketch (illustrative — not committed)

A possible shape:

```rust
#[async_trait]
pub trait TypedSkillHandler: Send + Sync {
    type Input: serde::de::DeserializeOwned + Send + 'static;
    type Output: serde::Serialize + Send + 'static;

    async fn run_typed(
        &self,
        input: Self::Input,
        sink: &dyn SkillProgressSink,
    ) -> Result<Self::Output, SkillError>;
}

// Blanket: any TypedSkillHandler is automatically a SkillHandler.
#[async_trait]
impl<T> SkillHandler for T
where
    T: TypedSkillHandler,
{
    async fn run(
        &self,
        params: serde_json::Value,
        sink: &dyn SkillProgressSink,
    ) -> Result<serde_json::Value, SkillError> {
        let input: T::Input = serde_json::from_value(params)
            .map_err(|e| SkillError::InvalidRequest(format!("typed input deserialise: {e}")))?;
        let output = self.run_typed(input, sink).await?;
        serde_json::to_value(output)
            .map_err(|e| SkillError::Internal(format!("typed output serialise: {e}")))
    }
}
```

This is the simplest shape that compiles. It is **not** what an
acceptance commit would necessarily ship — the open questions in §5
each have plausible knock-on effects on the trait signature.

## 3. Tradeoffs (honest, both directions)

### What we gain
- Handler bodies shrink to the skill's actual logic. The
  `skill-manifest-ollama-agent` typed migration cut ~25 LOC of
  `Value` walking.
- `Input` / `Output` types appear at the function signature, which
  is the right place for them as documentation.
- Compiler error messages improve substantially when an adopter
  mis-shapes a struct vs the manifest schema.

### What we lose
- **One more abstraction in the patterns crate.** Adopters who
  prefer raw `Value` (e.g. dynamic-shape skills where the input
  schema is data-driven) still have `SkillHandler`, but the
  patterns crate now ships two traits doing similar things.
- **Schema/struct drift risk shifts to the type.** Today the
  example's `validate_input` runs against the manifest schema before
  any typed deserialise. A `TypedSkillHandler` blanket impl that
  skips that step would silently accept inputs the manifest forbids
  (or vice versa). Three options for handling this — none free:
  1. **Test-time only.** Adopters write the same four round-trip
     tests the example carries (`every_example_payload_deserialises`,
     `typed_input_serialises_to_schema_valid_json`, etc.). Cheap,
     but easy to forget.
  2. **Runtime startup check.** The registry verifies that
     `to_value(Input::default())` validates against the manifest's
     `inputSchema` at `register_manifest()` time. Requires `Default`
     on every `Input`, which is fine for trivial cases and awkward
     for variants/enums.
  3. **Compile-time codegen.** `schemars` generates a JSON Schema
     from the struct; build script (or proc-macro) diffs against the
     manifest. Strongest guarantee, biggest dep surface. `schemars`
     is currently NOT in the workspace.
- **`schemars` decision.** If the typed trait grows
  `Input: JsonSchema` bounds to enable §3.2 codegen, the patterns
  crate gains a runtime dep on `schemars`. That changes the crate's
  weight from "trait + a few helpers" to "JSON Schema toolchain."
  Reasonable, but a real commitment.
- **Trait selection ambiguity.** With the blanket impl, a type
  that wants to implement both `SkillHandler` (raw `Value`) and
  `TypedSkillHandler` (typed) gets a conflicting-impl error. Likely
  fine (you pick one), but worth documenting.
- **`#[non_exhaustive]` interaction.** The `JsonSchema` derive
  produces a closed schema by default, so `#[non_exhaustive]`
  structs in adopter code would mismatch a permissive
  `additionalProperties: true` manifest. Resolvable, but a tax.

### What stays the same
- `SkillHandler` is not removed. Raw `Value` handlers continue to
  work; the typed trait is additive.
- `SkillProgressSink`, `SkillRegistry`, `SkillCard`,
  `register_manifest()` — unchanged.
- Manifest is still authoritative. Whatever validation strategy
  §3 lands on, `SKILL.md` remains the source of truth and a
  divergence between schema and struct must fail loudly.

## 4. Promotion criteria — Proposed → Accepted

This ADR transitions to Accepted **only** when all of the following
hold. None is hand-wavable.

1. **At least 2 examples carry typed structs by hand.** The first
   is `skill-manifest-ollama-agent` (in this release). The second
   must arise from a real new skill or refactor, not be manufactured
   to clear the gate. Candidates that might naturally hit it:
   - A typed payload variant of `agent-role-critic-agent`'s
     `validate_against_schema` skill.
   - An entirely new showcase that needs both `Input` and `Output`
     to be structured records.
2. **The repeated boilerplate is documented.** A side-by-side
   listing in this ADR showing the same `from_value` /
   `to_value` / `validate_input` / `validate_output` dance
   appearing in both examples, with the lines that would collapse
   under the trait highlighted. If the two examples diverge in a
   way the trait can't capture, the trait is wrong.
3. **Schema/struct equivalence strategy is decided.** One of
   §3 option 1, 2, or 3, with the tradeoffs documented as a §
   in this file. Not "we'll figure it out later."
4. **One adopter signal.** Either an internal adopter (a real
   service inside the maintainer's portfolio that consumes
   `turul-a2a-patterns`) or an external one (an issue, RFC reply,
   or PR) saying "I would use this." Without that, the trait
   risks being a framework-design exercise.

Until ALL four hold, this ADR stays Proposed. Adding code while it
remains Proposed is **out of scope** for any future slice and
violates ADR-021 §7.

## 5. Open questions

1. **Schema equivalence enforcement.** Test-only, runtime startup
   check, or compile-time codegen? (§3 lays out the tradeoffs.)
2. **Codegen vs hand-coded structs.** `schemars` derives a schema
   from a Rust struct; the manifest carries one written by hand.
   Bidirectional drift detection requires either generating one
   from the other or comparing both against a golden source.
3. **`#[non_exhaustive]` interaction with `JsonSchema` derive.**
   Adopter structs that want to evolve additively conflict with the
   closed-schema default. Workarounds exist
   (`#[serde(other)]`, custom `JsonSchema` impls) — but each is a
   small tax on adopter ergonomics.
4. **Error-type generality.** `SkillError` has two variants
   (`InvalidRequest`, `Internal`). A typed trait might want
   `SchemaViolation` or `Deserialise` as first-class — adding
   variants means revisiting ADR-021 §2.3 (the `SkillError`
   surface).
5. **Input-streaming and output-streaming.** This trait assumes a
   single input value in / single output value out, like
   `SkillHandler`. Streaming inputs (multi-turn refinements) and
   streaming outputs (long generations) both have plausible adopter
   demand and neither fits the simple `Input → Output` shape.
   Almost certainly a separate trait when the time comes.

## 6. Decision

**Proposed.** No code lands.

If and when the §4 criteria are met, a successor ADR (or a
revision-3 of this one) records the chosen schema-equivalence
strategy and the final trait signature. Until then,
`SkillHandler` remains the only adopter-facing handler trait;
typed `Input` / `Output` structs are example-local ergonomics with
round-trip tests pinning them to the manifest.

## Cross-references

- ADR-021 (patterns crate scope) — defines what may live in
  `turul-a2a-patterns`.
- ADR-022 (skill-invocation dispatcher profile) — the wire-side
  partner of "which skill should run." `TypedSkillHandler` would
  affect how a dispatched skill consumes
  `Message.metadata["a2a.skillParams"]`.
- `examples/skill-manifest-ollama-agent/src/main.rs` — the seed
  typed example carrying `GreetInput` / `GreetOutput` and the four
  round-trip tests that pin them to `SKILL.md`.
