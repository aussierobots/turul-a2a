# ADR-030: Skill Pre-Terminal Gates and Progress-Sink Heartbeat

- **Status:** Accepted
- **Date:** 2026-11-07
- **Related:** ADR-021 (patterns crate: `SkillHandler`, `SkillProgressSink`, `TerminalHook`), ADR-022 (skill-invocation dispatch profile), ADR-029 (stall detection)

## Context

`turul-a2a-patterns` gives skill authors a handler trait, a progress sink and a post-terminal observer. Two needs of long-running skills are not covered.

1. **Liveness.** A server with `executor_stall_timeout` fails an executor that records no progress for the timeout. A skill doing long silent work (an upstream call, a large computation) records progress only by emitting a status or artifact event. `EventSink::heartbeat()` records liveness without an event, but a handler written against `&dyn SkillProgressSink` cannot reach it.
2. **Veto before completion.** Some checks must run after a handler produces its output but before the task becomes terminal, and must be able to turn a successful output into a failure: an output-contract check, an audit precondition. `TerminalHook` cannot do this; it observes the outcome after the fact and its failures must not abort execution. Without a shared shape, each skill embeds such checks in its own body, and checks shared across skills are copied into each one.

The A2A spec already gives a skill everything else it needs mid-task, and `SkillProgressSink` already exposes it: a `TaskStatusUpdateEvent` with a message in any non-terminal state, including an intermediate `TASK_STATE_INPUT_REQUIRED` (`set_status`), and a `TaskArtifactUpdateEvent` with `append` / `lastChunk` (`emit_artifact`). No new event kind is introduced.

## Decision

### 1. `SkillProgressSink::heartbeat`

```rust
fn heartbeat(&self) {}
```

A provided, synchronous method with a no-op default. It records that the skill is alive and emits no event. A sink backed by the server's `EventSink` delegates to `EventSink::heartbeat()`. The default keeps every existing sink compiling; a sink with no liveness tracking needs no change.

### 2. `PreTerminalGate` and `GatedHandler`

```rust
#[non_exhaustive]
pub struct GateContext<'a> {
    pub skill_id: &'a str,
    pub params: &'a Value,
    pub output: &'a Value,
}

#[async_trait]
pub trait PreTerminalGate: Send + Sync {
    async fn check(&self, ctx: GateContext<'_>) -> Result<(), SkillError>;
}

pub struct GatedHandler { /* skill id, inner handler, gates */ }

impl GatedHandler {
    pub fn new(skill_id: impl Into<String>, inner: Arc<dyn SkillHandler>) -> Self;
    pub fn gate(self, gate: Arc<dyn PreTerminalGate>) -> Self;
}

impl SkillHandler for GatedHandler { /* ... */ }
```

Semantics, owned by `GatedHandler::run`:

- The inner handler runs first. If it returns `Err`, no gate runs and the error is returned unchanged.
- On `Ok(output)`, gates run in the order they were added, each with the skill id, the invocation params and the output.
- The first gate returning `Err` stops the rest; that error is the result. Otherwise the inner output is returned unchanged.
- A veto is `SkillError::Rejected { gate, reason }`, a new third variant whose `Display` is `rejected by <gate>: <reason>`. It maps to A2A `Internal` like `Internal`, so the executor records it as the task's `FAILED` status message, and a `TerminalHook` sees `SkillOutcome::Failure` with it. A distinct variant lets an executor, monitor or audit tell a refused output from a handler fault without parsing text. A gate that cannot decide (its own dependency failed) returns `Internal`.
- A gate sees only the output the handler returns, not artifacts the handler streamed through its sink. A handler whose result must pass a gate returns what the gate checks.
- Gates receive no sink. They decide; they do not emit events, and a terminal state stays out of the patterns crate.
- `GatedHandler` is a `SkillHandler`, so it is registered through the unchanged `SkillRegistry` methods.

The gate runs inside the handler's result rather than in the registry or the server because the patterns crate does not commit terminal states and owns no dispatcher: the executor calling the handler decides the terminal. Placing the veto in the handler's return value makes every executor honour it without new wiring.

## Alternatives considered

- **Gates on `SkillRegistry`** (`register_*` taking gates). Every registry implementation would have to run them, and `handler(id)` would return something other than the registered handler. Rejected: wrapping keeps the registry contract unchanged.
- **Reusing `SkillError::Internal` for a veto.** Keeps the variant set unchanged, but a veto then reads the same as a crash or bug to every consumer. Rejected in favour of `Rejected`.
- **Giving `GateContext` the artifacts the run emitted.** Requires the sink to record every artifact for the duration of the run, and a gate on a streamed artifact would run only after the client already received it. Rejected: gates check the returned output.
- **A server-side pre-terminal hook in `turul-a2a`.** The server does not know the skill output shape, and `turul-a2a` does not depend on the patterns crate. Rejected.
- **A direct `impl SkillProgressSink for EventSink`.** Would make `turul-a2a` depend on `turul-a2a-patterns`, which no other publish crate may. Adopters keep a named-field bridge (`struct S { event_sink: EventSink }`) whose `heartbeat` delegates.

## Consequences

- New public items in `turul-a2a-patterns`: `PreTerminalGate`, `GateContext`, `GatedHandler`, `SkillError::Rejected` and the provided `SkillProgressSink::heartbeat`. Additive: existing sinks and handlers compile unchanged, and `SkillError` is `#[non_exhaustive]`, so consumer matches already carry a wildcard arm. ADR-021's fixed `SkillError` variant set is amended to three.
- `GatedHandler` clones the invocation params once per call so gates can see them after the inner handler consumes its copy.

## Tests

`crates/turul-a2a-patterns/tests/pre_terminal_gate.rs`: gates run in order and pass the output through; the first veto (`SkillError::Rejected` naming the gate) is returned and later gates do not run; an inner error skips every gate; the context carries the skill id, params and output; the trait is object-safe. `crates/turul-a2a-patterns/tests/skill_progress_sink.rs`: the default heartbeat is callable through `dyn SkillProgressSink`, and an override is reached through it.

## Revision log

- 2026-11-07 — Accepted.
