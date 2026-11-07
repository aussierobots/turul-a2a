---
name: echo-loud
description: Echo the input string in uppercase.
metadata:
  a2a-examples: '["{\"text\": \"hello\"}  \u2192 {\"shouted\": \"HELLO\"}"]'
  a2a-id: echo_loud
  a2a-input-modes: '["application/json"]'
  a2a-input-schema: assets/input.schema.json
  a2a-name: Echo Loud
  a2a-output-modes: '["application/json"]'
  a2a-output-schema: assets/output.schema.json
  a2a-tags: '["demo","dispatch","profile"]'
---
Returns the uppercase form of the input string.

Routed via the `https://turul.dev/a2a/extensions/skill-invocation/v1`
profile: clients set `Message.metadata["a2a.skillId"] = "echo_loud"`
and put the `text` argument in `Message.metadata["a2a.skillParams"]`.
