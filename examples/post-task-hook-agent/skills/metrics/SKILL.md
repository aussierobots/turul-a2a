---
name: metrics
description: Report the running TerminalHook counter state (read-only).
metadata:
  a2a-examples: '["metrics"]'
  a2a-input-modes: '["application/json"]'
  a2a-input-schema: assets/input.schema.json
  a2a-name: Metrics
  a2a-output-modes: '["application/json"]'
  a2a-output-schema: assets/output.schema.json
  a2a-tags: '["demo","observability"]'
---
Returns the running outcome counter populated by the example's
`RecordingHook`: `{"success": <n>, "failure": <m>, "last": <string|null>}`.

Read-only: the skill itself does not mutate the counter. The counter
moves only when a prior skill (such as `count`) returns and the
`TerminalHook` records that outcome.
