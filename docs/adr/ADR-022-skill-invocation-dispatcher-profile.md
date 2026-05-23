# ADR-022: Skill-Invocation Dispatcher — Profile Extension Convention

- **Status:** Accepted
- **Date:** 2026-05-22
- **Accepted:** 2026-05-23 (red-phase tests + placement sketch landed)

## Acceptance gates cleared (2026-05-23)

Three blockers were named when this ADR was Proposed. All three are
now satisfied. Recorded here for the audit trail; the live status is
`Accepted` (top of file).

1. **Red-phase test suite landed.** `crates/turul-a2a/tests/profile_dispatch_tests.rs`
   (343 LOC, 13 tests) pins:
   - `A2A-Extensions` request-header parsing,
   - `Message.metadata["a2a.skillId"]` + `["a2a.skillParams"]` keyed routing,
   - `UnsupportedOperationError` on a `required` extension the server doesn't honour,
   - echo of activated URIs on the response header.

2. **First-profile placement decided.** Per ADR-021 §5 (single
   profile inhabitant lives in the transport-owning crate), the
   dispatcher is implemented inside `crates/turul-a2a/src/profile_dispatch.rs`
   (197 LOC; `#[doc(hidden)] pub mod` — internal plumbing) and wired
   into router (`src/router.rs`), JSON-RPC (`src/jsonrpc.rs`), and
   gRPC (`src/grpc/service.rs`). The stable adopter-facing surface
   is `turul_a2a::profiles` — a thin re-export module that exposes
   only the URI constants and header names, leaving the parse /
   validate / response-header helpers hidden. A new
   profile/extensions crate is not introduced; the trigger for one
   remains ≥2 profile inhabitants.

3. **Interop probe landed.** `examples/skill-dispatch-profile-agent/`
   advertises the profile URI; `examples/interop-clients/skill-dispatch-profile/{python,go,rust}/`
   exercise the dispatch path end-to-end. All three smoke-verified:
   header activation + metadata-keyed routing + response echo
   confirmed in each language. `examples/interop-clients/CLIENT_MATRIX.md`
   records the row.

The contract committed by acceptance: profile URI
`https://turul.dev/a2a/extensions/skill-invocation/v1` (exposed as
`turul_a2a::profiles::SKILL_INVOCATION_PROFILE_V1`), header
activation, metadata-keyed request shape, response echo.

**Why not Reject:** the four-point contract is the right shape for
A2A's extension model; the spec-compliance review found no drift.
Rejection would only make sense if upstream A2A added a normative
`skill_id` field to `Message` — that hasn't happened.
- **Depends on:** ADR-001 (proto-first architecture), ADR-015
  (declaration-only precedent for skill-level security), ADR-021
  (patterns extraction; deferred dispatcher §2.4/§2.5 + four-point
  spec §2.5)
- **Spec reference:** `proto/a2a.proto` L233 (`Part.data:
  google.protobuf.Value`), L272 (`Message.metadata:
  google.protobuf.Struct`), L273-274 (`Message.extensions: repeated
  string`), L260-277 (`Message`), L412 (`AgentCapabilities.extensions:
  repeated AgentExtension`), L418-427 (`AgentExtension { uri,
  description, required, params }`), L430-447 (`AgentSkill`),
  L642-651 (`SendMessageRequest`); A2A spec extensions activation:
  https://a2a-protocol.org/latest/topics/extensions/

> **Sections 1–8 below are reference / seed material.** The current
> contract is fully captured at the top of this file (Status,
> Acceptance gates cleared, plus the §2 Decision summary that
> follows). Sections 1, 3, 5, 6, 7, 8 describe the design reasoning
> from when the ADR was Proposed; they remain useful as background
> but are no longer normative beyond what the top of the file
> already states.

## 1. Context

### 1.1 A2A v1.0 silence on skill targeting

The A2A v1.0 proto has no normative `skill_id` binding on `Message`
(L260-277) or `SendMessageRequest` (L642-651). A client sending a
message to a multi-skill agent cannot express "route this to skill X"
through any proto-defined channel. The fields available for carrying
such intent are `Message.metadata` (`google.protobuf.Struct`, L272)
and `Part.data` (`google.protobuf.Value`, L233) — both free-form and
not normatively reserved for skill routing.

