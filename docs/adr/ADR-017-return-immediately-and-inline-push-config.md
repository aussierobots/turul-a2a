# ADR-017: `return_immediately` Lambda Gate, Inline `task_push_notification_config`, and `history_length` on SendMessage

- **Status:** Accepted
- **Date:** 2026-04-24

## Context

Three silent bugs exist in `core_send_message` (`crates/turul-a2a/src/router.rs:762`), which is shared by all three transports:

- HTTP `POST /message:send` — `router.rs` route handler.
- JSON-RPC `SendMessage` — `jsonrpc.rs:522-533` serializes params and calls `core_send_message` directly.
- **gRPC `SendMessage`** — `grpc/service.rs:99-118` serializes the proto request back to JSON and calls `core_send_message` directly (comment at line 106 explicitly notes "including `configuration.returnImmediately`"). gRPC is feature-gated (`--features grpc`) and is out of scope for the Lambda adapter per ADR-014 §2.6.

A single decision in `core_send_message` therefore governs behavior on all three transports.

**Bug 1 — `return_immediately` on Lambda silently orphans the executor.** `SendMessageConfiguration.return_immediately` (proto `a2a.proto:155-160`) is read and branched at `router.rs:781-785` / `router.rs:931-949`. The non-blocking path drops `yielded_rx`, spawns a background executor via `spawn_tracked_executor` (`server/spawn.rs:146`, `:167`), and returns 200. On AWS Lambda, the execution environment may be frozen immediately after the HTTP response is flushed; any `tokio::spawn` continuation is opportunistic only (ADR-013 §4.4). A caller that relies on `return_immediately=true` has no correctness guarantee on Lambda. Downstream report: tasks stuck in `Working` indefinitely, 0 push callbacks, 21+ orphaned tasks observed in production (v0.1.12).

**Bug 2 — inline `task_push_notification_config` is silently dropped.** `SendMessageConfiguration.task_push_notification_config` (proto `a2a.proto:147-149`) carries a push config to be registered with the newly-created task. `core_send_message` reads only `return_immediately` from `request.configuration`; the inline push config is never consulted. A caller gets no error and no registration, and must discover the omission by listing `/tasks/{id}/pushNotificationConfigs` and observing an empty result.

**Bug 3 — inline `history_length` is not honored on SendMessage responses.** `SendMessageConfiguration.history_length` (proto `a2a.proto:154`) controls response message-history truncation. The `get_task` path already honors this flag end-to-end via the `trim_task` helper in each storage backend (see `storage/postgres.rs:212-216` reference implementation, invoked at `router.rs:316` from `core_get_task`). `core_send_message` does not read the field and passes `None` for `history_length` on the return-immediately re-read at `router.rs:938-944`; blocking send returns the task straight from `yielded_rx` without truncation. This is a wire-level visibility gap — callers who expect the field to bound response size on SendMessage silently receive the full history.

