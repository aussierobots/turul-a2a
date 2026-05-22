# Go interop client → `agent-role-planner-router-agent`

A self-contained Go program that calls one specific turul-a2a example
agent and prints its responses.

## What is A2A?

[A2A (Agent-to-Agent)](https://a2a-protocol.org) is an open protocol for
agent-to-agent communication. An A2A server publishes an **AgentCard** at
`/.well-known/agent-card.json`. Clients resolve that card, pick a
supported transport, and exchange `Message`s and `Task`s.

Normative spec: <https://a2a-protocol.org/latest/specification/>.
This client targets **A2A v1.0**.

## Which agent this calls

[`examples/agent-role-planner-router-agent`](../../../agent-role-planner-router-agent/)
— a turul-a2a agent that demonstrates the planner+router role idiom with
two deterministic skills:

- `add`     — sums two numbers.
- `concat`  — joins an array of strings.

The agent's planner inspects inbound text and routes to one of the two.

Default URL: `http://localhost:3012`. Override with `A2A_BASE_URL`.

## What this demonstrates

Two calls in one run:

| # | Text sent             | Routed to | Expected artifact text         |
|---|-----------------------|-----------|--------------------------------|
| 1 | `add 3 5`             | `add`     | `{"result":8}`                 |
| 2 | `concat: foo bar baz` | `concat`  | `{"joined":"foo bar baz"}`     |

## Transport used

JSON-RPC over `POST /jsonrpc`. The Go SDK selects this automatically from
the agent's card.

## Run

Open two terminals.

**Terminal 1 — start the agent:**

```bash
cd /path/to/turul-a2a
cargo run -p agent-role-planner-router-agent
```

Wait for `Planner-Router Agent listening on http://0.0.0.0:3012`.

**Terminal 2 — run the client:**

```bash
cd /path/to/turul-a2a/examples/interop-clients/agent-role-planner-router/go
go mod tidy   # first time only
go run ./cmd/probe
```

## Expected output

```
a2a-go protocol version: 1.0
target: http://localhost:3012
--- AgentCard ---
Name:    Planner-Router Agent
  - JSONRPC @ http://localhost:3012/jsonrpc
--- SendMessage [add] ---
text: "add 3 5"
state=TASK_STATE_COMPLETED
  artifact[0] name="add" parts=1
    part[0].text={"result":8}
--- SendMessage [concat] ---
text: "concat: foo bar baz"
state=TASK_STATE_COMPLETED
  artifact[0] name="concat" parts=1
    part[0].text={"joined":"foo bar baz"}
```

Exit status `0` on success.

## SDK reference

- Module: `github.com/a2aproject/a2a-go/v2` v2.3.1
- Go: 1.24.4+
- Imports: `a2a`, `a2aclient`, `a2aclient/agentcard`
