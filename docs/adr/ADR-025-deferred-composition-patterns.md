# ADR-025: Deferred composition patterns — escalation triggers

- **Status:** Proposed (informational; no behavior commitment)
- **Date:** 2026-05-23
- **Depends on:** ADR-021 (patterns crate scope), ADR-022
  (skill-invocation dispatcher profile), ADR-024 (typed-handler
  sketch).
- **Implements:** nothing — this ADR is a parking record so deferred
  composition patterns don't decay into an unbounded "we should
  maybe do this" backlog.

## 1. Context

The composition-patterns slice landed five existing showcase agents
+ one new (`remote-delegate-agent`). Each agent demonstrates exactly
one composition idiom (manifest skill, planner/router, critic,
post-task hook, dispatcher profile, server-side delegation). Each
has Python/Go/Rust interop clients.

During the review pass that produced this slice (codex + Claude
back-and-forth), several adjacent composition patterns were
considered and deliberately not built. Some are well-known from
other agent frameworks; others surface naturally from the patterns
already shipped. Without a concrete escalation trigger, any of them
could leak into "maybe next sprint" forever.

This ADR records each deferred pattern with a **falsifiable trigger**
— a condition under which the pattern would graduate from deferred
to actually-on-the-roadmap. If the trigger never fires, the pattern
stays deferred. If it fires, the pattern gets its own ADR (no code
without that).

## 2. Deferred patterns

### 2.1 Agent graph engine

