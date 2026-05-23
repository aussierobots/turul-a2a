# skill-dispatch-profile-agent

A showcase A2A agent that demonstrates the **skill-invocation dispatcher
profile** end-to-end: how a multi-skill agent declares the extension on
its AgentCard, how clients activate it on the wire, and how the agent
routes inbound messages to a specific skill using a Turul-local
metadata convention.

## What is A2A?

A2A (Agent-to-Agent Protocol) is an open protocol for interoperating
between AI agents over HTTP and gRPC. See
[`a2a-protocol.org`](https://a2a-protocol.org/latest/) for the spec.
This example targets A2A v1.0.

## What this example demonstrates

A2A v1.0 has no normative "send this message to skill X" field on
`Message`. The framework defines an opt-in extension that fills that
gap with a small, well-scoped convention:

- **Declared** by the server in `AgentCapabilities.extensions` with the
  URI `https://turul.dev/a2a/extensions/skill-invocation/v1`.
- **Activated** by the client via the standard A2A
  `A2A-Extensions` HTTP request header.
- **Routed** by placing the target skill id in
  `Message.metadata["a2a.skillId"]` and the structured inputs in
  `Message.metadata["a2a.skillParams"]`.
- **Echoed** by the server on the response — the same
  `A2A-Extensions` header carries every URI that was actually
  honoured.

The agent ships two manifest-backed skills:

| Skill id | Input | Output |
| --- | --- | --- |
| `echo_loud` | `{ "text": "<string>" }` | `{ "shouted": "<UPPERCASE>" }` |
| `reverse`   | `{ "text": "<string>" }` | `{ "reversed": "<reversed>" }` |

## Architecture sketch

```
                       A2A-Extensions: <profile-uri>
client  ───────────────────────────────────────────────────►  agent
        message.metadata["a2a.skillId"] = "echo_loud"
        message.metadata["a2a.skillParams"] = { "text": "hi" }

                            ▼
                    framework router
                    (parse header + validate
                     advertised extensions)
                            ▼
                    AgentExecutor::execute
                    ├─ read message.metadata["a2a.skillId"]
                    ├─ SkillRegistry::handler(id) → SkillHandler
                    └─ SkillHandler::run(params) → JSON output
                            ▼
                    emit Artifact + complete task

client  ◄───────────────────────────────────────────────────  agent
        A2A-Extensions: <profile-uri>     (echoed)
        artifacts[0].parts[0].text = {"shouted":"HI"}
```

## Prerequisites

- Rust 1.85+ (workspace pin).
- `cargo` on `PATH`. No external services. No LLM. No network.

## Run

```
cargo run -p skill-dispatch-profile-agent
```

The agent listens on `http://0.0.0.0:3015`. The discovery surface is
at `http://localhost:3015/.well-known/agent-card.json`.

Override the port with `A2A_PORT=3099`.

## Expected request and response shape

### Request

```
POST /message:send HTTP/1.1
Content-Type: application/json
A2A-Version: 1.0
A2A-Extensions: https://turul.dev/a2a/extensions/skill-invocation/v1

{
  "message": {
    "messageId": "demo-1",
    "role": "ROLE_USER",
    "parts": [ { "text": "hi" } ],
    "metadata": {
      "a2a.skillId": "echo_loud",
      "a2a.skillParams": { "text": "hi" }
    }
  }
}
```

### Response

```
HTTP/1.1 200 OK
Content-Type: application/json
A2A-Extensions: https://turul.dev/a2a/extensions/skill-invocation/v1

{
  "id": "<task-id>",
  "status": { "state": "TASK_STATE_COMPLETED", ... },
  "artifacts": [
    {
      "artifactId": "<uuid>",
      "name": "echo_loud",
      "parts": [ { "text": "{\"shouted\":\"HI\"}" } ]
    }
  ]
}
```

The artifact's text part is a JSON object that satisfies the skill's
`outputSchema` (see `skills/<id>/SKILL.md`).

## How the profile activates

1. The server publishes the profile URI in
   `AgentCard.capabilities.extensions`. Clients reading
   `/.well-known/agent-card.json` see the URI and know the convention
   is supported.
2. The client sends the `A2A-Extensions` header on each request that
   wants the routing behaviour. Multiple URIs are comma-separated per
   spec.
3. The framework router validates the activation set against the
   advertised extensions. Any extension advertised as `required = true`
   that the client did not activate is rejected with
   `UnsupportedOperationError`.
4. The server echoes the intersection of advertised and activated URIs
   in the response `A2A-Extensions` header so the client can confirm
   what was honoured.
5. The agent executor reads `Message.metadata["a2a.skillId"]` and
   dispatches via the `SkillRegistry`. Missing metadata produces a
   `Failed` task with an explanatory message.

## Per-client usage

Future cross-language smoke clients will live under
`examples/interop-clients/skill-dispatch-profile/<lang>/`. The
canonical wire shape for each language is below.

### Python (`a2a-sdk`)

```python
import httpx

PROFILE = "https://turul.dev/a2a/extensions/skill-invocation/v1"

r = httpx.post(
    "http://localhost:3015/message:send",
    headers={
        "content-type": "application/json",
        "a2a-version": "1.0",
        "a2a-extensions": PROFILE,
    },
    json={
        "message": {
            "messageId": "py-1",
            "role": "ROLE_USER",
            "parts": [{"text": "hi"}],
            "metadata": {
                "a2a.skillId": "echo_loud",
                "a2a.skillParams": {"text": "hi"},
            },
        }
    },
)
print(r.headers.get("a2a-extensions"))
print(r.json()["artifacts"][0]["parts"][0]["text"])
```

### Go (`a2a-go`)

```go
body := map[string]any{
    "message": map[string]any{
        "messageId": "go-1",
        "role":      "ROLE_USER",
        "parts":     []any{map[string]any{"text": "hi"}},
        "metadata": map[string]any{
            "a2a.skillId":     "reverse",
            "a2a.skillParams": map[string]any{"text": "abc"},
        },
    },
}
buf, _ := json.Marshal(body)
req, _ := http.NewRequest("POST",
    "http://localhost:3015/message:send", bytes.NewReader(buf))
req.Header.Set("Content-Type", "application/json")
req.Header.Set("A2A-Version", "1.0")
req.Header.Set("A2A-Extensions",
    "https://turul.dev/a2a/extensions/skill-invocation/v1")
resp, _ := http.DefaultClient.Do(req)
fmt.Println(resp.Header.Get("A2A-Extensions"))
```

### Rust (`turul-a2a-client`)

```rust
use serde_json::json;
use turul_a2a_client::prelude::*;

let mut client = A2aClient::connect("http://localhost:3015").await?;
let mut msg = MessageBuilder::new()
    .role_user()
    .add_text("hi")
    .build()?;
// Inject the two reserved metadata keys via the proto layer.
let mut inner = msg.into_proto();
inner.metadata = Some(turul_a2a_types::pbjson::json_object_to_struct(
    [
        ("a2a.skillId".into(), json!("echo_loud")),
        ("a2a.skillParams".into(), json!({"text": "hi"})),
    ]
    .into_iter()
    .collect(),
));
let msg = turul_a2a_types::Message::try_from(inner)?;

// `A2aClient` does not currently expose a typed setter for the
// activation header; the cross-language smoke probe sets it at the
// HTTP layer (see Python/Go above) until the client gains a helper.
```

> **Patterns/framework gap:** `turul-a2a-client` currently has no
> first-class helper for setting `Message.metadata` or the
> `A2A-Extensions` request header. Adopters either drop down to a
> raw HTTP client (recommended for the activation header) or convert
> their `Message` through `pbjson::json_object_to_struct`. Flagged
> for follow-up.

## Failure modes

| Trigger | Outcome |
| --- | --- |
| Activation header missing and extension is `required = false` (default in this example) | Request reaches the executor; if `a2a.skillId` is absent the executor returns a `Failed` task. |
| Activation header missing and extension is `required = true` | Transport returns `UnsupportedOperationError` (HTTP 400 / JSON-RPC `-32004`). |
| Activation header set but `a2a.skillId` metadata absent | Executor returns a `Failed` task citing the missing key. |
| `a2a.skillId` references an unregistered skill | Executor returns `InvalidParamsError`. |
| `a2a.skillParams` violates the skill's `inputSchema` | `SkillHandler` returns `InvalidRequest` → `InvalidParamsError`. |

## Layout

```
examples/skill-dispatch-profile-agent/
├── Cargo.toml
├── README.md
├── src/main.rs               # executor + dispatcher
├── skills/
│   ├── echo_loud/SKILL.md    # uppercases input
│   └── reverse/SKILL.md      # reverses input
└── tests/
    └── smoke.rs              # offline end-to-end smoke
```
