# Go interop client → `post-task-hook-agent`

A self-contained Go program that calls one specific turul-a2a example
agent and prints its responses.

## What is A2A?

[A2A (Agent-to-Agent)](https://a2a-protocol.org) is an open protocol for
agent-to-agent communication. An A2A server publishes an **AgentCard** at
`/.well-known/agent-card.json`. Clients resolve that card, pick a
supported transport, then exchange `Message`s and receive `Task`s.

Normative spec: <https://a2a-protocol.org/latest/specification/>.
This client targets **A2A v1.0**.

## Which agent this calls

[`examples/post-task-hook-agent`](../../../post-task-hook-agent/) — a
turul-a2a agent demonstrating the post-task terminal hook pattern. Each
skill call fires a `TerminalHook` that records the outcome to an
in-memory counter; the `metrics` skill exposes the counter snapshot so a
caller can verify the hook fired.

Skills:

- `count`   — input `{"n": <number>}`, output `{"squared": n*n}`.
- `metrics` — no input, returns `{"success": <u64>, "failure": <u64>, "last": <string>}`.

The agent's planner accepts plain text `count <number>` and the bare
keyword `metrics`. JSON is not parsed by this particular planner — keep
the inputs textual.

Default URL: `http://localhost:3014`. Override with `A2A_BASE_URL`.

## What this demonstrates

Four sequential calls:

| # | Text sent | Skill   | Expected artifact text                                |
|---|-----------|---------|-------------------------------------------------------|
| 1 | `count 3` | count   | `{"squared":9}`                                       |
| 2 | `count 3` | count   | `{"squared":9}`                                       |
| 3 | `count 3` | count   | `{"squared":9}`                                       |
| 4 | `metrics` | metrics | `{"success":3,"failure":0,"last":"ok(count): {...}"}` |

The point of the demo is step 4: the counter only reaches `3` if the
terminal hook actually ran after each `count` call.

## Transport used

JSON-RPC over `POST /jsonrpc`. The Go SDK selects this automatically from
the agent's card.

## Run

Open two terminals.

**Terminal 1 — start the agent:**

```bash
cd /path/to/turul-a2a
cargo run -p post-task-hook-agent
```

Wait for `Post-Task Hook Agent listening on http://0.0.0.0:3014`.

**Terminal 2 — run the client:**

```bash
cd /path/to/turul-a2a/examples/interop-clients/post-task-hook/go
go mod tidy   # first time only
go run ./cmd/probe
```

## Expected output

```
a2a-go protocol version: 1.0
target: http://localhost:3014
--- AgentCard ---
Name:    Post-Task Hook Agent
  - JSONRPC @ http://localhost:3014/jsonrpc
--- SendMessage step 1 ---
text: "count 3"
state=TASK_STATE_COMPLETED
  artifact[0] name="count" parts=1
    part[0].text={"squared":9}
--- SendMessage step 2 ---
... (same shape)
--- SendMessage step 3 ---
... (same shape)
--- SendMessage step 4 ---
text: "metrics"
state=TASK_STATE_COMPLETED
  artifact[0] name="metrics" parts=1
    part[0].text={"failure":0,"last":"ok(count): {\"squared\":9}","success":3}
```

JSON key ordering is not guaranteed. The load-bearing facts are
`success: 3`, `failure: 0`, and a non-null `last` summary. Exit `0` on
success.

## SDK reference

- Module: `github.com/a2aproject/a2a-go/v2` v2.3.1
- Go: 1.24.4+
- Imports: `a2a`, `a2aclient`, `a2aclient/agentcard`