ADR-021 §1.3 documents this gap as a load-bearing constraint. Any
skill-routing convention is therefore a Turul-local extension, not a
spec-native behaviour.

### 1.2 Dispatcher is currently example-owned

The `examples/skill-manifest-ollama-agent` example contains a
dispatcher that routes inbound messages to skills based on payload
inspection. Per ADR-021 §2.4, this is correct placement today —
the dispatcher is *wire-affecting* (it defines what clients must
send) and therefore belongs in the profile-extensions bucket, not
in `turul-a2a-patterns`.

The consequence for adopters is that every multi-skill agent
reinvents the dispatcher, and no two are interoperable. This ADR
proposes the framework-level convention.

### 1.3 Wire-affecting classification

Any dispatcher convention changes what a well-behaved client must
include in its request. That makes it observable on the wire:
the client's request to a dispatcher-aware agent differs from its
request to a dispatcher-unaware one. Per ADR-021 §1.1, this
classification places the dispatcher squarely in the
profile-extensions bucket — not patterns.

The activation path mandated by A2A is the HTTP `A2A-Extensions`
header (A2A spec §"Extensions"). The server's response SHOULD echo
activated URIs in the same header. This is the correct hook; no
Turul-local mechanism is needed.

### 1.4 Four-point contract from ADR-021 §2.5

ADR-021 §2.5 requires that any successor profile extension specify
all four points: declaration, activation, request shape, and
rejection. This ADR satisfies that requirement for the
skill-invocation dispatcher profile.

## 2. Decision

### 2.1 Declaration — extension URI and `AgentExtension` entry

The extension is identified by the Turul-owned URI:

```
https://turul.dev/a2a/extensions/skill-invocation/v1
```

Servers that implement this profile advertise it by adding an
`AgentExtension` entry to `AgentCapabilities.extensions` (proto
L412) in their `AgentCard`:

```proto
AgentExtension {
  uri:         "https://turul.dev/a2a/extensions/skill-invocation/v1"
  description: "Routes messages to registered skills via Message.metadata
                keys a2a.skillId and a2a.skillParams."
  required:    <bool>  // adopter choice; see §2.4
  params:      {}      // reserved; empty for v1
}
```

**Versioning convention.** The version appears in the URI path
(`/v1`). A breaking change to the request shape or rejection
semantics increments the path component (`/v2`, etc.) and is
treated as a distinct extension URI — agents MAY advertise both
simultaneously during a transition. Non-breaking additions (new
optional metadata keys) do not change the URI. The version suffix
is path-in-URI, not embedded in `AgentExtension.description`;
see §8 Q1 for rationale.

**Parameter schema.** `AgentExtension.params` (proto L426,
`google.protobuf.Struct`) is reserved for future use. Servers
SHOULD leave it empty for `v1`; clients MUST NOT require it to be
non-empty.

**Per-skill advertising.** `AgentSkill` (L430-447) has no
`extensions` field. Advertising which skills are reachable via
this extension is done through the `AgentExtension.description`
prose in the `AgentCard`, and optionally through the
`turul-a2a-patterns` `SkillRegistry` descriptor surface (ADR-021
§2.2 item 2) in process. No proto change is required or proposed.

### 2.2 Activation — `A2A-Extensions` HTTP header

The dispatcher profile is applied to a request **only when** the
client sends the HTTP header:

```
A2A-Extensions: https://turul.dev/a2a/extensions/skill-invocation/v1
```

Multiple extensions may be comma-separated per the A2A spec. The
server's response SHOULD echo all activated URIs in the same
header:

```
A2A-Extensions: https://turul.dev/a2a/extensions/skill-invocation/v1
```

**No-header default.** A request that arrives without the
`A2A-Extensions` header — or with a header that does not include
this URI — is treated as un-targeted. The server runs its default
executor logic without skill dispatch. No error is returned for
the absence of the header unless the extension was advertised as
`required = true` (see §2.4).

