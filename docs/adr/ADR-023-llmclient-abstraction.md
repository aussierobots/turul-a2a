# ADR-023: LlmClient Abstraction Decision

- **Status:** Proposed
- **Date:** 2026-05-22
- **Depends on:** ADR-021

## 1. Context

ADR-021 §2.4 explicitly defers the `LlmClient` question: "A separate ADR decides
whether a `turul-llm-client` crate exists at all." The deferral named three
reasons — different audience, faster cadence, different dependency surface — and
directed the LLM call responsibility to the adopter, with a reference example
as evidence the pattern works.

That reference example, `examples/skill-manifest-ollama-agent`, now exists. It
demonstrates the end-to-end pattern:

1. Parse `SKILL.md` via `turul-a2a-patterns` (manifest, schemas, prompt template).
2. Validate input against the manifest's `inputSchema`.
3. Render the prompt template against the call parameters.
4. Call Ollama's `/api/chat` endpoint directly from the example crate, passing
   the rendered prompt and the manifest's `outputSchema` as the `format` field
   for structured output.
5. Validate the response against the manifest's `outputSchema`.
6. Emit the result as an artifact through the `SkillProgressSink` bridge.

The provider-specific code — `reqwest` HTTP client, Ollama request/response
envelope construction, `provider_config` extraction — is entirely contained in
`examples/skill-manifest-ollama-agent/src/main.rs`. The `turul-a2a-patterns`
crate is provider-neutral: it sees a rendered `String` prompt going in and a
`serde_json::Value` coming out, but it does not hold or call either.

This ADR captures the decision about whether the framework should also provide a
`turul-llm-client` crate that standardises the interface between
`turul-a2a-patterns` and model providers.

### 1.1 What a future LLM-backed skill ecosystem would need

Beyond `turul-a2a-patterns`, a framework-level LLM client abstraction would
provide:

- A stable trait surface so adopter skill handlers are not coupled to a specific
  provider SDK (`ollama-rs`, `async-openai`, `anthropic`, etc.).
- A shared integration test harness for asserting schema validation and prompt
  rendering against a stubbed model response.
- A common error taxonomy for LLM-specific failure modes: timeout, schema
  mismatch in the model response, provider rate limiting, context-window
  overflow.
- Optional: a standard retry and token-budgeting policy that providers can opt
  into without duplicating logic per-adapter.

Without such a crate, each adopter independently solves the provider-call layer,
and patterns that emerge across adopters (retry policy, schema down-conversion,
error mapping) are duplicated or copied from the Ollama example.

### 1.2 Current state

The Ollama example is the only in-tree provider integration. The `SkillCard`
manifest's `provider_config:` block is opaque (ADR-021 §2.2 item 3): the
patterns crate does not interpret it. Any provider adapter consumes it with a
direct `serde_json::Value` read.

## 2. Decision

**Option C: Create `turul-llm-client` as a trait-only crate; provider adapters
stay in adopter code or examples — no provider-specific code or dependencies in
the framework crate.**

Rationale follows from weighing each factor.

### 2.1 Cadence

LLM provider HTTP APIs churn materially faster than the A2A spec. Ollama added
the `format` structured-output field mid-2024; OpenAI's `response_format` JSON
schema path went through three incompatible revisions across 2023–2024; Anthropic
switched from per-call auth to prefixed-key schemes. The A2A proto has had one
normative release. Hosting provider adapters in the framework crate couples
framework release cadence to provider-API drift. A trait-only crate is insulated:
the trait shape changes at most when `turul-a2a-patterns`' prompt/schema surface
changes, which tracks the A2A spec, not individual vendor APIs.

### 2.2 Audience

A substantial fraction of `turul-a2a` adopters do not use LLMs at all: Rust-native
handlers, deterministic logic skills, MCP-tool-backed skills, and gateway/facade
roles require only `turul-a2a-patterns` (ADR-021 §2.2 item 3 is execution-model-
agnostic). Even LLM-backed adopters who do want a client abstraction may already
have a preferred SDK (`async-openai`, `ollama-rs`). Shipping a `turul-llm-client`
crate with provider adapters would add transitive compilation weight and churn-
exposure to all adopters. A trait-only crate imposes negligible weight; adopters
who want the trait depend on it, others ignore it.

