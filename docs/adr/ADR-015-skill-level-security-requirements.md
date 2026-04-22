# ADR-015: Skill-Level `security_requirements` — Advertisement vs. Enforcement

- **Status:** Accepted
- **Date:** 2026-04-22
- **Depends on:** ADR-001 (proto-first architecture), ADR-002 (wrapper
  boundary), ADR-005 (dual transport), ADR-007 (auth middleware),
  ADR-014 (gRPC transport)
- **Spec reference:** A2A v1.0 §1.4 ("the proto is the single authoritative
  normative definition"); `proto/a2a.proto` L260-277 (`Message`), L356-393
  (`AgentCard`), L429-447 (`AgentSkill`), L488-510 (`SecurityRequirement`,
  `SecurityScheme`)
- **Supersedes/relates:** `CLAUDE.md` §"Deferred (ordered by priority)"
  item 1 ("Skill-level `security_requirements` — agent-level only for now")

## 1. Context

The A2A proto defines three distinct security surfaces on an `AgentCard`:

| Proto field | Line | Semantics |
|---|---|---|
| `AgentCard.security_schemes : map<string, SecurityScheme>` | 376 | Scheme catalogue — names the mechanisms (`apiKey`, `bearer`, `oauth2`, `openIdConnect`, `mtls`) |
| `AgentCard.security_requirements : repeated SecurityRequirement` | 378 | Agent-level OR-of-ANDs — what a client MUST present to *contact* the agent |
| `AgentSkill.security_requirements : repeated SecurityRequirement` | 446 | Per-skill OR-of-ANDs — proto comment says "Security schemes necessary for this skill" |

Only the first two are honoured by turul-a2a today.

### 1.1 Current state in-tree

- **Middleware**: `A2aMiddleware::before_request()` runs at the Tower
  layer before any routing or handler dispatch
  (`crates/turul-a2a/src/middleware/traits.rs:11-20`). It has no visibility
  into which skill (if any) a given `Message` targets.
- **AgentCard augmentation**: `SecurityAugmentedExecutor::agent_card()`
  merges `SecurityContribution`s from the middleware stack into
  `card.security_schemes` and `card.security_requirements`
  (`crates/turul-a2a/src/server/mod.rs:884-896`). It does **not** touch
  `card.skills[].security_requirements`.
- **Builder**: `AgentSkillBuilder::build()` hard-codes
  `security_requirements: vec![]` (`crates/turul-a2a/src/card_builder.rs:65`).
  There is no setter. The only way for an adopter to publish skill-level
  requirements today is raw proto mutation on the returned `AgentSkill` —
  which the project's own policy flags as a design smell (CLAUDE.md §"Example
  and API Surface Policy").
- **Tests**: No test asserts anything about
  `card.skills[].security_requirements`. A card that advertises a scheme
  only at the skill level, without adding the scheme name to
  `card.security_schemes`, would serialize cleanly and pass the full suite.

### 1.2 The normative gap — no `skill_id` on the wire

Skill-level enforcement requires the server to know, at request time,
*which skill* a given message targets. The proto does not provide that
binding:

- `Message` fields (L260-277): `message_id`, `context_id`, `task_id`,
  `role`, `parts`, `metadata`, `extensions`, `reference_task_ids`. No
  `skill_id`.
- `SendMessageRequest` fields (L642-651): `tenant`, `message`,
  `configuration`, `metadata`. No `skill_id`.
- `SendMessageConfiguration` (L143-161): accepted output modes, push
  config, history length, return-immediately flag. No `skill_id`.

The only carriers available are `Message.metadata`
(`google.protobuf.Struct`) or `Message.extensions` (`repeated string`) —
both free-form and not normatively reserved for skill routing.
`AgentSkill.id` is a `REQUIRED` identifier (proto L432) but the spec does
not define how a client signals intent to invoke a specific skill.

This is the load-bearing constraint for this ADR. Any enforcement path
has to either:

1. define a turul-a2a-specific convention over `Message.metadata` (e.g.,
   `metadata["a2a.skillId"] = "<id>"`) — a local extension that will not
   round-trip to other A2A implementations,
2. rely on the executor to classify the message internally and surface the
   resolved skill back to the auth layer (a routing-first design — adds a
   full trait surface and re-orders the request pipeline), or
3. defer enforcement entirely until the upstream spec adds a normative
   skill binding.

### 1.3 What the spec actually says

The proto comment at L445-446 is the whole normative statement:

```proto
  // Security schemes necessary for this skill.
  repeated SecurityRequirement security_requirements = 8;
```

The upstream A2A v1.0 specification does not define:

- whether skill-level requirements **override** or **AND with**
  agent-level requirements,
- whether an absent skill-level list means "same as agent" or "no
  requirement",
- how a client signals which skill a request targets.

**Consequence:** any enforcement implementation is a local interpretation.
Advertising skill-level requirements that the framework then quietly
ignores is a conformance *claim* that doesn't match behaviour — the
AgentCard lies to the client about what gatekeeping is in force.

## 2. Decision

### 2.1 Choose **Option 2: declaration-only with truthfulness invariants**

Of the three options in the scope:

1. ❌ **Reject/suppress skill-level requirements** until enforced.
   Rejected: the proto field is normative and legitimate out-of-band
   consumers (discovery registries, UIs, cross-ecosystem auditing tools)
   read it. Silently dropping it on serialization would make turul-a2a's
   AgentCard non-isomorphic to the proto it claims to implement.

2. ✅ **Declaration-only with clear docs and truthfulness invariants.**
   Chosen. Adopters get a first-class `AgentSkillBuilder` API to publish
   skill-level requirements, the framework checks that every scheme name
   referenced by a requirement in the final advertised card is also
   declared in that same card's `security_schemes`, and the docs state
   bluntly that authorization happens at the agent level only. The
   framework makes no claim that advertised skill-level requirements
   will be enforced at request time — consistency here is a serialization
   invariant, not a gatekeeping one.

3. ❌ **Enforce skill-level requirements at request routing.**
   Rejected *for 0.1.x*. Blocked by §1.2. Reopening this ADR (or a
   successor) is the right path once the upstream spec defines a skill
   binding on `Message`, or once there is sufficient adopter demand to
   justify a turul-a2a-local `metadata["a2a.skillId"]` convention.

### 2.2 Public API shape (declaration-only)

Add two symmetric surfaces to `crates/turul-a2a/src/card_builder.rs`:

- `AgentSkillBuilder::security_requirements(Vec<SecurityRequirement>) -> Self`
- `AgentCardBuilder::security_schemes(...)` / `::security_requirements(...)`
  for agent-level **adopter-supplied** metadata that is *not* derived from
  middleware. (Today this is implicit — only middleware contributes. An
  adopter who wants to publish an mTLS scheme at the agent level without
  wiring the equivalent middleware has no path.)

These accept raw `turul_a2a_proto::SecurityRequirement` to avoid
duplicating a wrapper type for a thin proto message — consistent with
ADR-002 and the existing `AgentCardBuilder::skill()` contract that takes
raw `AgentSkill`.

### 2.3 Truthfulness invariants (enforced post-merge at server build)

`AgentCardBuilder::build()` deliberately does **NOT** validate that
requirements reference declared schemes. Rationale: middleware
contributions are layered in later by `SecurityAugmentedExecutor` at card
serve / server-build time, so a skill requirement that names `"bearer"`
may be legitimate even when the adopter-supplied
`card.security_schemes` map is empty — the bearer scheme arrives from
middleware. Validating at builder time would false-reject such cards.

Instead, validation runs **post-merge** inside the server build
pipeline, against every card surface that clients will actually
receive. Concrete seam: `A2aServerBuilder::build()` (after
`merged_security` is computed at `server/mod.rs:511-514`, and before
returning the `A2aServer` handle) materializes **two** augmented cards
and validates each:

- The public card produced by `SecurityAugmentedExecutor::agent_card()`
  (served over HTTP `.well-known/agent-card.json`).
- The extended card produced by
  `SecurityAugmentedExecutor::extended_agent_card(None)`, when the
  executor's `extended_agent_card(None)` returns `Some(_)` (served over
  HTTP `/extendedAgentCard`, JSON-RPC `GetExtendedAgentCard`, and gRPC
  `A2AService.GetExtendedAgentCard`).

