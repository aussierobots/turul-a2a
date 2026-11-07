---
name: count
description: Square the input integer.
metadata:
  a2a-examples: '["{\"n\": 3}"]'
  a2a-input-modes: '["application/json"]'
  a2a-input-schema: assets/input.schema.json
  a2a-name: Count
  a2a-output-modes: '["application/json"]'
  a2a-output-schema: assets/output.schema.json
  a2a-tags: '["demo","hook-trigger"]'
---
Squares the supplied integer `n` and returns `{"squared": n*n}`.

Deterministic and offline. The skill always returns successfully when the
input validates against the schema; non-integer or missing `n` is rejected
as `SkillError::InvalidRequest`, which the agent's `TerminalHook` records
as a `Failure` outcome.