**gRPC transport.** Per ADR-014, gRPC transport mirrors HTTP
behaviour. The `A2A-Extensions` header is carried as a gRPC
metadata key (lowercase) with the same comma-separated URI list.
Activation and rejection semantics are identical across transports.

**Lambda transport.** Lambda request/response (ADR-008) passes
headers through the API Gateway event. The header is visible to
the router layer on the Lambda handler side. No special handling
is required.

### 2.3 Request shape — `Message.metadata` with reserved keys

**Chosen: `Message.metadata` (`google.protobuf.Struct`, proto
L272).**

When the extension is activated (§2.2), the client places
skill-routing data in `Message.metadata` under two reserved keys:

| Key | Type | Semantics |
|---|---|---|
| `a2a.skillId` | string | The `AgentSkill.id` value this message targets (proto L432, `REQUIRED` identifier) |
| `a2a.skillParams` | object | Optional. Structured parameters for the skill; validated against `SkillDescriptor.params_schema` (ADR-021 §2.2 item 4) if present |

Example wire representation (JSON, camelCase per proto JSON
mapping):

```json
{
  "messageId": "msg-001",
  "role": "ROLE_USER",
  "parts": [{ "text": "Summarise this quarter's results." }],
  "metadata": {
    "a2a.skillId": "quarterly-summary",
    "a2a.skillParams": {
      "format": "bullet",
      "maxItems": 5
    }
  }
}
```

**Key namespace rationale.** The `a2a.` prefix is a Turul-local
reservation within the `Message.metadata` map. It is not defined
by the upstream A2A spec. The prefix is chosen to be legible to
A2A-aware consumers while being clearly scoped — compare with
Kubernetes annotation conventions. No upstream spec registration
is claimed. If the upstream proto eventually adds `skill_id`
natively on `Message`, this extension becomes a migration path to
that field; see §8 Q3.

**Why `Message.metadata` over the alternatives** (see §5 for
full rejection rationale):

- `Message.metadata` is a flat `Struct` that accepts arbitrary
  key-value pairs. Adding two keys is the minimum invasive change;
  it does not redefine the semantics of any existing field.
- Text parts remain free for human-readable content; skill-routing
  metadata does not pollute the conversation payload.
- `Part.data` (L233, `google.protobuf.Value`) is part of a `oneof
  content` and semantically the payload of one content unit — not
  an envelope for message-level routing decisions.
- A first text-part JSON envelope (the example crate's current
  approach) conflates payload with routing and is not
  proto-idiomatic.

**Absent keys.** If either key is missing when the extension is
activated, the server SHOULD respond with `InvalidParamsError`
(A2A error model: `-32602`, HTTP 400) citing the missing key.
`a2a.skillParams` is optional; its absence is not an error.

**Additional metadata.** Clients MAY include other keys in
`Message.metadata` alongside the reserved keys; the server MUST
NOT reject messages solely because `metadata` contains keys
outside the `a2a.*` namespace.

### 2.4 Rejection — `UnsupportedOperationError` for required extensions

**When `required = true` and the client does not activate:**

If `AgentExtension.required = true` (proto L424) and the inbound
request does not include the activation header (§2.2), the server
MUST return `UnsupportedOperationError`:

- HTTP transport: `400 Bad Request`, JSON-RPC error code `-32004`.
- gRPC transport: `UNIMPLEMENTED` status with A2A error detail
  (`google.rpc.ErrorInfo`, `reason = "UNSUPPORTED_OPERATION"`,
  `domain = "a2a-protocol.org"`).

This aligns with the A2A error model as used elsewhere in
turul-a2a (e.g. SubscribeToTask on a terminal task) and with
ADR-021 §2.5 point 4.

**When `required = false` and the client does not activate:**

No error. The server runs its default executor logic without skill
dispatch (§2.2 no-header default).

**When the client activates but the `a2a.skillId` key is absent:**

`InvalidParamsError` (§2.3 absent-keys behaviour), regardless of
`required`.

**When the client provides a `a2a.skillId` that does not match any
registered skill:**

The server SHOULD return `InvalidParamsError` citing the unrecognised
skill id. Adopters may choose to fall through to default executor
logic instead; this ADR does not mandate a hard reject for unknown
skill ids in order to preserve executor flexibility.