Both cards receive the same merge pass (middleware schemes +
requirements appended to whatever the executor returned), and both
cards are independently validated:

1. For every `SecurityRequirement` in the merged
   `card.security_requirements`, every key in its `schemes` map MUST
   appear in the merged `card.security_schemes`.
2. For every skill in `card.skills`, for every `SecurityRequirement` in
   `skill.security_requirements`, every key in its `schemes` map MUST
   appear in the merged `card.security_schemes`.
3. `A2aServerBuilder::build()` returns `A2aError::InvalidRequest` with a
   message that names the offending scheme, the skill id (or
   "agent-level"), and which card surface failed (`agent_card` vs.
   `extended_agent_card`). The server fails fast at startup.

Extended-card validation is conditional: if
`executor.extended_agent_card(None)` returns `None`, there is nothing
served at `/extendedAgentCard` and nothing to check. If the executor
returns a claims-sensitive extended card (takes `claims` into account),
only the no-claims variant is validated at build — per-claim variants
are the adopter's responsibility and cannot be materialized at build
time. Documenting this caveat is part of §2.5.

This placement guarantees that every **build-materializable**
advertised card surface — the public card over HTTP, and the no-claims
extended card over HTTP / JSON-RPC / gRPC — cannot reference an
undeclared scheme at server boot. Claims-sensitive extended-card
variants (where `extended_agent_card(Some(claims))` returns a card
shaped by the caller's identity) are out of scope for build-time
validation and remain adopter responsibility; the caveat from §2.3
applies. The per-request
`SecurityAugmentedExecutor::agent_card()` and `extended_agent_card()`
calls do not re-validate; the invariant is checked once at server
build for every surface that can be materialized there.

These are card-level invariants, not runtime gatekeeping. The framework
does **not** read `skill.security_requirements` during
`A2aMiddleware::before_request()`. Adopters who need true skill-level
enforcement in 0.1.x must implement it inside their `AgentExecutor` using
whatever skill-classification scheme they control — documented as an
out-of-framework extension.

### 2.4 `SecurityAugmentedExecutor` does NOT push middleware contributions into skills

The middleware-derived `SecurityContribution` is by construction an
*agent-level* statement ("all traffic through this Tower layer will be
validated against X"). Auto-pushing it into every skill's
`security_requirements` would be **more** misleading — it would imply
that these requirements are per-skill gatekept, when in reality the same
check applies uniformly to every request.

Decision: skill-level `security_requirements` content is 100%
adopter-supplied via `AgentSkillBuilder`. Middleware contributes
agent-level only, unchanged from today. `SecurityAugmentedExecutor`
gains no new responsibility for skill-level *content*; validation (§2.3)
runs at server build, not inside the per-request augmentation path.

### 2.5 Docs contract

The following must be added (and are the sole documentation changes
required by this ADR):

- **`crates/turul-a2a/src/card_builder.rs`** rustdoc on
  `AgentSkillBuilder::security_requirements`: state explicitly that
  turul-a2a does not enforce these at request time in 0.1.x; they are
  published as-is for discovery and out-of-band tooling; any runtime gate
  is the adopter's responsibility inside `AgentExecutor`.
- **`crates/turul-a2a-auth/README.md`**: add a "Skill-level requirements"
  section pointing to ADR-015 with the same caveat.
- **`CLAUDE.md`** §"Deferred": replace the existing item 1 with a pointer
  to ADR-015, and add a new deferred item for "Skill-level enforcement
  (blocked on upstream proto skill_id binding)".

## 3. Non-Goals

- Any change to `A2aMiddleware` or the request pipeline. Authorization
  remains agent-level.
- Any turul-a2a-local convention for `Message.metadata["a2a.skillId"]` or
  similar. Deferred — if this becomes necessary it gets its own ADR with
  interop analysis.
- Any wrapper around `turul_a2a_proto::SecurityRequirement` or
  `SecurityScheme`. ADR-002 explicitly permits raw proto at the builder
  boundary for stateless record types.
- Any change to `turul-a2a-types` or `turul-a2a-proto`. Auth remains
  optional and out of the protocol/types crates (per scope requirement).

## 4. Test Obligations

All new tests live in `crates/turul-a2a/tests/` or in
`card_builder.rs::tests` — no new test crate. The existing framework gate
(`cargo test -p turul-a2a`) covers them.

### 4.1 Truthful advertisement — two layers

Validation runs at server build (§2.3). Tests split into two groups:

**Builder-level unit tests** (`card_builder.rs::tests`) — verify the
builder does *not* reject requirements whose schemes are absent from the
adopter-supplied `security_schemes` map, since middleware may contribute
them later. These are negative tests for over-validation:

1. **`builder_permits_requirement_with_scheme_to_be_merged_later`** —
   build a card with a skill requirement naming `"bearer"`, no
   adopter-declared `"bearer"` scheme. `build()` succeeds. No
   cross-field validation runs at this layer.

**Post-merge integration tests** (`crates/turul-a2a/tests/` — new file
or extension of an existing auth/card test) — verify the final
advertised card is self-consistent:

2. **`server_build_rejects_skill_requirement_with_undeclared_scheme`** —
   with no middleware installed and a skill advertising
   `SecurityRequirement { schemes: { "bearer": [] } }` but no
   `security_schemes["bearer"]` anywhere in the merged card,
   `A2aServerBuilder::build()` returns `InvalidRequest` citing `bearer`
   and the skill id. Server does not start.
3. **`server_build_rejects_agent_requirement_with_undeclared_scheme`** —
   same, at the agent level.
3a. **`server_build_rejects_extended_card_skill_requirement_with_undeclared_scheme`**
   — executor's `agent_card()` is clean but its
   `extended_agent_card(None)` returns `Some(card)` where a skill
   advertises `SecurityRequirement { schemes: { "bearer": [] } }` and
   no middleware contributes `"bearer"`. `A2aServerBuilder::build()`
   returns `InvalidRequest` naming `bearer`, the skill id, and
   `extended_agent_card` as the failing surface. This is the
   load-bearing test for §2.3's extended-card coverage — without it,
   the extended card can ship an inconsistent scheme reference that
   the public card would have caught.
4. **`server_build_accepts_skill_requirement_satisfied_by_middleware`** —
   skill advertises a requirement naming `"bearer"`; adopter declares no
   `security_schemes` map; a bearer middleware is installed and
   contributes `"bearer"` via `SecurityContribution`. Merged card has
   `securitySchemes.bearer` once. Server starts. This is the case the
   builder-layer validation would incorrectly reject.
5. **`server_build_accepts_skill_requirement_declared_by_adopter`** —
   adopter declares `security_schemes["bearer"]` AND references it from
   one skill's `security_requirements`. No middleware. Server starts.
   The served `.well-known/agent-card.json` round-trips with
   `skills[0].securityRequirements[0].schemes.bearer` preserved
   (camelCase per proto JSON mapping).
6. **`middleware_contributions_and_skill_requirements_both_survive`** —
   bearer middleware + adopter-supplied skill requirement naming the
   same scheme. Final served card shows `securitySchemes.bearer` once
   (no duplicate), `card.security_requirements` populated by middleware,
   and the skill's requirement preserved verbatim.

### 4.2 Declaration-only invariant (no-skill-targeting runtime test)

7. **`advertised_skill_requirements_do_not_install_middleware`** —
   construct an agent with:
   - No middleware in the stack.
   - One skill that advertises a `SecurityRequirement` naming `"bearer"`.
   - `security_schemes["bearer"]` declared on the card.
   POST `/message:send` with NO `Authorization` header and a message
   with no skill-targeting metadata. The request succeeds with HTTP 200.

   The invariant under test is explicit and does not rely on a
   skill-targeting convention: **advertising skill-level
   `security_requirements` does not install a middleware and does not
   change request authorization.** Authorization is governed solely by
   the middleware stack. This test would only flip to a reject-path
   response if a successor ADR both (a) introduces a normative way for a
   message to target a skill and (b) enables runtime enforcement — at
   which point the successor ADR supersedes this record.

### 4.3 No spec-compliance regression

8. **`agent_card_with_skill_level_security_roundtrips_via_all_card_paths`**
   — add one case to `crates/turul-a2a/tests/spec_compliance.rs`. An
   executor whose `agent_card()` AND `extended_agent_card()` both return
   a card containing a skill-level `SecurityRequirement`. Verify the
   skill-level `securityRequirements` are byte-identical
   (post-canonicalization) across:
   - HTTP `GET /.well-known/agent-card.json` (the only normative path
     for the public card);
   - HTTP `GET /extendedAgentCard` (proto route);
   - JSON-RPC `GetExtendedAgentCard`;
   - gRPC `A2AService.GetExtendedAgentCard`.

   `GetAgentCard` / `A2AService.GetAgentCard` are NOT present in the
   proto service or JSON-RPC dispatch (`proto/a2a.proto:122` is the only
   card RPC — `GetExtendedAgentCard`). Do not invent RPC names. The
   public card has exactly one transport — HTTP discovery.

   Skip the `GetExtendedAgentCard` arm of this test if the executor's
   `extended_agent_card()` returns `None` (i.e., the extended card is
   not enabled for this test setup). Only the HTTP discovery arm is
   universally applicable.

## 5. Implementation Workflow (agent team — required pre-implementation gate)

This ADR does not flip from **Proposed** to **Accepted** until the A2A
1.0 Spec Compliance Agent and the Test Writer Agent have both signed
off (see the two sign-off gates below). No implementation commit may
begin before that sign-off.

Four named workstream owners, kept as separate review gates. The
workflow lives in this ADR and in the subsequent implementation plan /
task handoff — it **MUST NOT** appear in rustdoc, source comments, or
any committed artifact that isn't a planning document.

### 5.1 A2A 1.0 Spec Compliance Agent

- **Owns:** proto/spec truthfulness. Single source of truth for "what
  the wire looks like."
- **Verifies before sign-off:**
  - The vendored `proto/a2a.proto` still matches upstream
    `a2aproject/A2A:main/specification/a2a.proto` (CLAUDE.md §"Proto and
    Spec Compliance" gate — SHA256 check).
  - No plan / ADR / test language introduces a *positive* requirement
    to call a non-existent RPC. In particular, this ADR contains no
    positive requirement to invoke `GetAgentCard` or
    `A2AService.GetAgentCard`; any mention of those names in this ADR
    appears only as an explicit anti-claim — a statement that the RPC
    does not exist in the proto service (e.g., §4.3 guarding the
    spec-compliance test against inventing them). The only card RPC is
    `GetExtendedAgentCard` (`proto/a2a.proto:122`). Sign-off check:
    every occurrence of `GetAgentCard` in this ADR must be inside a
    negative clause ("NOT present", "does not exist", "do not invent")
    — grep confirms.
  - The semantics of `AgentCard.security_schemes` (L376),
    `AgentCard.security_requirements` (L378), and
    `AgentSkill.security_requirements` (L446) as cited match the
    current proto on disk.
  - No local-only wire convention (e.g., `metadata["a2a.skillId"]`) is
    introduced by this ADR or its tests.
- **Sign-off gate (§5):** must produce a written statement "ADR-015 is
  spec-truthful as of proto SHA256 <hash>" before acceptance.

### 5.2 Test Writer Agent

- **Owns:** writing the tests from §4 *before* any implementation
  code, per CLAUDE.md §"TDD Discipline".
- **Covers:**
  - Builder-level permissiveness (§4.1, test 1).
  - Post-merge validation at `A2aServerBuilder::build()` (§4.1, tests
    2–6).
  - Declaration-only runtime invariant (§4.2, test 7) — worded so the
    test body does NOT pretend a message targets a skill.
  - Spec-compliance round-trip across the real card transports (§4.3,
    test 8) — HTTP discovery + `GetExtendedAgentCard` only, skipping
    arms that do not apply.
- **Constraint:** MUST NOT invent a skill-targeting convention. If a
  test appears to need one, return to the ADR for an explicit
  decision before proceeding.
- **Sign-off gate (§5):** produces a compiling test suite on a
  branch that fails against current `main` (red phase); the tests must
  be reviewable and runnable before implementation begins.

### 5.3 Documentation Writer Agent

- **Owns:** ADR-015 content itself (this file), `CLAUDE.md`
  updates to the §"Deferred" list, and rustdoc on the new
  `AgentSkillBuilder::security_requirements` / `AgentCardBuilder::
  security_schemes` / `AgentCardBuilder::security_requirements` setters.
- **Ensures:** docs state *declaration-only* clearly; rustdoc on each
  new setter explicitly says "turul-a2a does not enforce these at
  request time; any skill-level gatekeeping is the adopter's
  responsibility inside `AgentExecutor`."
- **Constraint:** MUST NOT include workflow / phase / agent-role
  references in rustdoc or source comments. Those belong in this ADR
  and the task handoff only — per CLAUDE.md §"Comment and Docstring
  Style", internal planning labels must not leak into committed code.

### 5.4 Test Executor Agent

- **Owns:** running the gate commands and reporting exact output.
- **Gate commands (minimum):**
  - `cargo test -p turul-a2a`
  - `cargo test -p turul-a2a-auth`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo fmt --all -- --check`
- **Also runs:** any targeted ADR-015 test added by the Test Writer
  Agent; reports pass/fail per test.
- **Constraint:** does NOT modify source, does NOT broaden scope, does
  NOT decide whether a failure is expected. Reports only.
- **Default-feature regression check:** `cargo check --workspace`
  without `--features grpc` MUST stay tonic-free (ADR-014 §2.2).
  Adding ADR-015 code MUST NOT introduce a new default-feature
  dependency.

### 5.5 Implementation owner (downstream of sign-offs)

After §5.1 and §5.2 sign off on the revised ADR and the red-phase test
suite, a separate implementation pass does:

1. Extend `AgentSkillBuilder` with `security_requirements(...)`; update
   `build()` to populate the field instead of hard-coding empty.
2. Extend `AgentCardBuilder` with `security_schemes(...)` and
   `security_requirements(...)` for adopter-supplied agent-level
   metadata.
3. Add the **post-merge** validation pass in `A2aServerBuilder::build()`
   per §2.3 — materialize the final augmented card once, run the two
   scheme-reference checks, return `A2aError::InvalidRequest` on
   failure.
4. Run §4 tests green; §5.4 Test Executor confirms.
5. Update docs per §2.5 (declaration-only language, deferred-list
   pointer) — no workflow references in committed code.
6. Single commit: `feat(card): advertise skill-level security_requirements (declaration-only, ADR-015)`.
7. Flip ADR status to **Accepted** in the same commit (convention per
   ADR-014).
8. No version bump — additive and backward-compatible; ships with the
   next scheduled release.

## 6. Rollback

Fully reversible. The change is additive on the builder and introduces
one new post-merge validation branch at server build. Revert leaves
existing cards (which have no skill-level requirements today) compiling
unchanged.

## 7. Open Questions (not blocking acceptance)

- **Q1**: Should `security_schemes()` on the builder merge with, or
  replace, middleware contributions? Proposed default: merge with
  collision detection reusing the existing `schemes_equivalent()` helper
  (`server/mod.rs:639-644`). Identical schemes dedup, conflicting schemes
  error at build time.
- **Q2**: When the upstream proto eventually adds a `skill_id` on
  `Message`, does turul-a2a flip §4.2 to enforce? That is a successor-ADR
  decision; flagging now so the deferred list stays accurate.
