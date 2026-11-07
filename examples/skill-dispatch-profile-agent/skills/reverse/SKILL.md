---
name: reverse
description: Reverse the input string character-by-character.
metadata:
  a2a-examples: '["{\"text\": \"abc\"}  \u2192 {\"reversed\": \"cba\"}"]'
  a2a-input-modes: '["application/json"]'
  a2a-input-schema: assets/input.schema.json
  a2a-name: Reverse
  a2a-output-modes: '["application/json"]'
  a2a-output-schema: assets/output.schema.json
  a2a-tags: '["demo","dispatch","profile"]'
---
Returns the input string reversed.

Routed via the `https://turul.dev/a2a/extensions/skill-invocation/v1`
profile: clients set `Message.metadata["a2a.skillId"] = "reverse"` and
put the `text` argument in `Message.metadata["a2a.skillParams"]`.
