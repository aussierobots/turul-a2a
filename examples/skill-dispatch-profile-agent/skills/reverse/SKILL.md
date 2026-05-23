---
id: reverse
name: Reverse
description: Reverse the input string character-by-character.
tags: [demo, dispatch, profile]
examples:
  - "{\"text\": \"abc\"}  → {\"reversed\": \"cba\"}"
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
    reversed: { type: string }
  required: [reversed]
---
Returns the input string reversed.

Routed via the `https://turul.dev/a2a/extensions/skill-invocation/v1`
profile: clients set `Message.metadata["a2a.skillId"] = "reverse"` and
put the `text` argument in `Message.metadata["a2a.skillParams"]`.