### 2.5 Activation hook placement — `turul-a2a` router layer

Per ADR-021 §1.2, the code that:

1. Reads the `A2A-Extensions` request header,
2. Identifies whether the skill-invocation URI is present,
3. Echoes activated URIs on the response,
4. Enforces the `required = true` rejection path (§2.4), and
5. Passes the resolved `a2a.skillId` and `a2a.skillParams` to the
   registered dispatcher,

belongs in `turul-a2a`'s transport/router layer
(`crates/turul-a2a/src/router.rs`) and its gRPC/Lambda equivalents.
NOT in `turul-a2a-patterns`.

`turul-a2a-patterns` MAY host:

- The URI constant (`pub const SKILL_INVOCATION_V1_URI: &str = "…"`),
- A `SkillDispatchPayload { skill_id, params }` parsed-value struct,
- A validator that reads `Message.metadata` and extracts the two
  reserved keys into a `SkillDispatchPayload`,

because none of those touch the wire directly — they are parsing
helpers. The router layer consumes them; it does not move them into
the patterns crate's public surface.

The `turul-a2a-patterns` crate MUST NOT gain a dependency on
`turul-a2a` (ADR-021 §2.3, §6 dep direction rule). The above
helpers depend only on `turul-a2a-proto` types and on the standard
library.

## 3. Non-Goals

- **MCP tool references.** Connecting `a2a.skillId` to a named MCP
  tool or resource is not in scope. MCP routing is an adopter
  concern.
- **Chained-dispatcher routing.** This profile defines single-hop
  skill targeting from client to server. Multi-hop or nested
  dispatcher chains are not addressed.
- **Adding `skill_id` to the A2A proto.** That is an upstream proto
  change outside this project's control. This ADR is explicitly a
  Turul-local convention intended to be superseded if/when the
  upstream spec provides a normative field (see §8 Q3).
- **Per-request authentication based on the targeted skill.** ADR-015
  documents skill-level security as declaration-only. This ADR does
  not change that. Routing a message to a skill does not trigger
  skill-level auth enforcement — that remains the adopter's
  responsibility inside `AgentExecutor`.
- **Modifying `turul-a2a-proto` or `turul-a2a-types`.** This ADR adds
  no new proto-generated types. `AgentExtension` (L418-427) and
  `Message.metadata` (L272) are already in the proto; no new struct
  or wrapper is required.

## 4. Backwards Compatibility and Interop

### 4.1 Clients that do not know the URI

A client that has never heard of this extension sends no
`A2A-Extensions` header. If the server advertises the extension as
`required = false` (the default recommendation for migration), the
request reaches the default executor unchanged. Existing clients
are fully compatible.

If the server advertises `required = true`, the extension is a
hard prerequisite for the agent — the client must upgrade or the
agent will reject its requests. Adopters SHOULD default to
`required = false` during rollout and graduate to `required = true`
only when all known clients have been updated.

### 4.2 Servers that do not advertise the extension

A client that includes `A2A-Extensions: https://turul.dev/a2a/extensions/skill-invocation/v1`
in a request to a server that does not support it receives no echo
header in the response. Per the A2A spec, the server is not
obligated to error on an unknown extension unless the extension is
marked `required`. The client SHOULD treat the absence of the echo
as "extension not activated" and degrade gracefully.

### 4.3 Interop with non-Turul A2A servers

Because the URI is Turul-owned and not an upstream spec construct,
non-Turul A2A implementations will neither recognise nor echo it.
A client sending the header to a non-Turul server behaves as in
§4.2. There is no cross-implementation interop claim for this
extension.

### 4.4 `Message.metadata` key collision risk

The `a2a.` prefix is a Turul-local reservation. Other Turul-owned
extensions that carry per-message metadata SHOULD use distinct
prefixes (e.g. `turul.` for fully internal keys not intended for
other consumers). Adopters MUST NOT use the `a2a.skillId` or
`a2a.skillParams` keys for non-dispatcher purposes; the patterns
crate validator (§2.5) will interpret them as skill-routing intent
when the extension is activated.

## 5. Rejected Alternatives

