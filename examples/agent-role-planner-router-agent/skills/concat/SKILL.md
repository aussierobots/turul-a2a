---
id: concat
name: Concat
description: Join an array of strings with single spaces.
tags: [demo, strings]
examples:
  - "concat: foo bar baz"
inputModes: [application/json]
outputModes: [application/json]
securityRequirements: []
inputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    strings:
      type: array
      items: { type: string }
  required: [strings]
outputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    joined: { type: string }
  required: [joined]
---
Joins the provided strings with single spaces and returns the result.

This skill is deterministic — there is no LLM dispatch and the Markdown body
is documentation only, not a prompt template.
