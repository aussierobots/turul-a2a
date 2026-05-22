---
id: check_invariants
name: "Check Invariants"
description: "Evaluate a value against a list of deterministic rule-based invariants."
tags: [demo, validation]
examples:
  - '{"value":"hello","invariants":[{"name":"non_empty","check":"non_empty"}]}'
inputModes: [application/json]
outputModes: [application/json]
securityRequirements: []
inputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    value: {}
    invariants:
      type: array
      items:
        type: object
        properties:
          name:
            type: string
          check:
            type: string
          args:
            type: object
        required: [name, check]
  required: [value, invariants]
outputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    verdict:
      type: string
      enum: [pass, fail]
    failures:
      type: array
      items:
        type: object
        properties:
          name:
            type: string
          reason:
            type: string
        required: [name, reason]
  required: [verdict, failures]
---
Evaluate `value` against each entry in `invariants`. Each entry has a
`name` (free-form label) and a `check` selecting one of four deterministic
rule kinds:

- `non_empty` — fails on `null`, empty string, empty array, empty object.
- `min_length` — `args.min` required; passes when string/array length ≥ min.
- `max_length` — `args.max` required; passes when string/array length ≤ max.
- `contains` — `args.needle` required; substring match for strings, element
  membership for arrays.

The output `verdict` is `"pass"` when every invariant passed, otherwise
`"fail"` with one entry per failure in `failures`. A malformed invariant
(unknown `check`, missing `args` field) is surfaced as a failure entry
rather than aborting the whole evaluation.
