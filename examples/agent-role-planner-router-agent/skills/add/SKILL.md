---
name: add
description: Sum two numbers.
metadata:
  a2a-examples: '["add 3 5","12 + 30"]'
  a2a-input-modes: '["application/json"]'
  a2a-input-schema: assets/input.schema.json
  a2a-name: Add
  a2a-output-modes: '["application/json"]'
  a2a-output-schema: assets/output.schema.json
  a2a-tags: '["demo","arithmetic"]'
---
Adds the two numbers and returns the sum.

This skill is deterministic — there is no LLM dispatch and the Markdown body
is documentation only, not a prompt template.
