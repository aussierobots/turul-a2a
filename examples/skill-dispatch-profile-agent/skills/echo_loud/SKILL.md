---
id: echo_loud
name: Echo Loud
description: Echo the input string in uppercase.
tags: [demo, dispatch, profile]
examples:
  - "{\"text\": \"hello\"}  → {\"shouted\": \"HELLO\"}"
inputModes: [application/json]
outputModes: [application/json]
securityRequirements: []
inputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    text: { type: string, minLength: 1 }
  required: [text]
outputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    shouted: { type: string, minLength: 1 }
  required: [shouted]
---
Returns the uppercase form of the input string.

Routed via the `https://turul.dev/a2a/extensions/skill-invocation/v1`
profile: clients set `Message.metadata["a2a.skillId"] = "echo_loud"`
and put the `text` argument in `Message.metadata["a2a.skillParams"]`.