### 2.3 Coupling

ADR-021 §2.3 is explicit: `turul-a2a-patterns` MUST NOT depend on `turul-a2a`
even transitively. A `turul-llm-client` crate must respect the same constraint: it
sits alongside `turul-a2a-patterns` in the dependency graph (above wire-type
crates, below transport crates), and provider adapters must not reintroduce
transport-crate dependencies. A trait-only crate satisfies this with minimal
surface.

### 2.4 Maintenance

Framework team maintenance burden for provider adapters scales with the number of
providers and their API churn rate. `ollama-rs` and `async-openai` are community-
maintained crates with their own release cycles. Keeping provider adapters in
adopter code or named examples (and accepting community-contributed example crates
under `examples/`) puts maintenance responsibility where knowledge and incentive
live: with the adopter who depends on that provider. The framework team owns the
trait contract; the community owns the adapter code.

### 2.5 Adopter copy path

The `skill-manifest-ollama-agent` example is small (~200 lines including the
`ExampleProgressSink` bridge). An adopter targeting Ollama can copy the provider
dispatch code from that example directly. The copy cost is low and the provider
code is simple. The argument for a shared abstraction strengthens when a second
provider adapter exists and the two reveal a stable shared interface — that has
not happened yet.

### 2.6 Why not A/B/D

**Option A (no crate at all):** The adopter-copy path is sufficient today, but
leaves no stable anchor for multi-provider adopters and no shared error taxonomy
for LLM-specific failures. It also creates no obvious migration path if a second
provider adapter appears and the community wants convergence. Option C costs
little over A (a small trait-only crate) and preserves the future path.

**Option B (trait + provider adapters in the framework crate):** Couples framework
releases to individual vendor API churn. Adds compilation weight for adopters who
do not use LLMs. Puts maintenance burden on the framework team for providers they
may not use. Rejected.

**Option D (trait IN `turul-a2a-patterns`, no new crate):** ADR-021 §2.4 explicitly
rejects this: "different audience" and "orthogonal concern." An `LlmClient` trait
has non-A2A consumers (an LLM client is useful in non-A2A Rust programs); placing
it inside the A2A-specific patterns crate couples two orthogonal concerns and
expands the patterns crate's scope in a direction ADR-021 §2.4 explicitly bounded.
Rejected on those grounds; this ADR does not revisit the ADR-021 §2.4 judgement.

## 3. Non-Goals

- Hosting model-specific code in `turul-llm-client`. No `ollama-rs`, no
  `async-openai`, no Anthropic SDK in the framework crate. Provider adapters
  live in adopter code or named example crates.
- Competing with `ollama-rs`, `async-openai`, or equivalent community provider
  SDKs. `turul-llm-client` defines the interface; those crates implement the
  underlying transport.
- Implementing retry logic, token budgeting, observability, or cost attribution
  in the initial release. These may be added by amendment once the trait shape is
  stable.
- Streaming in the initial release. See §7.

## 4. Trait sketch

The following is the binding trait contract if this ADR is Accepted and the crate
is created. Deviations from these signatures require an ADR amendment.