**Explicitly deferred — `accepted_output_modes`.** `SendMessageConfiguration.accepted_output_modes` (proto `a2a.proto:146`) is advisory output-format guidance for the agent. Threading it into `ExecutionContext` is additive and safe, but provides value only if executors explicitly consult it — that is a contract-level design question (what is the executor's obligation? what if it ignores the hint?) separate from the three bugs above. Deferred to a future ADR, reassessed if an adopter reports a concrete interoperability failure.

## Decision

### Bug 1 — `RuntimeConfig` capability flag with Lambda gate

Add a boolean field `supports_return_immediately: bool` to `RuntimeConfig` (`crates/turul-a2a/src/server/mod.rs:41`), defaulting to `true`. `LambdaA2aServerBuilder::build()` sets this field to `false` on the `RuntimeConfig` it constructs before passing it to `AppState`. `core_send_message` checks `state.runtime_config.supports_return_immediately` **before any storage write**; if the flag is `false` and the caller requested `return_immediately=true`, it returns `A2aError::UnsupportedOperation` with a descriptive message and emits `tracing::warn!` at the reject site. The error maps to HTTP 400 / JSON-RPC -32004 per ADR-004, matching the existing `UnsupportedOperationError` surface used for terminal `:subscribe` on Lambda (`crates/turul-a2a-aws-lambda/src/lib.rs:9-14`).

Chosen over a Lambda-adapter pre-filter because `AppState.runtime_config` is already threaded into `core_send_message`; adding a field costs one `bool` read on the hot path. A pre-filter would require parsing the request body twice and cannot be unit-tested without the Lambda crate.

### Bug 2 — Inline push config registration with tight failure semantics

The registration is inserted into `core_send_message` with this ordering, explicitly addressing the partial-persistence risk:

1. **Parse and validate inline config up front** — before any storage write. If `request.configuration.task_push_notification_config` is `Some`, parse its `url` field via `url::Url::parse` (ADR-011 §R1). A malformed URL returns HTTP 400 / JSON-RPC -32602 / gRPC `INVALID_ARGUMENT` **before** `create_task_with_events` is called. No task row is persisted on this path.

2. **Capability + `return_immediately` checks** — Bug 1 gate.

3. **Task creation** — unchanged. `create_task_with_events` writes Submitted then Working.

4. **Push config registration** — `state.push_storage.create_config(tenant, config)` with `config.task_id` set to the newly-created task id. This happens **after** task creation but **before** `spawn_tracked_executor`. `verify_task_ownership` is skipped because `core_send_message` just created the task under `(tenant, owner)`.

5. **Compensating FAILED transition on registration failure.** If step 4 fails (storage error, quota, backend unavailable), the handler follows the established framework pattern at `router.rs:1034-1054`: it builds an agent `Message` with a single `Part::text("inline push notification config registration failed: <error>")`, constructs `TaskStatus::new(TaskState::Failed).with_message(reason_msg)`, emits `update_task_status_with_events` to transition the task to `FAILED`, and returns the original push-storage error to the caller. The executor is never spawned. `TaskStatus` has no `reason` field (`crates/turul-a2a-types/src/task.rs:81-99`); the reason text lives inside the terminal status message, which tests can assert on. This leaves storage consistent: a task exists, but in a terminal `FAILED` state with a discoverable reason message; there is no orphaned `Working` task with no callback.

6. **Executor spawn** — unchanged. Only reached after task + config both exist.

This is pragmatic, not strictly transactional. True atomicity would require a new `A2aAtomicStore::create_task_with_push_config` method implemented across four backends (in-memory, SQLite, PostgreSQL, DynamoDB); that is a larger surface-trait change deferred to a future ADR. The chosen ordering with compensating failure guarantees no orphaned `Working` task and is testable from integration tests.

### Bug 3 — Honor `history_length` on SendMessage responses

The proto field is `optional int32 history_length` (`proto/a2a.proto:150-154`) with a three-valued semantic the storage layer already implements faithfully in `trim_task` (`storage/postgres.rs:212-222`):

- **Unset (`None`)** — the client does not impose a limit; return the full history.
- **`Some(0)`** — the client requests no messages; `trim_task` calls `history.clear()`.
- **`Some(n)` where `n > 0`** — return the last `n` messages.

`core_send_message` preserves this tri-state exactly. Extraction is:

```rust
let history_length: Option<i32> = request
    .configuration
    .as_ref()
    .and_then(|c| c.history_length);
```

The value is threaded into `get_task(tenant, task_id, owner, history_length)` on both send-response paths. The return-immediately re-read (`router.rs:938-944`) and the blocking response (currently returning the task straight from `yielded_rx`) both re-read from storage so `trim_task` is invoked consistently. The extra round-trip on the blocking path is negligible — the caller has already waited for the executor; one storage read is noise. No new storage-trait surface required; the plumbing is reused verbatim from `GetTask`.

Collapsing `Some(0)` to `None` would be a wire-contract bug: callers explicitly requesting an empty history would silently receive the full history. The test matrix covers all three cases (`unset`, `0`, `n>0`) to prevent regression.

## Scope

In scope:

- `crates/turul-a2a/src/server/mod.rs` — add `supports_return_immediately: bool` to `RuntimeConfig` (default `true`).
- `crates/turul-a2a/src/router.rs` — capability guard; inline push-config registration with compensating FAILED semantics; honor `history_length` on both send-response paths.
- `crates/turul-a2a-aws-lambda/src/lib.rs` — set `supports_return_immediately = false`.
- New and adapted tests covering HTTP, JSON-RPC, and gRPC (Lambda coverage applies to HTTP + JSON-RPC only per ADR-014 §2.6).

Explicitly deferred:

- `accepted_output_modes` — executor-contract design question.
- Durable Lambda continuation (Step Functions / SQS / EventBridge) that would allow `return_immediately=true` on Lambda. Separate ADR when demand materialises.
- Transactional `create_task_with_push_config` on `A2aAtomicStore`.

## Compliance

- Proto `a2a.proto:155-160`: `return_immediately` is a server capability, not a MUST-accept. The MUST clause applies only to `false`. `UnsupportedOperationError` is the correct reason code — same surface the Lambda adapter already uses for terminal `:subscribe`.
- Proto `a2a.proto:147-149`: `task_push_notification_config` SHOULD be honored when present on `SendMessage`. Silent drop is a latent interoperability bug.
- Proto `a2a.proto:154`: `history_length` is a response-shape hint; the `trim_task` helper is already the normative implementation on `GetTask`.
- ADR-004: `UnsupportedOperation` maps to HTTP 400 / JSON-RPC -32004 / gRPC `INVALID_ARGUMENT` with `google.rpc.ErrorInfo reason=UNSUPPORTED_OPERATION`.
- ADR-008: Lambda is request/response only; persistent background executors are out of scope.
- ADR-010 §4.2: non-blocking send is a runtime capability the server MAY advertise.
- ADR-011 §R1: push-config URL MUST be validated at CRUD time (this ADR tightens it to "before any task persists").
- ADR-013 §4.4: `tokio::spawn` post-return on Lambda is opportunistic; the Lambda gate aligns the framework with this reality.
- ADR-014 §2.6: gRPC on Lambda is out of scope; Lambda coverage in this ADR applies to HTTP + JSON-RPC only.

## Test obligations

**New — Lambda gate (HTTP + JSON-RPC only; gRPC on Lambda is out of scope):**

- `turul_a2a_aws_lambda::tests::send_message_return_immediately_rejected_http` — HTTP `POST /message:send` through a `LambdaA2aHandler`-backed router with `configuration.returnImmediately=true` returns HTTP 400 with `ErrorInfo.reason=UNSUPPORTED_OPERATION`.
- `turul_a2a_aws_lambda::tests::send_message_return_immediately_rejected_jsonrpc` — JSON-RPC `POST /jsonrpc` with method `SendMessage`, same assertion.

**New — inline push config (all three transports):**

- `turul_a2a::tests::spec_compliance::send_message_inline_push_config_http` — HTTP send with `configuration.taskPushNotificationConfig.url` set; after send, `GET /tasks/{id}/pushNotificationConfigs` returns the registered config.
- `turul_a2a::tests::spec_compliance::send_message_inline_push_config_jsonrpc` — JSON-RPC parity.
- `turul_a2a::tests::spec_compliance::send_message_inline_push_config_grpc` — gated on `#[cfg(feature = "grpc")]`; gRPC `SendMessage` registers inline config; confirmed via a subsequent gRPC `ListTaskPushNotificationConfig`.
- `turul_a2a::tests::spec_compliance::send_message_inline_push_config_invalid_url` — HTTP send with `task_push_notification_config.url = "not a url"`. Asserts:
  1. Response is HTTP 400.
  2. **No task row is persisted** (list tasks for the tenant returns 0 new rows; direct storage inspection confirms).
  This is the P1 failure-semantics anchor.
- `turul_a2a::tests::spec_compliance::send_message_inline_push_config_storage_failure_compensates` — injects a push-storage stub that rejects `create_config`; sends a valid inline config. Asserts:
  1. Caller sees the registration error.
  2. Task exists in storage in `TASK_STATE_FAILED`.
  3. The task's terminal `status.message` is an agent `Message` whose `Part::text` payload contains the substring `inline push notification config registration failed`. (`TaskStatus` has no `reason` field; the reason rides on the terminal status message per the pattern at `router.rs:1034-1054`.)
  4. Executor was never spawned (in-flight registry count unchanged).
  This is the P1 compensation anchor.

**New — `history_length` (three-valued per proto `a2a.proto:150-154`):**

- `turul_a2a::tests::spec_compliance::send_message_respects_history_length_return_immediately` — continuation whose task has ≥3 historical messages; `configuration.historyLength = 1`. Asserts response task's `history.len() == 1`.
- `turul_a2a::tests::spec_compliance::send_message_respects_history_length_blocking` — same assertion on the blocking send path.
- `turul_a2a::tests::spec_compliance::send_message_history_length_zero_empties_response` — continuation whose task has ≥2 historical messages; `configuration.historyLength = 0`. Asserts response task's `history.len() == 0`. This is the wire-contract anchor that prevents collapsing `Some(0)` to `None`.
- `turul_a2a::tests::spec_compliance::send_message_history_length_unset_is_unbounded` — continuation whose task has ≥3 historical messages; `configuration` omits `historyLength` entirely. Asserts response task's `history` is returned without truncation.

**Adapted (no modification required):**

- `turul_a2a::tests::send_mode_tests::non_blocking_send_return_immediately_returns_before_terminal` (`tests/send_mode_tests.rs:651`) — continues to pass because the flag defaults to `true` on the binary server. Confirms the non-Lambda path is unchanged.

## Migration

Adopters on Lambda:

> `return_immediately=true` on `SendMessage` now returns `UnsupportedOperationError` (HTTP 400 / JSON-RPC -32004) when the server is built with `LambdaA2aServerBuilder`. Previously the flag was silently accepted but the background executor had no guarantee of completing in the Lambda execution environment (ADR-008, ADR-013 §4.4). If your client sets `returnImmediately: true`, remove the flag for Lambda deployments or submit with `returnImmediately: false` for blocking send (respecting the API Gateway 29s upper bound). No action is required for non-Lambda deployments.
>
> `SendMessageConfiguration.taskPushNotificationConfig` inline push configs supplied on `SendMessage` are now registered with the newly-created task on all runtimes (HTTP, JSON-RPC, gRPC). If the URL is malformed, the send request fails with HTTP 400 before the task is persisted. If push-storage registration fails after the task was created, the task is transitioned to `TASK_STATE_FAILED` with reason `inline_push_config_registration_failed` and the error is returned to the caller. Callers that previously worked around the silent drop with a separate `CreateTaskPushNotificationConfig` round-trip may remove that workaround.
>
> `SendMessageConfiguration.historyLength` now trims response message history on both `return_immediately=true` and blocking send responses, matching the behavior on `GetTask`. The three-valued semantic matches the proto (`a2a.proto:150-154`): **unset** means no limit (return the full history), **`0`** means return no messages (empty `history`), and **`n > 0`** means return the last `n` messages. Callers that relied on receiving the full history regardless of the flag must omit `historyLength` — setting it to `0` now correctly clears the response history per the proto contract.

## Alternatives considered

**Lambda-adapter pre-filter.** Rejected: body double-parse, Lambda-only code path, not unit-testable outside the Lambda crate.

**Silent downgrade to blocking on Lambda.** Rejected: callers asked for async, silently serving sync exposes them to the API Gateway 29s timeout and masks the decision in logs. Hard rejection is the principle-of-least-surprise path.

**Durable continuation via Step Functions / SQS / EventBridge.** Adopters reaching for fire-and-forget-style work on Lambda have two distinct patterns. This ADR ships Pattern A as the intended current path (no framework changes required) and reserves Pattern B for a future ADR.

- **Pattern A — manual workflow invocation inside the skill handler (works today, no framework changes).** The adopter's `AgentExecutor::execute` body calls Step Functions / SQS / EventBridge directly to kick off the long-running work, then returns. From turul-a2a's perspective the executor ran to completion synchronously — `return_immediately = false` (default) is the correct client setting. Semantic: the A2A task is Completed for **"workflow accepted / started"**, not **"workflow finished"**. If the adopter's domain requires the A2A task to track full workflow completion, the workflow itself (Step Function activity, SQS consumer, etc.) must later call back into turul-a2a storage to update task state — that is a domain-integration concern outside the framework contract.

- **Pattern B — framework-managed durable continuation (deferred to a future ADR).** turul-a2a itself would route executor work through a persistent mechanism (Step Functions task token, SQS visibility timeout, EventBridge rule, async self-invoke) and update the A2A task lifecycle automatically. This is the only model in which the framework could honour `return_immediately = true` on Lambda. When it lands, the adopter surface MUST be a capability-taking builder method: `LambdaA2aServerBuilder::with_durable_return_immediately(continuation_handler)` — the builder takes the durable mechanism as its argument and sets the `supports_return_immediately` flag as a side effect, so the capability cannot be claimed without the mechanism being wired. Shorthand: "capability, not intent."

- **Rejected shape — public `supports_return_immediately(bool)` setter on `LambdaA2aServerBuilder`.** Flipping the boolean alone does not create durable continuation; it only silences the guard installed by this ADR. That is a footgun and will not ship. If an adopter reaches the point where they want Pattern B semantics, they must wait for the future ADR's capability-taking builder shape (or contribute it).

- **`RuntimeConfig.supports_return_immediately` is `pub` on a `#[non_exhaustive]` struct.** That is deliberate — it is an escape hatch for tests and advanced internal consumers that construct `AppState` directly (e.g., router-level integration tests). It is NOT an adopter surface; the field's doc comment flags this explicitly. The adopter-facing gate is the builder contract, not the field visibility.

**Strict transactional atomicity for task + inline push config.** New `A2aAtomicStore::create_task_with_push_config` across four backends. Rejected for this ADR: large surface-trait change. The compensating FAILED transition is materially equivalent from the caller's perspective (no orphaned `Working` task) and lands in a point release.

**Register inline push config BEFORE task creation.** Generate `task_id` up front, call `create_config` first, then `create_task_with_events`. Rejected: the failure mode (orphaned push config with no task) is worse than the chosen ordering's failure mode (FAILED task with clear reason). Push configs are keyed on task id; an orphan is harder to observe and clean up than a FAILED task with a reason string.

**Bundle `accepted_output_modes`.** Rejected: requires a contract-level decision on executor obligations. Separate ADR.

**Bundle `history_length` or defer it.** `history_length` is bundled because the plumbing already exists (`trim_task` on every backend, `core_get_task` invocation pattern) and the fix is three lines in `core_send_message` reusing `get_task(... , history_length)`. Rejected deferral on the grounds that omitting a trivially-available fix would recreate the "silent drop" pattern this ADR is closing.

**Collapse `Some(0)` to `None` for `history_length`.** An earlier draft of this ADR proposed `if c.history_length > 0 { Some(c.history_length) } else { None }` as the extraction rule. Rejected: the proto (`a2a.proto:150-154`) explicitly distinguishes unset (no limit) from `Some(0)` (empty), and the storage layer's `trim_task` handles them differently (`storage/postgres.rs:212-222`). Collapsing them is a wire-contract regression. The three-case test matrix (`unset`, `0`, `n>0`) is non-negotiable for this ADR.

**Carry the FAILED reason on a dedicated `reason` field.** An earlier draft referred to a `reason` property on `TaskStatus`. Rejected: no such field exists. `TaskStatus` is `{state, message, timestamp}` (`crates/turul-a2a-types/src/task.rs:81-99`). The framework-wide pattern at `router.rs:1034-1054` puts the reason in an agent `Message` attached via `with_message`; this ADR follows that pattern and asserts on the message text in tests.
