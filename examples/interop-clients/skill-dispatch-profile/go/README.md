# Go interop client → `skill-dispatch-profile-agent`

A self-contained Go program that activates the **skill-invocation
dispatcher profile** against one specific turul-a2a example agent and
prints the artifact text from two different skills.

## What is A2A?

[A2A (Agent-to-Agent)](https://a2a-protocol.org) is an open protocol for
agent-to-agent communication. An A2A server publishes an **AgentCard**
at `/.well-known/agent-card.json` describing its identity, transports,
and skills. A client resolves that card, picks a supported transport,
then sends `Message`s and receives `Task`s.

Normative spec: <https://a2a-protocol.org/latest/specification/>.
This client targets **A2A v1.0**.

## Which agent this calls

[`examples/skill-dispatch-profile-agent`](../../../skill-dispatch-profile-agent/)
— a turul-a2a agent that registers two manifest-backed skills
(`echo_loud`, `reverse`) and advertises the skill-invocation profile
URI in `AgentCard.capabilities.extensions[]`. The agent inspects
`Message.metadata` for the reserved dispatch keys, runs the matching
skill, and emits a single `Artifact` with the JSON result.

Default URL: `http://localhost:3015`. Override with `A2A_BASE_URL`.

## The skill-invocation dispatcher profile (what this demonstrates)

To call a specific skill by id on a multi-skill agent, the client
must do **two** things on every request:

1. **Send the `A2A-Extensions` HTTP header** with the profile URI:

   ```
   A2A-Extensions: https://turul.dev/a2a/extensions/skill-invocation/v1
   ```

   Activates the profile for that request. A compliant agent echoes
   the same header back on its response so the caller can verify
   negotiation succeeded.

2. **Stamp `Message.metadata`** with two reserved keys:

   | Key               | Type        | Purpose                                  |
   | ----------------- | ----------- | ---------------------------------------- |
   | `a2a.skillId`     | string      | Target `AgentSkill.id` to dispatch to.   |
   | `a2a.skillParams` | JSON object | Input for the skill's `inputSchema`.     |

   Missing `a2a.skillId` → the agent fails the task with an
   explanatory message.

## How the Go SDK exposes those two hooks

This client uses [a2a-go v2 v2.3.1](https://pkg.go.dev/github.com/a2aproject/a2a-go/v2).

**`A2A-Extensions` header** — injected via `ServiceParams`:

```go
ctx = a2aclient.AttachServiceParams(ctx, a2aclient.ServiceParams{
    "A2A-Extensions": {"https://turul.dev/a2a/extensions/skill-invocation/v1"},
})
```

The JSON-RPC transport serialises `ServiceParams` as HTTP request
headers. All subsequent calls made with this `ctx` carry the header.

**`Message.Metadata`** — the SDK models it as `map[string]any` and
serialises it under the wire-level `metadata` key:

```go
msg := a2a.NewMessage(a2a.MessageRoleUser, a2a.NewTextPart("dispatch:echo_loud"))
msg.Metadata = map[string]any{
    "a2a.skillId":     "echo_loud",
    "a2a.skillParams": map[string]any{"text": "hello"},
}
```

**Response-header echo** — the SDK does not surface response
headers on its public API, so the probe wraps `http.DefaultTransport`
with a small `http.RoundTripper` that records `A2A-Extensions` from
every response. Plug it in via `a2aclient.WithJSONRPCTransport(httpClient)`.

## Transport used

JSON-RPC over `POST /jsonrpc`. The Go SDK picks JSON-RPC automatically
because that is the only binding in the agent's card.

## Run

Open two terminals.

**Terminal 1 — start the agent:**

```bash
cd /path/to/turul-a2a
cargo run -p skill-dispatch-profile-agent
```

Wait for the line
`Skill Dispatch Profile Agent listening on http://0.0.0.0:3015`.

**Terminal 2 — run the client:**

```bash
cd /path/to/turul-a2a/examples/interop-clients/skill-dispatch-profile/go
go mod tidy   # first time only
go run ./cmd/probe
```

## Expected output

```
a2a-go protocol version: 1.0
target: http://localhost:3015
--- AgentCard ---
Name:    Skill Dispatch Profile Agent
Version: 0.1.0
  Advertised extensions:
    - https://turul.dev/a2a/extensions/skill-invocation/v1 (required=false)
--- SendMessage request (skill=echo_loud) ---
metadata: {a2a.skillId="echo_loud", a2a.skillParams=map[text:hello]}
--- SendMessage response (skill=echo_loud) ---
kind=Task id=<uuid> state=TASK_STATE_COMPLETED
  artifact[0] id=<uuid> name="echo_loud" parts=1
    part[0].text={"shouted":"HELLO"}
response A2A-Extensions echo: [https://turul.dev/a2a/extensions/skill-invocation/v1]
--- SendMessage request (skill=reverse) ---
metadata: {a2a.skillId="reverse", a2a.skillParams=map[text:abc]}
--- SendMessage response (skill=reverse) ---
kind=Task id=<uuid> state=TASK_STATE_COMPLETED
  artifact[0] id=<uuid> name="reverse" parts=1
    part[0].text={"reversed":"cba"}
response A2A-Extensions echo: [https://turul.dev/a2a/extensions/skill-invocation/v1]
```

UUIDs vary per run. Exit status is `0` on success.

## SDK reference

- Module: `github.com/a2aproject/a2a-go/v2` v2.3.1
- Go: 1.24.4+
- Imports: `a2a`, `a2aclient`, `a2aclient/agentcard`
