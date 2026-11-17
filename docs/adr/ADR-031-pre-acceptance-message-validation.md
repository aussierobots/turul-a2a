# ADR-031: Pre-Acceptance Message Validation

- **Status:** Accepted
- **Date:** 2026-11-17
- **Related:** ADR-012 (executor lifecycle), ADR-017 (return-immediately), ADR-018 (durable executor queue)

## Context

An executor can only report a malformed request once the task exists. `SendMessage` and `SendStreamingMessage` resolve the target task, persist it (SUBMITTED → WORKING, or the continuation message), and only then call `AgentExecutor::execute`. If the executor finds a bad input, say an unparseable timestamp in a data part, its only outlets are:

- return `Err(A2aError::InvalidRequest { .. })`, which the framework commits as a FAILED task whose status message is the text `executor error: <message>`;
- commit FAILED itself through its `EventSink`, with whatever parts it builds.

Either way the caller gets an accepted task that then fails, never a refusal. A2A v1.0.1 has a refusal for this: an invalid-params error (HTTP 400, JSON-RPC `-32602`, gRPC `INVALID_ARGUMENT`), which the framework already returns for its own checks (missing `message`, `contextId` mismatch, unsupported media type) before anything is written. An agent cannot add its own checks to that list.

A2A v1.0.1 §3.3.2 makes this a server obligation: "Servers **MUST** validate all input parameters before processing", with the validation error as HTTP `400 Bad Request`, gRPC `INVALID_ARGUMENT`, JSON-RPC `-32602 Invalid params`, and servers **SHOULD** say which parameter failed and why. §3.1.1 does not require a send to create a task. The framework can validate only what it knows (the envelope, media types, follow-up rules); input whose meaning belongs to the agent can only be validated by the agent.

What breaks today: a server whose callers need a typed refusal for an invalid field cannot give one; it creates and fails a task per bad request, which also costs the storage writes (and, on Lambda with the durable queue, an enqueue).

Rejected alternatives:

- **Give `InvalidRequest` an `ErrorInfo` reason.** The A2A error table (§5.4) defines `ErrorInfo` reasons for the A2A-specific errors only. Inventing one under the `a2a-protocol.org` domain makes the framework emit a reason the spec does not define.
- **Have the framework attach a typed data part to its FAILED status.** A convention the spec does not have; clients would depend on a turul-specific part shape. An executor that wants this can already commit it through its `EventSink`.

## Decision

### 1. `AgentExecutor::validate_message`

```rust
async fn validate_message(
    &self,
    _message: &Message,
    _ctx: &ValidationContext<'_>,
) -> Result<(), A2aError> {
    Ok(())
}
```

A provided method with an allow-everything default, so every existing executor compiles and behaves as before.

```rust
#[non_exhaustive]
pub struct ValidationContext<'a> {
    pub owner: &'a str,
    pub tenant: Option<&'a str>,
    pub task_id: &'a str,
    pub context_id: &'a str,
    pub is_continuation: bool,
    pub claims: Option<&'a serde_json::Value>,
    pub accepted_output_modes: &'a [String],
    pub request_metadata: Option<&'a serde_json::Value>,
    pub activated_extensions: &'a [String],
}
```

The fields mirror `ExecutionContext` (same tenant convention, same request inputs), borrowed. `ValidationContext::anonymous(task_id)` builds one for unit-testing a validator without a server.

### 2. Where it runs

In both send paths (`core_send_message`, `open_send_stream`, through the one helper `validate_send` in `router.rs`), so on HTTP+JSON, JSON-RPC and gRPC alike: after the framework's own request checks and `resolve_send_target` (so the target task id, context id and continuation flag are known and the follow-up rules have passed), and **before** any storage write, durable-queue payload check or enqueue.

### 3. Outcome

- `Ok(())` — the send proceeds unchanged.
- `Err(e)` — the send returns `e` as the error response, through the existing error mapping. No task is created, a continuation leaves its task untouched, nothing is enqueued, no executor runs. The intended error is `A2aError::InvalidRequest` (400 / `-32602` / `INVALID_ARGUMENT`); any `A2aError` is passed through as-is.

The framework does not call it anywhere else: the SQS dequeue path runs a job that was validated when it was accepted.

### 4. Contract for implementers

The hook must be cheap and side-effect free: it runs on the request path before acceptance (on Lambda, inside the billed request invocation) and may run for a request that is later refused by a framework check. It must not write storage or start work.

## Consequences

- Agents can refuse invalid input with the spec's invalid-params error instead of creating and failing a task.
- An executor's `Err` returned from `execute` is unchanged: still a FAILED task with the text reason.
- The error carries no `ErrorInfo` (as for every framework-raised `InvalidRequest`). Field-level detail, if wanted, is a separate decision (`google.rpc.BadRequest` field violations are the standard, A2A-neutral shape).

## Required tests

1. A refused new-task `SendMessage` and `SendStreamingMessage` return 400 / `-32602` / `INVALID_ARGUMENT` with the executor's message on HTTP+JSON, JSON-RPC and gRPC; no task exists; `execute` was never called; an accepted message still runs — `message_validation_tests::refused_new_message_creates_nothing_on_http_and_jsonrpc`, `message_validation_tests::refused_message_is_invalid_argument_on_grpc` (`grpc` feature).
2. A refused follow-up leaves its task INPUT_REQUIRED with its history unchanged — `message_validation_tests::refused_follow_up_leaves_the_task_untouched`.
3. With a durable queue and `returnImmediately`: refused, no task, nothing enqueued — `durable_executor_tests::validation_refusal_creates_no_task_and_enqueues_nothing`.
4. The default accepts: every existing suite passes unchanged.
5. Negation: with the call moved after `start_task_turn`, tests 1–3 fail.

## Revision log

- 2026-11-17 — Proposed and accepted. Spec basis A2A v1.0.1 §3.3.2; `ValidationContext` fields aligned with `ExecutionContext`; required tests named.