### 5.1 `Part.data` envelope

Placing `{"skillId": "…", "params": {…}}` in a `Part` with
`content = data` (proto L233, `google.protobuf.Value`) is more
structured than `Message.metadata` keys in isolation, but has
three defects:

1. `Part` is a content unit — its semantics are "this is payload",
   not "this is routing metadata". A routing envelope in a `Part`
   conflates transport control with message content.
2. It forces the server to scan all parts for a dispatch envelope,
   mixing routing logic with content inspection.
3. It complicates clients: a multi-modal message (text + image +
   routing) must now also carry a structured `data` part that has
   no user-visible meaning.

`Message.metadata` is the purpose-built carrier for per-message
annotations. The choice mirrors how `SendMessageRequest.metadata`
(L650) is used for request-level context.

### 5.2 First text-part JSON envelope

The `examples/skill-manifest-ollama-agent` example currently
routes by inspecting the first text part for a JSON object with a
`skillId` key. This approach is not proto-idiomatic (text parts
are human-readable content by spec convention), forces the server
to attempt JSON parsing of every text part, and makes it
impossible for a client to send a plain-text message to a
multi-skill agent without the server misinterpreting it.

### 5.3 `Message.extensions` attribution channel

`Message.extensions` (proto L273-274, `repeated string`) is an
attribution channel — it lists URIs of extensions "present or
contributed to this Message" (proto comment). It is NOT a routing
channel; its semantic is declaration, not invocation. Writing skill
ids into `extensions` would misuse the field and conflict with
legitimate attribution use.

### 5.4 Version in `AgentExtension.description` rather than URI path

Embedding the version only in the human-readable `description`
field (e.g. `"version": "1"` inside the `params` struct) would
make the URI stable across breaking changes. This hides the
breaking change from any client that inspects the URI to decide
whether to activate. The URI path carries version as a
machine-readable signal; description prose supplements it for
human readers. See §8 Q1 for the open question about whether both
mechanisms are needed simultaneously.

## 6. Implementation Triggers

> *Historical section.* This text was written when the ADR was
> Proposed and recorded the gates that had to fire before code
> shipped. Both gates fired before acceptance and the implementation
> has landed. The current state:
>
> - Gate 1 (acceptance) fired 2026-05-23 — see top-of-file Status
>   and the "Acceptance gates cleared" section.
> - Gate 2 (location for the URI constant) was resolved by placing
>   `SKILL_INVOCATION_PROFILE_V1` inside `turul-a2a` and re-exporting
>   it from the stable `turul_a2a::profiles` module, rather than
>   waiting on `turul-a2a-patterns` to become publishable. The
>   patterns crate stays internal; the URI constant lives in the
>   transport-owning crate per ADR-021 §5 single-profile placement.
> - The reference example is `examples/skill-dispatch-profile-agent/`
>   (port 3015) with Python/Go/Rust interop clients.

## 7. Phase A Acceptance Gates

### 7.1 A2A Spec Compliance sign-off

Before acceptance, a spec-compliance review MUST confirm:

- The vendored `proto/a2a.proto` SHA256 still matches upstream
  `a2aproject/A2A:main/specification/a2a.proto`.
- `Message.metadata` at L272 is `google.protobuf.Struct` (flat map)
  and is the correct carrier for per-message annotations.
- `Part.data` at L233 is `google.protobuf.Value` inside `oneof
  content` — confirming §5.1's rationale.
- `Message.extensions` at L273-274 is `repeated string` used as
  attribution, not routing — confirming §5.3's rationale.
- `AgentExtension` at L418-427 has exactly four fields (`uri`,
  `description`, `required`, `params`) — confirming §2.1's
  struct usage.
- `AgentCapabilities.extensions` at L412 is `repeated
  AgentExtension` — confirming the declaration surface.
