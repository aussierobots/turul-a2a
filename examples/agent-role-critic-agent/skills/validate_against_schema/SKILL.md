---
id: validate_against_schema
name: "Validate Against Schema"
description: "Validate a value against a JSON Schema 2020-12; return verdict + first violation."
tags: [demo, validation]
examples:
  - '{"value":42,"schema":{"type":"integer"}}'
inputModes: [application/json]
outputModes: [application/json]
securityRequirements: []
inputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    value: {}
    schema:
      type: object
  required: [value, schema]
outputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    valid:
      type: boolean
    errors:
      type: array
      items:
        type: string
  required: [valid, errors]
---
Validate the supplied `value` against the supplied JSON Schema 2020-12 `schema`.
On success, return `{"valid": true, "errors": []}`. On the first violation,
return `{"valid": false, "errors": ["<violation message>"]}`.

The handler runs `turul_a2a_patterns::validate_json` internally; the
`schema` field in the manifest input is intentionally typed as
`{"type": "object"}` because the *contained* JSON Schema is data, not a
schema declared at manifest authoring time.
