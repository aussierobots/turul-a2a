# conversation-agent

Demonstrates the A2A **task-refinement** flow: a follow-up message
reuses the originating task's `contextId` (and references the prior
`taskId`) so the server treats the second turn as a refinement of the
first. Each refinement creates a **new task** under the same context
and emits an updated artifact with a new `artifactId` but the same
artifact name.

Per A2A v1.0 "Life of a Task": context groups related tasks; tasks
themselves are immutable once terminal; refinements are new tasks.

## Run

```bash
cargo run -p conversation-agent
# binds http://0.0.0.0:3007
```

The startup banner prints the two-step curl sequence. Summary:

```bash
# Step 1 — originate a task. The agent generates an artifact named
# 'sailboat_image.png' with artifactId 'sailboat-v1'.
RESP1=$(curl -sS -X POST http://localhost:3007/message:send \
  -H 'a2a-version: 1.0' -H 'content-type: application/json' \
  -d '{"message":{"messageId":"u1","role":"ROLE_USER","parts":[{"text":"sailboat"}]}}')
CTX=$(echo "$RESP1" | jq -r '.task.contextId')
TID=$(echo "$RESP1" | jq -r '.task.id')

# Step 2 — refine. Pass the same contextId and reference the prior
# taskId. The agent emits a new task in the same context with a
# v2 artifactId; the artifact name is preserved.
curl -sS -X POST http://localhost:3007/message:send \
  -H 'a2a-version: 1.0' -H 'content-type: application/json' \
  -d "{\"message\":{\"messageId\":\"u2\",\"role\":\"ROLE_USER\",\"contextId\":\"$CTX\",\"referenceTaskIds\":[\"$TID\"],\"parts\":[{\"text\":\"make it red\"}]}}" \
  | jq '.task.artifacts[0].name, .task.artifacts[0].artifactId'
# "sailboat_image.png"
# "sailboat-v2"
```

## What this example shows

- `Message.contextId` propagation across turns groups tasks under a
  shared conversational context.
- `Message.referenceTaskIds` lets the server resolve which prior
  artifact the refinement is modifying.
- A refinement task is a **new task**, not a mutation of the prior
  one — both task records remain in storage with terminal
  `Completed` state.
- The artifact `name` stays the same across versions; the
  `artifactId` changes so consumers can distinguish v1 from v2.

## Tests

```bash
cargo test -p conversation-agent
```

`tests/smoke.rs` runs the two-step flow against a live agent and
asserts the artifact-name + artifactId-version contract.

## See also

- `examples/interrupting-agent` — the `INPUT_REQUIRED` interrupted
  state (a different "two turns" pattern: same task, not a
  refinement).
- A2A v1.0 spec — "Life of a Task" / "Task Immutability".