```rust
// crates/turul-llm-client/src/lib.rs

use std::pin::Pin;
use std::future::Future;
use serde_json::Value;

/// Structured error returned by any `LlmClient` implementation.
#[non_exhaustive]
#[derive(Debug)]
pub enum LlmError {
    /// The model call timed out before returning a complete response.
    Timeout,
    /// The model returned a response but its content did not satisfy the
    /// caller's output schema. Carries the raw model output and a
    /// human-readable reason string.
    SchemaMismatch { raw: String, reason: String },
    /// A transport or protocol error communicating with the provider
    /// endpoint. Carries a human-readable description. The original
    /// provider error chain is available via `std::error::Error::source`.
    Transport(Box<dyn std::error::Error + Send + Sync>),
    /// The provider rejected the request due to rate limiting or quota.
    RateLimited,
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Timeout => write!(f, "LLM call timed out"),
            LlmError::SchemaMismatch { reason, .. } => {
                write!(f, "LLM response schema mismatch: {reason}")
            }
            LlmError::Transport(e) => write!(f, "LLM transport error: {e}"),
            LlmError::RateLimited => write!(f, "LLM provider rate limited"),
        }
    }
}

impl std::error::Error for LlmError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LlmError::Transport(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

/// Type alias for the boxed future returned by `LlmClient` methods.
/// Mirrors the `SinkFuture` alias pattern from ADR-021 §2.3 for the same
/// reason: AFIT does not yet let callers name the `Send` bound on returned
/// futures in a way that works with `tokio::spawn` over `&dyn LlmClient`.
pub type LlmFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A provider-neutral interface for a single synchronous LLM call.
///
/// The caller is responsible for:
/// - Rendering the prompt from a `SkillCard` manifest template (via
///   `turul-a2a-patterns`).
/// - Supplying the JSON Schema for the expected output. The provider
///   adapter MAY down-convert the schema to whatever subset its API
///   supports; it MUST NOT loosen what the caller's schema declares.
///
/// The caller's `provider_config` is the opaque `serde_json::Value`
/// from the `SkillCard` manifest's `provider_config:` frontmatter block
/// (ADR-021 §2.2 item 3). The adapter reads vendor-specific keys from
/// this value (model identifier, endpoint URL, format hints). Callers
/// that do not use `SkillCard` manifests may supply any `Value::Object`
/// or `Value::Null`.
///
/// # Object safety
///
/// This trait is object-safe. `SkillHandler` implementations that
/// depend on it accept `&dyn LlmClient` (matching the `&dyn
/// SkillProgressSink` pattern from ADR-021 §2.3).
pub trait LlmClient: Send + Sync {
    /// Send a rendered prompt and an expected-output JSON Schema to the
    /// provider; return the parsed JSON output on success.
    ///
    /// - `prompt`: fully-rendered prompt string (post template expansion).
    /// - `output_schema`: JSON Schema 2020-12 `Value` describing the
    ///   expected response shape. The adapter passes this to the provider's
    ///   structured-output API if the provider supports it.
    /// - `provider_config`: opaque per-provider configuration from the
    ///   manifest's `provider_config:` block, or `Value::Null` if absent.
    fn complete<'a>(
        &'a self,
        prompt: String,
        output_schema: Value,
        provider_config: Value,
    ) -> LlmFuture<'a, Result<Value, LlmError>>;
}

