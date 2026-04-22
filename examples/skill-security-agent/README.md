# Skill-Security Agent

ADR-015 adopter demo: skill-level `security_requirements` are
**declaration-only**. The public `AgentCard` advertises them for
discovery clients; the framework does NOT route or authorize per
skill.

## What this example shows

One agent with two skills:

| Skill | `skills[].securityRequirements` | Runtime enforcement |
|---|---|---|
| `public-echo` | _(none)_ | governed by installed agent-level middleware |
| `secure-summary` | `[{schemes: {bearer: []}}]` | governed by installed agent-level middleware |

**No auth middleware is installed in this example.** That is
deliberate — it isolates the ADR-015 invariant from agent-level auth.
Every request succeeds regardless of which skill a caller intended,
because authorization is NOT derived from advertised skill-level
metadata.

To see a working agent-level gate, look at `examples/auth-agent`,
which installs `BearerMiddleware` and actually rejects unauthenticated
traffic.

## Run

```bash
cargo run -p skill-security-agent
# → http://0.0.0.0:3006
```

## Inspect advertised per-skill requirements

```bash
curl -s http://localhost:3006/.well-known/agent-card.json \
  | jq '.skills[] | {id, securityRequirements}'
```

Expected output:

```json
{
  "id": "public-echo",
  "securityRequirements": null
}
{
  "id": "secure-summary",
  "securityRequirements": [
    { "schemes": { "bearer": {} } }
  ]
}
```

The empty `{}` under `bearer` is the camelCase JSON shape of an empty
`StringList` (no scopes requested). Add scopes by passing a non-empty
`StringList` from the builder; they will appear under a `list` key.

The `bearer` scheme itself is declared on the card at the agent level:

```bash
curl -s http://localhost:3006/.well-known/agent-card.json | jq '.securitySchemes'
```

That declaration is what lets the post-merge validator in
`A2aServerBuilder::build()` accept the card — every scheme referenced
by a `SecurityRequirement` must also appear in
`securitySchemes`, per ADR-015 §2.3.

## Verify request behavior

With no middleware installed, both skills accept unauthenticated
traffic. Since A2A has no normative skill-target binding on `Message`,
there is no way to "invoke the secure-summary skill specifically" on
the wire — the example deliberately does not invent such a
convention.

```bash
curl -i http://localhost:3006/message:send \
  -H 'Content-Type: application/json' \
  -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"hello"}]}}'
```

Expected: `HTTP/1.1 200 OK` and a `Task` in the response body.

If you were to edit this example to install
`turul_a2a_auth::BearerMiddleware` and re-run, the SAME request would
return `401 Unauthorized` — but that rejection would apply uniformly,
not per skill.

## What this example does NOT show

- **Per-skill gatekeeping.** The framework cannot route a request to a
  specific skill; there is no `skill_id` on `Message`. If per-skill
  enforcement is a requirement for your deployment, classify the
  message inside your `AgentExecutor` and return an `A2aError` from
  `execute()` — that is outside the ADR-015 surface.
- **A `metadata["a2a.skillId"]` convention.** Intentionally omitted.
  ADR-015 §1.3 explains why.

## See also

- `docs/adr/ADR-015-skill-level-security-requirements.md` for the full
  rationale and invariants.
- `examples/auth-agent` for a working agent-level bearer middleware
  that actually rejects unauthenticated requests.
