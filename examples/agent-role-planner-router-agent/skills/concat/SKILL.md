---
name: concat
description: Join an array of strings with single spaces.
metadata:
  a2a-examples: '["concat: foo bar baz"]'
  a2a-input-modes: '["application/json"]'
  a2a-input-schema: assets/input.schema.json
  a2a-name: Concat
  a2a-output-modes: '["application/json"]'
  a2a-output-schema: assets/output.schema.json
  a2a-tags: '["demo","strings"]'
---
Joins the provided strings with single spaces and returns the result.

This skill is deterministic — there is no LLM dispatch and the Markdown body
is documentation only, not a prompt template.
