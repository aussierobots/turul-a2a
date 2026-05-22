---
id: count
name: Count
description: Square the input integer.
tags: [demo, hook-trigger]
examples:
  - '{"n": 3}'
inputModes: [application/json]
outputModes: [application/json]
securityRequirements: []
inputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    n: { type: integer }
  required: [n]
outputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    squared: { type: integer }
  required: [squared]
---
Squares the supplied integer `n` and returns `{"squared": n*n}`.

Deterministic and offline. The skill always returns successfully when the
input validates against the schema; non-integer or missing `n` is rejected
as `SkillError::InvalidRequest`, which the agent's `TerminalHook` records
as a `Failure` outcome.
