# remote-delegate-agent

A real A2A server whose `AgentExecutor` **forwards every inbound
message to a configured upstream A2A agent** and re-emits its
artifacts as its own. The pattern lights up gateway agents,
auth-gating proxies, region-fanout precursors, and A2A-shape mesh
ingress — anywhere you want one A2A endpoint to stand in for another.

## What this example demonstrates

- An `AgentExecutor` impl can own a `turul_a2a_client::A2aClient` and
  call out to another A2A agent inside `execute()`. The delegate is
  itself an ordinary A2A server: it has an `AgentCard`, accepts
  `/message:send` calls, and runs the same wire stack as every other
  showcase agent.
- The two task lifecycles stay independent. The delegate owns its
  *local* task; the upstream task is internal and its ID is not
  exposed to the original caller.
- Cross-agent composition does not require new framework primitives —
  the existing `A2aClient` + `AgentExecutor` traits are sufficient.

## Run

In one shell, bring up the upstream the delegate will forward to.
The default config pairs with `skill-manifest-ollama-agent` in offline
mode out of the box:

```bash
cargo run -p skill-manifest-ollama-agent
# listening on :3010
```

In another shell, the delegate:

```bash
cargo run -p remote-delegate-agent
# listening on :3016, forwarding to http://localhost:3010
```

Send a message through the delegate. The artifact it returns came
from the upstream:

```bash
curl -X POST http://localhost:3016/message:send \
  -H 'Content-Type: application/json' \
  -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"{\"user\":{\"name\":\"Ada\"},\"style\":\"formal\"}"}]}}'
```

The response carries a `task.artifacts[0]` whose body is the upstream
agent's greeting — re-wrapped with a new local artifact id.

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `REMOTE_AGENT_URL` | `http://localhost:3010` | Base URL of the upstream A2A agent. |
| `REMOTE_AGENT_BEARER` | (unset) | Bearer token the delegate presents to the upstream. The delegate's own credential — **not** the caller's. |
| `REMOTE_TIMEOUT_SECS` | `30` | Per-call HTTP deadline. On timeout the delegate fails the local task. |
| `A2A_PORT` | `3016` | Local bind port. |

## Contract — what the delegate guarantees

This is the spec the example commits to. The implementation,
test suite (`tests/smoke.rs` + the unit tests in `src/main.rs`),
and this README move together.

### 1. Discovery / cache

- At startup the delegate calls `GET {REMOTE_AGENT_URL}/.well-known/agent-card.json`
  once and caches the result.
- If discovery fails, the delegate **fails fast** — the binary exits
  with the discovery error. Lazy-init at first request is deliberately
  rejected so an unreachable upstream surfaces at deploy time, not
  request time.
- No periodic refresh. Restart the delegate to pick up upstream
  changes.

### 2. Request forwarding

For each inbound `Message` the delegate constructs an outbound proto
message:

- `messageId` — **regenerated** locally (fresh UUID v7). The upstream
  never sees the caller's message id.
- `contextId` / `taskId` — **cleared**. The upstream call is a fresh
  conversation from its perspective. Trade-off: the upstream cannot
  correlate the delegate's tasks across calls. A future revision
  could expose passthrough of these fields under a flag.
- `role`, `parts` — copied verbatim.
- `metadata` — forwarded intact, so the skill-dispatch profile
  (`Message.metadata["a2a.skillId"]` and `["a2a.skillParams"]`) keeps
  working through the delegate. Same for any other extension that
  parks data on `Message.metadata`.
- `extensions` — forwarded intact (the per-message extension URI
  list).
- `referenceTaskIds` — forwarded intact.

### 3. Task / status / artifact mapping

- Successful upstream `SendResponse::Task` → the delegate walks every
  upstream artifact and re-emits it via `ctx.events.emit_artifact(...)`
  as a fresh local artifact (new id; name preserved). After the last
  artifact, the local task is `task.complete()`.
- Upstream `SendResponse::Message` → wrapped into a single local
  artifact named `upstream-message` with the concatenated text parts
  as its body. Then `task.complete()`.
- Future `SendResponse` variants (`SendResponse` is `#[non_exhaustive]`)
  → local `A2aError::Internal` so the operator notices rather than
  silently dropping the response.
- Upstream **intermediate** `Working` statuses are not yet propagated.
  The delegate's local task transitions Submitted → Working → Completed
  in one shot. Per-step propagation depends on the streaming work in
  §7.

### 4. Error mapping

