---
name: validate-against-schema
description: Validate a value against a JSON Schema 2020-12; return verdict + first violation.
metadata:
  a2a-examples: '["{\"value\":42,\"schema\":{\"type\":\"integer\"}}"]'
  a2a-id: validate_against_schema
  a2a-input-modes: '["application/json"]'
  a2a-input-schema: assets/input.schema.json
  a2a-name: Validate Against Schema
  a2a-output-modes: '["application/json"]'
  a2a-output-schema: assets/output.schema.json
  a2a-tags: '["demo","validation"]'
---
Validate the supplied `value` against the supplied JSON Schema 2020-12 `schema`.
On success, return `{"valid": true, "errors": []}`. On the first violation,
return `{"valid": false, "errors": ["<violation message>"]}`.

The handler runs `turul_a2a_patterns::validate_json` internally; the
`schema` field in the manifest input is intentionally typed as
`{"type": "object"}` because the *contained* JSON Schema is data, not a
schema declared at manifest authoring time.
