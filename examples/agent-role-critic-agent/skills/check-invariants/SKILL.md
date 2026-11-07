---
name: check-invariants
description: Evaluate a value against a list of deterministic rule-based invariants.
metadata:
  a2a-examples: '["{\"value\":\"hello\",\"invariants\":[{\"name\":\"non_empty\",\"check\":\"non_empty\"}]}"]'
  a2a-id: check_invariants
  a2a-input-modes: '["application/json"]'
  a2a-input-schema: assets/input.schema.json
  a2a-name: Check Invariants
  a2a-output-modes: '["application/json"]'
  a2a-output-schema: assets/output.schema.json
  a2a-tags: '["demo","validation"]'
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
