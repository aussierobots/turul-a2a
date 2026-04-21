# streaming-agent

Worked example of an A2A executor that emits chunked artifacts over SSE using
`EventSink::emit_artifact(append, last_chunk)` — the contract from ADR-006
(transport-level `last_chunk`) and ADR-010 (executor `EventSink`).

The executor tokenises a canned response and emits each word as an appended
chunk to a single artifact. Watching `curl --no-buffer` against
`/message:stream` gives you the "tokens arriving in real time" experience
that A2A streaming was designed for.

## Run

```bash
cargo run -p streaming-agent
# listens on http://0.0.0.0:3002
```

## Try it

```bash
# Streaming — --no-buffer is required, otherwise curl accumulates
# the full response before printing anything.
curl --no-buffer http://localhost:3002/message:stream \
  -H 'Content-Type: application/json' \
  -H 'a2a-version: 1.0' \
  -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"say something"}]}}'

# Agent card advertises streaming capability
curl -s http://localhost:3002/.well-known/agent-card.json | jq .capabilities.streaming
```

## What it shows

- `ctx.events.emit_artifact(artifact, append=true, last_chunk=<bool>)` used in
  a loop — one call per token, `last_chunk=true` only on the final iteration.
- `append=true` + reusing a single artifact id (`stream-1`) means all chunks
  merge into one growing artifact, mirroring how an LLM streams tokens into a
  single response.
- `ctx.cancellation.is_cancelled()` checked each tick so `POST
  /tasks/{id}:cancel` from another client trips the loop and emits
  `ctx.events.cancelled(...)` — real cooperative cancel.
- `ctx.events.complete(None)` at the end commits the terminal status update
  the client is waiting for before the stream closes.
- `AgentCardBuilder::streaming(true)` toggles the capability bit on the
  public agent card so discovery clients know the agent supports it.

## Expected SSE wire sequence

Watching the `/message:stream` response, you'll see (order is guaranteed on
monotonic SSE ids; intermediate StatusUpdates may appear):

| # | Event                 | Notes                                             |
| - | --------------------- | ------------------------------------------------- |
| 1 | `statusUpdate`        | non-terminal (Submitted/Working)                  |
| 2 | `artifactUpdate`      | `append=true`, `lastChunk=false`, first token     |
| … | `artifactUpdate` × N  | each adds one word                                |
| N | `artifactUpdate`      | `append=true`, **`lastChunk=true`** — final token |
| — | `statusUpdate`        | `state=TASK_STATE_COMPLETED` — stream closes      |

The chunked-artifact invariant (final chunk flagged, one-artifact-one-id
append-merge) is the single thing the example exists to teach.

## Smoke test

```bash
cargo test -p streaming-agent
```

The test in `tests/smoke.rs` spawns the server on a random port, drives it
through `turul-a2a-client::send_streaming_message`, and asserts the chunked
contract — the terminal Completed status, reassembled canned text, monotonic
SSE ids. It tolerates intermediate non-terminal StatusUpdates (their exact
placement isn't pinned by the spec). The test doubles as the
`turul-a2a-client` streaming example, which is otherwise undocumented.

## Runtime scope

Server-runtime only (plain Axum). This is a **scope choice**, not a
framework limitation — the Lambda adapter supports buffered-streaming
`/message:stream` (`crates/turul-a2a-aws-lambda/src/lib.rs:6-8`). A Lambda
variant would demonstrate buffered SSE rather than live wire-streaming;
not in scope for this example.

## See also

- `crates/turul-a2a/tests/send_mode_tests.rs::SinkDrivenCompleter` — the
  minimal executor pattern this example elaborates on.
- `crates/turul-a2a/tests/sse_tests.rs` — HTTP-level SSE wire-format
  assertions.
- ADR-006 (`last_chunk` as transport metadata), ADR-010 (executor
  `EventSink`).
