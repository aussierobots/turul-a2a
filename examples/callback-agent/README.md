# callback-agent

Worked example of the A2A push-notification contract (ADR-011): register a
webhook URL and a shared-secret token against a task; when the task
terminates, the framework POSTs the terminal Task to that URL with the token
echoed in the `X-Turul-Push-Token` header for receiver-side authentication.

For demo friendliness, a single `cargo run` spawns **two servers in one
process**:

- the A2A agent on `http://0.0.0.0:3003`
- a webhook receiver on `http://127.0.0.1:3004/webhook`

See [§Splitting for production](#splitting-for-production) below for why
you'd pull them apart in a real deployment.

## Run

```bash
cargo run -p callback-agent
```

The startup banner prints the full three-step curl sequence with the
right ports and token baked in.

## Demo sequence

```bash
# 1. Non-blocking send — returns immediately with a task_id.
TASK_ID=$(curl -s http://localhost:3003/message:send \
  -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"m1","role":"ROLE_USER","parts":[{"text":"hi"}]},"configuration":{"returnImmediately":true}}' \
  | jq -r .task.id)
echo "task_id=$TASK_ID"

# 2. Register webhook + shared secret against that task.
curl -s http://localhost:3003/tasks/$TASK_ID/pushNotificationConfigs \
  -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \
  -d '{"taskId":"'$TASK_ID'","url":"http://127.0.0.1:3004/webhook","token":"demo-secret-change-me"}'

# 3. Wait ~1.5 s — the agent's terminal log line "webhook called"
#    appears in the cargo run terminal. The receiver prints the
#    delivered Task JSON with the token header it observed.
```

## What it shows

- `.storage(...)` + `.with_push_dispatch_enabled(true)` + `.push_delivery_store(...)` — the three-part ADR-013 §4.3 wiring. The server builder rejects inconsistent combinations.
- `.allow_insecure_push_urls(true)` — required to deliver to `http://127.0.0.1`. **Never enable in production.** Default is `false`; real deployments use `https://` only.
- `TaskPushNotificationConfig.token` — shared secret echoed in the
  `X-Turul-Push-Token` header by the framework's `PushDeliveryWorker`
  (`crates/turul-a2a/src/push/delivery.rs:591-593`).
- Receiver-side validation — the `/webhook` handler in `lib.rs` rejects
  calls whose token header doesn't match the one it registered with,
  returning `401`. Without this check, anyone who knows the receiver URL
  could forge callbacks.

## Token header note

The framework emits `X-Turul-Push-Token`, not the A2A spec's suggested
`X-A2A-Notification-Token`. This is a framework implementation choice
flagged in ADR-011 §4. If you integrate with a receiver that expects
the spec header name, you'll need to either adapt the receiver or
patch the worker — this example uses the framework's header verbatim.

## Smoke test

```bash
cargo test -p callback-agent
```

`tests/smoke.rs` runs both servers in-process, does the send +
register + wait flow through `turul-a2a-client`, and asserts:

1. The webhook is called within 5 s of terminal.
2. `X-Turul-Push-Token` header equals the registered token.
3. `X-Turul-Event-Sequence` header is present.
4. The delivered body is a proto-JSON Task with `state: TASK_STATE_COMPLETED` and the matching task id.
5. A direct POST with the **wrong** token gets `401` and is tagged as rejected — proves the validation path runs.

Both server handles are kept and aborted at test end; no leaked
listening sockets.

## Splitting for production

Real deployments separate the webhook receiver from the agent:

- The receiver sits behind your service's ingress with its own TLS
  termination and an allowlist of sender IPs.
- The agent runs wherever its storage + dispatcher live (Lambda,
  Kubernetes, bare metal).
- Drop `.allow_insecure_push_urls(true)` — TLS is mandatory.
- Rotate the shared-secret `token` per config or per task; treat
  leaks like credential exposure.

## See also

- ADR-011 — push notification delivery (claim-based fan-out, SSRF
  allowlist, secret redaction).
- ADR-013 — Lambda push-delivery parity (atomic pending-dispatch marker,
  causal `registered_after_event_sequence` CAS).
- `crates/turul-a2a/src/push/delivery.rs` — `PushDeliveryWorker`
  implementation; the header shape is at lines 569-593.
- `examples/lambda-stream-worker` and `examples/lambda-scheduled-worker`
  for the Lambda-side recovery workers that make push delivery durable
  across container freeze.