- The A2A spec's `A2A-Extensions` header mechanism
  (https://a2a-protocol.org/latest/topics/extensions/) describes
  the activation convention this ADR relies on.
- No Turul-local convention introduced by this ADR conflicts with
  an existing normative use of any cited field.

Sign-off statement: "ADR-022 is spec-truthful as of proto SHA256
`<hash>`."

### 7.2 Red-phase test sketch

A test-sketch review (descriptive, no implementation code) must
identify the following test cases before acceptance. Tests are not
written until §6's implementation triggers are met, but the sketch
must be reviewable alongside this ADR.

**Router-layer tests (live in `crates/turul-a2a/tests/` or
`router.rs::tests`):**

1. **`extension_not_activated_runs_default_executor`** — request
   arrives with no `A2A-Extensions` header; server has the
   dispatcher registered as `required = false`; default executor
   receives the message unchanged.

2. **`extension_activated_routes_to_registered_skill`** — request
   arrives with `A2A-Extensions: https://turul.dev/a2a/extensions/skill-invocation/v1`
   and `Message.metadata` containing `a2a.skillId = "demo"`; server
   has the skill "demo" registered; the skill handler receives the
   call; response echo header includes the URI.

3. **`extension_required_no_header_returns_unsupported_operation`** —
   extension advertised as `required = true`; request has no
   header; server returns `UnsupportedOperationError` (HTTP 400,
   JSON-RPC `-32004`).

4. **`extension_activated_missing_skill_id_returns_invalid_params`** —
   activation header present; `Message.metadata` does not contain
   `a2a.skillId`; server returns `InvalidParamsError` (HTTP 400,
   JSON-RPC `-32602`).

5. **`extension_required_false_unknown_skill_id_behaviour`** — server
   MAY fall through to default executor or return
   `InvalidParamsError`; test documents both branches and the
   adopter-configuration point.

**`turul-a2a-patterns` unit tests (live in
`crates/turul-a2a-patterns/tests/` once the crate exists):**

6. **`dispatch_payload_parses_metadata_with_both_keys`** — a
   `Message.metadata` `Struct` with `a2a.skillId` and
   `a2a.skillParams` round-trips through the patterns-crate
   validator into a `SkillDispatchPayload`.

7. **`dispatch_payload_absent_skill_id_is_err`** — metadata without
   `a2a.skillId` produces a structured parse error.

8. **`dispatch_payload_absent_params_is_ok`** — metadata with only
   `a2a.skillId` (no `a2a.skillParams`) is valid.

## 8. Open Questions (did not block acceptance)

- **Q1 — version in URI path vs `params`**: This ADR places the
  version in the URI path (`/v1`). An alternative is to keep the URI
  stable and embed a version in `AgentExtension.params` (e.g.
  `{"schemaVersion": "1"}`). The path approach is cleaner for
  client activation logic (URI comparison is unambiguous); the
  params approach allows in-place negotiation. Resolve at the first
  breaking change to this extension's request shape, when the
  concrete tradeoff becomes observable.

- **Q2 — per-message vs per-session activation**: This ADR activates
  the dispatcher per-message via the `A2A-Extensions` header on
  each request. An alternative is a session-level activation (e.g.
  set once on the first message in a context and inherited by
  subsequent messages in the same `context_id`). Session-level
  activation would require the server to persist activation state
  per context — adding storage coupling the current design avoids.
  Deferred until a concrete adopter need surfaces.

- **Q3 — migration path when upstream proto adds `skill_id`**: If
  `a2aproject/A2A` adds a normative `skill_id` field to `Message`
  or `SendMessageRequest`, this extension should be deprecated in
  favour of the spec-native field. The migration path is: (a)
  support both the spec-native field and the `a2a.skillId` metadata
  key for one minor release, (b) remove the extension declaration
  from `AgentCapabilities.extensions` in a subsequent minor, (c)
  flip this ADR to Superseded. Deferred; track via §2.8 of the
  ADR-021 triggers list.

- **Q4 — `a2a.skillParams` schema publication on the wire**: This
  ADR keeps `SkillDescriptor.params_schema` (ADR-021 §2.2 item 4)
  as an in-process planning helper, not advertised on the wire.
  A future amendment could publish the schema inside
  `AgentExtension.params` or a skill-level sidecar so that clients
  can validate their `a2a.skillParams` before sending. Deferred;
  requires concrete adopter demand and a schema-publication shape
  that does not conflict with `AgentExtension.params`'s current
  "reserved empty" status in `v1`.
