---
id: metrics
name: Metrics
description: Report the running TerminalHook counter state (read-only).
tags: [demo, observability]
examples:
  - "metrics"
inputModes: [application/json]
outputModes: [application/json]
securityRequirements: []
inputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties: {}
outputSchema:
  $schema: "https://json-schema.org/draft/2020-12/schema"
  type: object
  properties:
    success: { type: integer }
    failure: { type: integer }
    last:
      type: [string, "null"]
  required: [success, failure, last]
---
Returns the running outcome counter populated by the example's
`RecordingHook`: `{"success": <n>, "failure": <m>, "last": <string|null>}`.

Read-only: the skill itself does not mutate the counter. The counter
moves only when a prior skill (such as `count`) returns and the
`TerminalHook` records that outcome.