// Compile-time object-safety assertion.
const _: fn() = || {
    fn assert<T: ?Sized + LlmClient>() {}
    assert::<dyn LlmClient>();
};
```

### 4.1 Design notes

- **Three-argument signature.** `prompt` is the rendered string from
  `SkillCard::render_prompt`. `output_schema` is the manifest's `output_schema`
  field (a `serde_json::Value`). `provider_config` is the manifest's opaque
  `provider_config` block. Bundling all three keeps the interface self-contained
  for callers that hold a `SkillCard` and a `&dyn LlmClient`; no builder or
  config struct is needed for the common case.
- **No system-prompt argument in v1.** The prompt passed to `complete` is the
  fully-rendered manifest body. Provider adapters that distinguish user vs system
  prompt roles may split the rendered string at a marker or treat the full
  rendered prompt as the user turn. Formalising a system-prompt argument is
  deferred to §7.
- **Schema-mismatch error carries `raw`.** If the model returns content that does
  not satisfy `output_schema`, the caller can log the raw model output without
  coupling the error type to a specific schema validator.
- **`Send + Sync` on the trait.** Same rationale as `SkillProgressSink` in
  ADR-021 §2.3: handlers crossing `tokio::spawn` boundaries use `&dyn LlmClient`
  and need the bound.

## 5. Adoption path

`turul-llm-client` is created as a path-only `publish = false` workspace crate
when this ADR is Accepted. It follows the same pre-publish gate structure as
ADR-021 §4:

1. **Two non-toy adopters exercise `LlmClient`.** At minimum one must be a
   provider other than Ollama (demonstrating the trait generalises beyond the
   reference example).
2. **The `skill-manifest-ollama-agent` example is updated** to depend on
   `turul-llm-client` and implement `LlmClient` for its Ollama call path,
   replacing the direct `reqwest` dispatch in `run_live`. This serves as the
   canonical adopter migration reference.
3. **A second named example or community-contributed adapter** (e.g.
   `examples/openai-agent`) demonstrates a second provider implementation.

Until gates 1–3 clear, the crate is internal-workspace-only. After they clear,
publish in dependency order after `turul-a2a-patterns`.

## 6. Rejected alternatives

**Option A — no crate.** Leaves no stable anchor for error taxonomy or multi-
provider portability. Requires every adopter to independently solve LLM transport
error mapping. Adopted the Option C improvement over A to preserve the future
convergence path at low cost.

**Option B — trait + provider adapters in the framework crate.** Couples release
cadence to provider-API churn. Adds transitive deps for adopters who do not call
LLMs. Framework team cannot responsibly maintain adapters for providers they do
not test against. Rejected.

**Option D — trait in `turul-a2a-patterns`.** ADR-021 §2.4 explicitly forbids this:
LlmClient is an orthogonal concern with a different audience and different
dependency surface. Adding it to the patterns crate would expand patterns scope
beyond A2A skill-authoring abstractions, creating a scope-creep vector that
ADR-021 §2.4 was written to prevent. Rejected without reopening ADR-021.

## 7. Open questions

1. **Streaming responses.** The `complete` method returns `Value` (fully
   materialised). Streaming token output is useful for long-form generation
   and for feeding progress updates to `SkillProgressSink`. A streaming variant
   (`LlmFuture<'a, impl Stream<Item = Result<Value, LlmError>>>` or a separate
   `stream_complete` method) is not included in v1 to keep the trait object-safe
   and avoid a `futures`/`tokio-stream` dependency in the crate. Revisit when a
   concrete adopter streaming need is documented.

2. **Retry policy.** Exponential backoff on `LlmError::RateLimited` or transient
   `Transport` errors is a cross-cutting concern. Should it live in a wrapper
   struct in `turul-llm-client` (e.g. `RetryingLlmClient<C: LlmClient>`), or is
   it always the adopter's responsibility? Defer until a second adopter reports
   the need.

3. **Token budgeting.** The manifest's `executionHints.maxTokens` field is
   currently provider-config-only (read by the adapter from `provider_config`).
   Should `complete` accept an explicit `max_tokens: Option<u32>` argument so
   callers can enforce a framework-level ceiling regardless of the adapter's
   interpretation? Risks binding the trait to a tokenizer-specific concept;
   defer.

4. **Observability hooks.** Prompt length, token counts, latency, and cost are
   useful metrics but require provider-specific APIs to extract. A structured
   `LlmUsage` return alongside `Value` (analogous to OpenAI's `usage` field)
   could be added as an optional extension. The `#[non_exhaustive]` on `LlmError`
   and the boxed future return type allow this to be added without breaking the
   trait. Defer pending adopter demand.

5. **System prompt vs user prompt distinction.** Some providers (OpenAI, Anthropic)
   distinguish system-prompt from user-turn; Ollama's `/api/chat` accepts a
   `messages` array with `role` fields. The current trait treats the rendered
   manifest body as a single prompt string and leaves role assignment to the
   adapter. A more structured `LlmRequest { system: Option<String>, user: String
   }` input type would let adopters express role boundaries without embedding
   role markers in the prompt template. Defer to a trait amendment once a second
   provider adapter makes the generalisation apparent.