Every `A2aClientError` becomes a local `A2aError`. `google.rpc.ErrorInfo`
reason drives classification; HTTP status is the secondary signal.

| Upstream client error | Local `A2aError` |
|---|---|
| `A2aError { reason: "TaskNotFoundError" }` (or HTTP 404) | `TaskNotFound { task_id: "upstream:<msg>" }` |
| `A2aError { reason: "UnsupportedOperationError" }` | `UnsupportedOperation { message }` |
| `A2aError { reason: "ContentTypeNotSupportedError" }` (or HTTP 415) | `ContentTypeNotSupported { content_type }` |
| `A2aError { reason: "TaskNotCancelableError" }` (or HTTP 409) | `TaskNotCancelable { task_id: "upstream:<msg>" }` |
| `A2aError { status: 400, reason: <other> }` | `InvalidRequest { message }` |
| `Http { status, message }` (non-A2A, no ErrorInfo) | `Internal("upstream non-A2A HTTP <s>: <m>")` |
| `Request(e)` where `e.is_timeout()` | `Internal("upstream timed out (transport layer): …")` |
| `Request(e)` (other transport) | `Internal("upstream unreachable: …")` |
| `Json(e)` | `Internal("upstream response not JSON-parseable: …")` |
| `Conversion(msg)` | `Internal("upstream type conversion failed: …")` |
| any other | `Internal("upstream error: …")` |

### 5. Timeout

- A `tokio::time::timeout` wraps every `send_message_proto` call with
  the deadline from `REMOTE_TIMEOUT_SECS` (default 30s).
- On timeout the delegate returns `A2aError::Internal("upstream timed
  out after Ns (upstream=<name>)")`, which the framework then surfaces
  to the caller as the local task's failure.
- No retries, no backoff, no circuit breaking in v1. Adopters who
  need those wrap the executor in their own code.

### 6. Auth / header forwarding

**The delegate does NOT forward caller credentials by default.**

- The `A2aClient` is constructed at boot with the delegate's *own*
  optional `REMOTE_AGENT_BEARER`. That bearer is the delegate's
  identity to the upstream — not the caller's.
- Forwarding the inbound `Authorization` / `X-API-Key` header by
  default would be a confused-deputy footgun (the upstream cannot
  distinguish "the delegate is acting on behalf of caller X" from
  "the delegate is caller X"). Adopters who consciously want
  passthrough auth must add it on top — this example does not
  provide a hook.
- The `A2A-Extensions` header IS effectively forwarded — the
  `Message.extensions` list and any extension metadata on
  `Message.metadata` flow through under §2, so profile activation
  works end-to-end.

### 7. Streaming

**Buffered (`/message:send`) only in v1.** `/message:stream` is *not*
implemented end-to-end:

- The local agent card advertises `streaming: false`.
- The framework's default behavior applies if a client calls
  `/message:stream` on the delegate (single buffered emit-then-close
  shape).
- True SSE passthrough — subscribe to upstream SSE, fan out to local
  subscribers, propagate cancel — is non-trivial and deferred.
- **Trigger to revisit**: an adopter case where the upstream emits
  ≥3 intermediate `Working` events that callers need to observe in
  real time. Until then, buffered is sufficient.

## Tests

- `cargo test -p remote-delegate-agent` — runs unit tests + a real
  end-to-end smoke that spawns *both* `skill-manifest-ollama-agent`
  (the upstream) and `remote-delegate-agent` on fixed test ports,
  sends a message through the delegate, and asserts the upstream's
  offline-stub marker appears in the returned artifact. The smoke
  exercises every guarantee in §2 / §3 / §4 except the deferred
  streaming behavior.
- The four unit tests in `src/main.rs` pin: outbound
  `messageId`/`contextId`/`taskId` handling, `metadata` + `extensions`
  preservation for the skill-dispatch profile, and the §4 error
  mapping table for all classification rules.

## What this example is NOT

- **Not a load balancer.** It speaks to exactly one upstream.
  Multi-upstream fan-out is a separate pattern.
- **Not an auth proxy.** It does not translate caller auth into
  upstream auth.
- **Not a retry layer.** Single attempt; failures surface to the
  caller.
- **Not a stream relay.** Buffered only in v1 (see §7).
- **Not a discovery cache invalidation strategy.** Cards are pinned
  for the process lifetime.

Each of those is a legitimate follow-up if an adopter case forces it.
This example stays minimal so the *delegation pattern itself* is
visible.
