# Go interop client → `skill-manifest-ollama-agent`

A self-contained Go program that calls one specific turul-a2a example agent
and prints its response. Intended as a copy-pasteable starting point for
adopters writing Go clients against turul-a2a agents.

## What is A2A?

[A2A (Agent-to-Agent)](https://a2a-protocol.org) is an open protocol for
agent-to-agent communication. An A2A server publishes an **AgentCard** at
`/.well-known/agent-card.json` describing its identity, transports, and
skills. A client resolves that card, picks a supported transport, then
sends `Message`s and receives `Task`s (or streaming events).

Normative spec: <https://a2a-protocol.org/latest/specification/>.
This client targets **A2A v1.0**.

## Which agent this calls

[`examples/skill-manifest-ollama-agent`](../../../skill-manifest-ollama-agent/)
— a turul-a2a agent that exposes a single skill (`greet`) backed by a
SKILL.md manifest. Offline by default; can be wired to a local Ollama
instance via `OLLAMA_BASE_URL`.

Default URL: `http://localhost:3010`. Override with `A2A_BASE_URL`.

## What this demonstrates

1. AgentCard resolution from the well-known URL.
2. Building a transport-agnostic client (`a2aclient.NewFromCard`); the
   SDK selects JSON-RPC because that is what the card advertises.
3. Sending a JSON-shaped text payload that the agent's manifest validates
   against its `inputSchema`.
4. Decoding the returned `Task` and printing its `greeting` artifact.

**Request payload sent:**

```json
{"user":{"name":"Ada"},"style":"formal"}
```

**Expected artifact** (offline mode — `OLLAMA_BASE_URL=""`):

```json
{"greeting":"Good day, Ada! (offline stub)"}
```

## Transport used

JSON-RPC over `POST /jsonrpc`. The Go SDK picks JSON-RPC automatically
because that is the only binding in the agent's card.

## Run

Open two terminals.

**Terminal 1 — start the agent (offline mode):**

```bash
cd /path/to/turul-a2a
OLLAMA_BASE_URL="" RUN_OLLAMA_SMOKE="" cargo run -p skill-manifest-ollama-agent
```

Wait for the line `Skill Manifest Ollama Agent listening on http://0.0.0.0:3010`.

**Terminal 2 — run the client:**

```bash
cd /path/to/turul-a2a/examples/interop-clients/skill-manifest-ollama/go
go mod tidy   # first time only
go run ./cmd/probe
```

## Expected output

```
a2a-go protocol version: 1.0
target: http://localhost:3010
--- AgentCard ---
Name:    Skill Manifest Ollama Agent
Version: 0.1.0
  - JSONRPC @ http://localhost:3010/jsonrpc
--- SendMessage request ---
payload: {"user":{"name":"Ada"},"style":"formal"}
--- SendMessage response ---
kind=Task id=<uuid> state=TASK_STATE_COMPLETED
  artifact[0] id=<uuid> name="greeting" parts=1
    part[0].text={"greeting":"Good day, Ada! (offline stub)"}
```

UUIDs vary per run; exit status is `0` on success.

## SDK reference

- Module: `github.com/a2aproject/a2a-go/v2` v2.3.1
- Go: 1.24.4+
- Imports: `a2a`, `a2aclient`, `a2aclient/agentcard`
