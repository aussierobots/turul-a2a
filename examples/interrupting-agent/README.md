# interrupting-agent

Demonstrates the A2A **`INPUT_REQUIRED`** interrupted state: the
agent pauses mid-execution to ask the caller a question, then resumes
the **same task** (same `taskId`) when the caller replies.

Per A2A v1.0 "Life of a Task", `INPUT_REQUIRED` is one of the two
interrupted states (the other is `AUTH_REQUIRED`) that allow a task
to pause for user input without terminating. The framework only
accepts continuations on tasks in one of these two states.

## Run

```bash
cargo run -p interrupting-agent
# binds http://0.0.0.0:3008
```

The startup banner prints both turns. Summary:

```bash
# Turn 1 — initial request. The agent recognises it needs a
# destination and stops with INPUT_REQUIRED, returning a prompt.
RESP1=$(curl -sS -X POST http://localhost:3008/message:send \
  -H 'a2a-version: 1.0' -H 'content-type: application/json' \
  -d '{"message":{"messageId":"u1","role":"ROLE_USER","parts":[{"text":"book a flight"}]}}')
echo "$RESP1" | jq '.task | {state: .status.state, prompt: .status.message.parts[0].text}'
# { "state": "TASK_STATE_INPUT_REQUIRED",
#   "prompt": "Where would you like to fly? Please reply with the destination." }

TID=$(echo "$RESP1" | jq -r '.task.id')
CTX=$(echo "$RESP1" | jq -r '.task.contextId')

# Turn 2 — continuation. SAME taskId; the agent resumes the same
# task instance with the answer and completes it.
curl -sS -X POST http://localhost:3008/message:send \
  -H 'a2a-version: 1.0' -H 'content-type: application/json' \
  -d "{\"message\":{\"messageId\":\"u2\",\"role\":\"ROLE_USER\",\"taskId\":\"$TID\",\"contextId\":\"$CTX\",\"parts\":[{\"text\":\"Helsinki\"}]}}" \
  | jq '.task | {state: .status.state, artifact: .artifacts[0].parts[0].text}'
# { "state": "TASK_STATE_COMPLETED",
#   "artifact": "Booked flight to Helsinki.\noriginal request: book a flight\nconfirmed answer: Helsinki" }
```

## What this example shows

- An executor can return without terminating the task — it sets the
  status to `INPUT_REQUIRED` with a prompt message and the framework
  persists the task in the interrupted state.
- The caller resumes the same task by sending a follow-up message
  with **the same `taskId`** (not a new one).
- The framework accepts continuations only on tasks in
  `INPUT_REQUIRED` or `AUTH_REQUIRED` states — any other state
  rejects with `InvalidRequestError`.
- This is different from `conversation-agent`: there each turn is a
  new task in the same context (refinement); here both turns share
  one task id (continuation).

## Tests

```bash
cargo test -p interrupting-agent
```

`tests/smoke.rs` exercises the two-turn flow end-to-end.

## See also

- `examples/conversation-agent` — task refinement (new task per turn,
  same context).
- A2A v1.0 spec — "Life of a Task" interrupted-state section.
