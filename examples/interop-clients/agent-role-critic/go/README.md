# Go interop client → `agent-role-critic-agent`

A self-contained Go program that calls one specific turul-a2a example
agent and prints its responses.

## What is A2A?

[A2A (Agent-to-Agent)](https://a2a-protocol.org) is an open protocol for
agent-to-agent communication. An A2A server publishes an **AgentCard** at
`/.well-known/agent-card.json` describing its identity, transports, and
skills. Clients resolve that card, pick a supported transport, then
exchange `Message`s and receive `Task`s.

Normative spec: <https://a2a-protocol.org/latest/specification/>.
This client targets **A2A v1.0**.

## Which agent this calls

[`examples/agent-role-critic-agent`](../../../agent-role-critic-agent/) —
a turul-a2a agent demonstrating the critic / evaluator role idiom with
two deterministic skills:

- `validate_against_schema` — validate a JSON value against a JSON
  Schema 2020-12 document.
- `check_invariants` — run a deterministic invariant table over a value
  (`non_empty` / `min_length` / `max_length` / `contains`).

The agent dispatches by reading the inbound text as JSON and looking at
the `kind` field.

Default URL: `http://localhost:3013`. Override with `A2A_BASE_URL`.

## What this demonstrates

Two calls in one run:

| # | Payload `kind`           | Expected artifact text                                  |
|---|--------------------------|---------------------------------------------------------|
| 1 | `validate_against_schema`| `{"valid":true,"errors":[]}`                            |
| 2 | `check_invariants`       | `{"verdict":"pass","failures":[]}`                      |

Full payloads sent (one per call):

```json
{"kind":"validate_against_schema","value":42,"schema":{"type":"integer"}}
```

```json
{"kind":"check_invariants","value":"hello world",
 "invariants":[
   {"name":"ne","check":"non_empty"},
   {"name":"has_world","check":"contains","args":{"needle":"world"}}
 ]}
```

## Transport used

JSON-RPC over `POST /jsonrpc`. The Go SDK selects this automatically from
the agent's card.

## Run

Open two terminals.

**Terminal 1 — start the agent:**

```bash
cd /path/to/turul-a2a
cargo run -p agent-role-critic-agent
```

Wait for `Critic Agent listening on http://0.0.0.0:3013`.

**Terminal 2 — run the client:**

```bash
cd /path/to/turul-a2a/examples/interop-clients/agent-role-critic/go
go mod tidy   # first time only
go run ./cmd/probe
```

## Expected output

```
a2a-go protocol version: 1.0
target: http://localhost:3013
--- AgentCard ---
Name:    Critic Agent
  - JSONRPC @ http://localhost:3013/jsonrpc
--- SendMessage [validate_against_schema] ---
payload: {"kind":"validate_against_schema","value":42,"schema":{"type":"integer"}}
state=TASK_STATE_COMPLETED
  artifact[0] name="validate_against_schema" parts=1
    part[0].text={"errors":[],"valid":true}
--- SendMessage [check_invariants] ---
payload: {"kind":"check_invariants","value":"hello world","invariants":[...]}
state=TASK_STATE_COMPLETED
  artifact[0] name="check_invariants" parts=1
    part[0].text={"failures":[],"verdict":"pass"}
```

JSON key ordering is not guaranteed by the agent; what matters is
`valid==true` / `verdict=="pass"`. Exit status `0` on success.

## SDK reference

- Module: `github.com/a2aproject/a2a-go/v2` v2.3.1
- Go: 1.24.4+
- Imports: `a2a`, `a2aclient`, `a2aclient/agentcard`