**Pattern.** Compose multiple agents as nodes in an explicit
graph: directed edges, conditional branches, optional shared state.
Other frameworks (LangGraph, agno, CrewAI's "flows") ship this as
a first-class primitive.

**Why not now.** The existing `agent-role-planner-router-agent`
example IS the minimal useful graph: one source, N sinks, no
edges. For every demo we have today, that shape is sufficient. A
generic graph engine is a non-trivial commitment (DAG runtime,
state passing, cancellation propagation, error handling across
nodes) without any current adopter case that planner/router
cannot express.

**Escalation trigger.** Revisit when a concrete adopter case
requires **either**:
- conditional edges based on an inter-node value (e.g. "if
  classifier says X then go to step B else step C"), **or**
- stateful handoffs where node N+1 needs structured access to
  node N's intermediate state beyond what's expressible as the
  output artifact.

Until both shapes are demanded by a real consumer (internal or
external), planner/router stays sufficient and the graph engine
stays deferred.

### 2.2 Agent swarm / peer-collaboration

**Pattern.** Multiple peer agents communicate without fixed
routing — emergent task decomposition, voting, leader election.
Used by some research frameworks and a small number of production
systems.

**Why not now.** Strands itself explicitly does not support
A2AAgent inside its swarm primitive. The interaction model is
ill-defined for proxy-shaped agents (which is what A2A's wire
identity really is). The state space of "what does it mean to
swarm over A2A endpoints" is open.

**Escalation trigger.** Revisit only when **both**:
- at least one upstream framework ships supported A2A-in-swarm
  behavior (i.e. the design problem is solved elsewhere), **and**
- an adopter signals demand for it inside this workspace.

Neither holds today. Likely deferred indefinitely.

### 2.3 Agent-as-LLM-tool adapter

**Pattern.** Expose an A2A `AgentSkill` as an LLM "tool" definition
(name + JSON Schema for inputs), so an LLM-driven orchestrator can
call A2A agents the same way it calls MCP tools or native
function-call APIs.

**Why not now.** "Tool" is MCP / LLM-function-calling vocabulary,
not native A2A. A2A v1.0 has `AgentSkill`. Mapping
`AgentSkill → Tool` is a legitimate adapter, but it implicitly
bridges A2A into the LLM-tool worldview. The naming, ownership,
and the question "does this adapter live in `turul-a2a-patterns`,
in a new `turul-a2a-llm-tool-adapter` crate, or in
`turul-mcp-framework`?" are all unresolved.

**Escalation trigger.** Revisit when an adopter is building either:
- an MCP server that needs to expose A2A skills as MCP tools, **or**
- an LLM orchestration layer (function-calling client) that needs
  A2A-served skills mixed into its tool registry.

The right home for this adapter is most likely
[`turul-mcp-framework`](https://github.com/aussierobots/turul-mcp-framework)
(the sibling MCP repo), not this one. A joint ADR across both
workspaces would land first.

### 2.4 A2A ↔ MCP-tool deep bridge

**Pattern.** A full bidirectional bridge: any A2A agent surfaces
its skills as MCP tools AND any MCP server surfaces its tools as
an A2A agent. Both vocabularies preserved without lossy
translation.

**Why not now.** Same vocabulary-collision problem as §2.3, scaled
up. Adds a second axis: not just "AgentSkill → Tool" but
"AgentSkill ↔ Tool" plus the protocol shape differences
(streaming, push notifications, agent cards vs. tool definitions).

**Escalation trigger.** A joint design ADR proposed in **both**
`turul-a2a` and `turul-mcp-framework`'s `docs/adr/` directories,
resolving vocabulary, ownership, and the surface area that each
workspace owns. Until then, neither workspace adds bridge code.

### 2.5 `A2aClient`-based `AgentExecutor` helper

**Pattern.** A library-side helper that wraps the
`remote-delegate-agent` boilerplate into a reusable
`A2aClientExecutor` (or similar), so adopters don't copy the
~300 LOC of forwarding + error mapping into every gateway agent.

**Why not now.** `remote-delegate-agent` is the *first* example of
the delegation pattern. The error-mapping table, the
auth-forwarding policy, the streaming defer, and the discovery
caching strategy are all decisions made for one demo. A second
example would surface which of those decisions are
adopter-overridable and which are framework defaults — exactly the
information needed to design the helper trait.

**Escalation trigger.** At least one of:
- a second delegating example in this workspace (e.g. an
  auth-gating proxy, a fan-out aggregator) carrying enough of the
  same boilerplate that the duplication is obvious, **or**
- an adopter (internal or external) asking for the helper to land
  in `turul-a2a-client` with documented configuration knobs.

When the trigger fires, the helper likely lands in
`turul-a2a-client` (it's an executor adapter over the client) rather
than `turul-a2a-patterns` (which is skill-authoring, not transport
composition).

### 2.6 Streaming passthrough in `remote-delegate-agent`

**Pattern.** End-to-end SSE bridge: caller streams to the delegate,
the delegate streams from the upstream, intermediate `Working`
status events propagate, cancel propagates both directions.

**Why not now.** v1 of `remote-delegate-agent` advertises
`streaming: false` and buffers the upstream response. Adding true
SSE passthrough requires: subscribing to upstream SSE, fanning out
to local subscribers, propagating cancel (with the right timing
semantics so an orphaned upstream task doesn't burn resources), and
handling reconnection. Each is solvable; together they're a slice
of their own.

**Escalation trigger.** An adopter case where the upstream emits
**≥3 intermediate `Working` events** that callers need to observe
in real time. A single intermediate status (or none, as today) does
not warrant the complexity.

## 3. Not on this list (intentionally)

The following are NOT deferred — they're *out of scope* and don't
need triggers:

- **A2A-over-non-HTTP transports** (WebSockets, raw TCP). A2A v1.0
  specifies HTTP/JSON-RPC and HTTP/REST + gRPC. Anything else is
  upstream protocol design, not framework work.
- **Distributed task coordinator across heterogeneous agents.**
  That's a workflow engine wearing an A2A skin. ADR-018 (durable
  Lambda) covers the slice of this that the framework owns
  (per-agent durability); cross-agent orchestration is the
  adopter's choice of tool (Temporal, Step Functions, Airflow).
- **Visual builder / agent IDE.** Not in scope; downstream tooling.

## 4. How to use this ADR

When a future maintainer (or reviewer) catches themselves saying "we
should add X" about one of these patterns, the answer is: **check
if the trigger has fired.**

- If yes, open a successor ADR and the work is on the table.
- If no, the answer is "deferred per ADR-025 §<N>", with the
  trigger condition as the explanation.

This is the single forcing function that keeps the deferred list
honest.

## 5. Decision

**Proposed** (informational). This ADR records existing decisions
rather than committing the workspace to any new behavior. It moves
to Accepted as part of the composition-patterns slice close-out
(Task #46) once the rest of the slice lands.

## Cross-references

- ADR-021 (patterns crate scope) — what may/may not live in
  `turul-a2a-patterns`.
- ADR-022 (skill-invocation dispatcher profile) — the wire side
  of "which skill should run" for a multi-skill agent (related to
  §2.3, §2.4).
- ADR-023 (LLM client abstraction) — why provider concerns live
  outside this workspace (related to §2.3).
- ADR-024 (typed-handler sketch) — the typed-Input pattern that
  would naturally compose with `Agent-as-LLM-tool` (§2.3).
- `examples/remote-delegate-agent/README.md` — the v1 delegation
  contract whose streaming defer is captured in §2.6 here.
