---
id: add
name: Add
description: Sum two numbers.
tags: [demo, arithmetic]
examples:
  - "add 3 5"
  - "12 + 30"
inputModes: [application/json]
outputModes: [application/json]
securityRequirements: []
inputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    a: { type: number }
    b: { type: number }
  required: [a, b]
outputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    result: { type: number }
  required: [result]
---
Adds the two numbers and returns the sum.

This skill is deterministic — there is no LLM dispatch and the Markdown body
is documentation only, not a prompt template.
