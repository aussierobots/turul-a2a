# ADR-018: Lambda durable executor continuation via SQS

- **Status:** Accepted
- **Date:** 2026-04-24

## Context

ADR-017 §Decision Bug 1 ships a hard-reject for `return_immediately=true` on the Lambda adapter because Lambda's execution environment freezes after the HTTP response is flushed, leaving `tokio::spawn`'d executors with no guarantee of completing (ADR-013 §4.4). The guard is correct but closes off a legitimate use case: adopters who want to register a push callback, fire-and-forget, and let their executor run to terminal on a bounded-latency Lambda deployment.

ADR-017 §"Alternatives considered" named the opt-in shape reserved for this work: a capability-taking builder method `LambdaA2aServerBuilder::with_durable_return_immediately(...)`, never a public boolean setter ("capability, not intent"). This ADR makes that shape concrete.

The chosen durable mechanism is **AWS SQS**. Rationale: native AWS integration (IAM-bounded trust, Lambda event source mapping, DLQ, retry, visibility timeout, partial-batch response), at-least-once semantics that compose cleanly with the framework's CAS-guarded terminal commits, and first-class `aws-sdk-sqs` support. Other transports (Kinesis, SNS, self-invoke) are considered in "Alternatives considered" and deferred.

Two patterns remain valid after this ADR:

- **Pattern A (ADR-017) — manual workflow invocation inside the skill handler.** Still works. Still the default. No framework changes needed. The A2A task is Completed for "workflow accepted / started", not "workflow finished" unless the adopter wires their own callback into turul-a2a storage.
- **Pattern B (this ADR) — framework-managed durable continuation via SQS.** The HTTP Lambda invocation creates the task and enqueues a job; an SQS-triggered invocation of the same Lambda function runs the executor and commits the terminal. Push callbacks fire per ADR-011 exactly as on a long-lived host. Adopters opt in via a builder method.

## Decision

### Capability-taking builder shape

Two builder methods on `LambdaA2aServerBuilder`:

```rust
// Generic entry point. Accepts any queue implementation; enables
// unit tests with in-memory fakes and future non-SQS backends.
pub fn with_durable_executor(
    mut self,
    queue: Arc<dyn DurableExecutorQueue>,
) -> Self;

// Ergonomic helper for the SQS common case — constructs
// SqsDurableExecutorQueue and delegates to with_durable_executor.
#[cfg(feature = "sqs")]
pub fn with_sqs_return_immediately(
    self,
    queue_url: impl Into<String>,
    sqs_client: Arc<aws_sdk_sqs::Client>,
) -> Self;
```

Both methods set `runtime_config.supports_return_immediately = true` as an implementation detail; the flag cannot be flipped without supplying the queue. No public boolean setter ships — "capability, not intent" (ADR-017 §"Alternatives considered").

### Queue trait

```rust
/// Public extension point for non-SQS durable backends. External
/// implementers should treat method additions (and signature changes)
/// as semver-sensitive — adding a required method here is a
/// minor-version breaking change for external implementers. First-party
/// impls ship behind the `sqs` feature; other backends may be added by
/// adopters at their own risk.
#[async_trait]
pub trait DurableExecutorQueue: Send + Sync {
    /// Hard payload ceiling for this transport, in bytes. Implementations
    /// MUST return the transport's real limit, not an idealised one
    /// (SQS standard queue: `256 * 1024`; Kinesis: `1 * 1024 * 1024`).
    fn max_payload_bytes(&self) -> usize;

    /// Pre-enqueue size check. Default implementation JSON-encodes the
    /// job and compares the encoded length against `max_payload_bytes()`.
    /// Implementations MAY override to use the native encoding their
    /// `enqueue` would produce (avoids double-serialisation).
    ///
    /// Called by `core_send_message` BEFORE task creation so oversize
    /// payloads never persist a task row.
    fn check_payload_size(
        &self,
        job: &QueuedExecutorJob,
    ) -> Result<usize, QueueError> {
        let encoded = serde_json::to_vec(job).map_err(QueueError::Encode)?;
        let max = self.max_payload_bytes();
        if encoded.len() > max {
            Err(QueueError::PayloadTooLarge { actual: encoded.len(), max })
        } else {
            Ok(encoded.len())
        }
    }

    /// Enqueue a job for asynchronous executor dispatch. Implementations
    /// SHOULD call `check_payload_size` internally as a defence-in-depth
    /// guard; `core_send_message` also calls it upstream so `enqueue`
    /// failures for oversize payloads are unexpected at this point.
    async fn enqueue(&self, job: QueuedExecutorJob) -> Result<(), QueueError>;

    /// Identifier for logs / errors / diagnostics.
    fn kind(&self) -> &'static str;
}
```

Shipped implementations: `SqsDurableExecutorQueue` with `max_payload_bytes() -> 256 * 1024` (behind the `sqs` feature). The trait is `pub` in `turul-a2a-aws-lambda` so advanced adopters can inject fakes for testing or provide alternative transports.

### Payload schema

```rust
#[derive(Serialize, Deserialize)]
pub struct QueuedExecutorJob {
    /// Envelope version for forward-compatible schema evolution.
    /// Current: 1. Unknown versions are rejected at dequeue time.
    pub version: u16,
    pub tenant: String,
    pub owner: String,
    pub task_id: String,
    pub context_id: String,
    /// The incoming `SendMessage.message`, carried through to the
    /// executor's `ExecutionContext`.
    pub message: turul_a2a_proto::Message,
    /// JWT claims from the HTTP invocation, if any — matches
    /// `ExecutionContext.claims`. The SQS invocation does NOT
    /// re-validate the JWT; claims ride with the envelope.
    pub claims: Option<serde_json::Value>,
    /// Enqueue timestamp for lag / DLQ diagnostics (epoch micros).
    pub enqueued_at_micros: i64,
}
```

### HTTP invocation (enqueue side)

`core_send_message` on the `return_immediately=true` path, when the capability flag is set AND a `DurableExecutorQueue` is wired:

1. URL + capability checks (unchanged).
2. Pre-compute `task_id` and `context_id`. The existing flow at `router.rs:836` already generates `task_id` via `uuid::Uuid::now_v7()` before task creation, so this is the same code path.
3. Build the `QueuedExecutorJob` envelope — includes `task_id`, `context_id`, `tenant`, `owner`, `claims` (see §"Claims plumbing" in the extraction section), `message`, and `enqueued_at_micros`.
4. Call `queue.check_payload_size(&job)`. On `QueueError::PayloadTooLarge` → return `A2aError::InvalidRequest` (HTTP 400 / JSON-RPC -32602 / gRPC `INVALID_ARGUMENT`) with a descriptive error message. **No task is created on this path** — the preflight runs before any storage write.
5. `create_task_with_events` (Submitted → Working).
6. Inline push config registration (ADR-017 Bug 2).
7. `queue.enqueue(job)`. On enqueue failure (storage / transport): FAILED compensation (same shape as ADR-017 Bug 2 inline-push-config storage-failure compensation). Task moves to FAILED with an agent `Message` carrying `"durable executor enqueue failed: <error>"`. No executor is ever spawned. Caller receives the error.
8. Return the Working task to the caller.

The HTTP invocation **never** also tokio-spawns the executor locally — double-execute would break idempotency. Enqueue is the sole dispatch path in Pattern B.

### SQS invocation (dequeue side)

A new `LambdaA2aService::handle_sqs(event: SqsEvent) -> SqsBatchResponse`. For each record in the batch:

1. Deserialize the payload. Unknown envelope `version` → batch-item failure (retry).
2. Load the task by `(tenant, task_id)`. Not found → batch-item failure (DLQ).
3. If task is already terminal → success (SQS deletes the record; idempotent no-op).
4. **Check cancel marker via `state.cancellation_supervisor.supervisor_get_cancel_requested(tenant, task_id)`** (`crates/turul-a2a/src/storage/traits.rs:195` — existing read API; no new trait surface). If `true`, the SQS handler commits CANCELED directly and the executor is NEVER invoked:
   - Build a CANCELED `TaskStatus` with an agent `Message` whose `Part::text` reads `"canceled before durable executor dispatch"` (same pattern as ADR-017 Bug 2 compensation and hard-timeout compensation at `router.rs:1034-1054`).
   - Call `atomic_store.update_task_status_with_events(tenant, task_id, owner, canceled_status, vec![canceled_event])`.
   - On `Ok` → success (CANCELED committed via CAS, SQS deletes the record).
   - On `TerminalStateAlreadySet` → success (a racing writer — typically a concurrent cancel-commit from another invocation or an executor that finished on a second delivery — won; the task is terminal, which is what the caller asked for).
   - On other storage error → batch-item failure (retry per the adopter's `MaximumRetryAttempts`).
5. `run_executor_for_existing_task(deps, scope)` — only reached if the task is non-terminal AND no cancel marker was observed. Synchronously awaits executor completion and post-execute detection. The Lambda invocation *is* the executor; no `tokio::spawn` needed.
6. On executor error: batch-item failure (SQS retries; DLQ on exhaustion).

Return `SqsBatchResponse { batch_item_failures }` so per-record retry is honoured (matches `lambda-stream-worker` pattern).

**Rationale for committing CANCELED directly rather than tripping the cancellation token and invoking the executor:** an `AgentExecutor::execute` body may perform side effects (external API calls, metric emits, log writes, database mutations) before its first `ctx.cancellation.is_cancelled()` check. For a task that was already cancelled before dispatch, those side effects are user-observable leakage. Committing CANCELED directly ensures the executor is never invoked at all for queued-then-cancelled tasks — no chance of partial side effects.

### `run_executor_body` extraction + claims plumbing (preparatory refactor)

`crates/turul-a2a/src/server/spawn.rs:185` currently hides `run_executor_body` inside the `tokio::spawn` closure of `spawn_tracked_executor`. Extract as `pub(crate) async fn run_executor_for_existing_task(deps, scope) -> Result<(), A2aError>` so:

- `spawn_tracked_executor` remains unchanged externally — its `tokio::spawn` body calls the extracted function.
- `LambdaA2aService::handle_sqs` calls it directly, in-line with the SQS Lambda invocation.
- Post-execute detection (§7.2) and terminal routing fire identically on both paths.

**Claims plumbing.** `SpawnScope` gains a `claims: Option<serde_json::Value>` field. `run_executor_for_existing_task` threads it into `ExecutionContext { claims, ... }` where today's code hardcodes `claims: None` (`crates/turul-a2a/src/server/spawn.rs:249`). This fixes a pre-existing bug on the blocking-send path where executors never observed JWT claims even when a valid token was presented. `QueuedExecutorJob.claims` now actually round-trips through to the executor on the SQS path.

**`core_send_message` signature change.** To make claims reachable inside `core_send_message`, its signature grows a parameter:

```rust
#[doc(hidden)]
pub async fn core_send_message(
    state: AppState,
    tenant: &str,
    owner: &str,
    claims: Option<serde_json::Value>,  // NEW
    body: String,
) -> Result<Json<serde_json::Value>, A2aError>;
```

Because `core_send_message` is `#[doc(hidden)]` (`router.rs:761`) it is not on the stable public API surface; this is an additive change, not a semver break. Call-sites updated in Phase 1:

- HTTP handlers at `router.rs:703` (and the tenant-prefixed variant) pass `ctx.identity.claims().cloned()`.
- JSON-RPC dispatch at `jsonrpc.rs:522-533` passes `ctx.identity.claims().cloned()`.
- gRPC dispatch at `grpc/service.rs:111` passes `None` — matches today's runtime behaviour since gRPC auth claims are not yet wired into the middleware. Wiring gRPC auth claims is a separate future ADR.

**Behaviour delta.** Phase 1 is not a pure code move. The blocking-send path begins delivering real claims to executors where it previously delivered `None`. This is a bug fix, and `ExecutionContext` is `#[non_exhaustive]`, but adopter executors that check `ctx.claims.is_none()` as a signal will observe a change. The 0.1.14 CHANGELOG calls out the delta explicitly.

Ships in a preparatory minor release (0.1.14) so the shape is already in place when the Phase 2 adapter code lands.

### Event dispatch (same Lambda, two event types)

Single Lambda function, two event shapes. The adapter provides:

```rust
pub enum LambdaEvent {
    Http(lambda_http::Request),
    Sqs(aws_lambda_events::sqs::SqsEvent),
}

impl LambdaEvent {
    /// Classify a raw `serde_json::Value` into the right variant,
    /// returning `None` for unknown event shapes so the binary can
    /// decide whether to panic, no-op, or log.
    pub fn classify(event: serde_json::Value) -> Option<Self>;
}
```

Adopter `main.rs` pattern:

```rust
lambda_runtime::run(lambda_runtime::service_fn(|event: serde_json::Value| async {
    match LambdaEvent::classify(event) {
        Some(LambdaEvent::Http(r))  => service.handle(r).await.map(Into::into),
        Some(LambdaEvent::Sqs(e))   => service.handle_sqs(e).await.map(Into::into),
        None => Err("unknown Lambda event shape"),
    }
}))
```

## Scope

**In scope (Phase 2):**

- `crates/turul-a2a/src/server/spawn.rs` — extract `run_executor_for_existing_task`. Ships in Phase 1 (preparatory, separate minor).
- `crates/turul-a2a-aws-lambda` — trait `DurableExecutorQueue`, impl `SqsDurableExecutorQueue` (feature-gated), builder methods, payload type, SQS event handler, `LambdaEvent` classifier.
- `crates/turul-a2a-aws-lambda/Cargo.toml` — `aws-sdk-sqs` optional dep behind `sqs` feature.
- ADR-017's `RuntimeConfig` field doc — extend to mention ADR-018's opt-in.
- Tests: unit tests with in-memory fake queue; SQS envelope serialization round-trip; enqueue-size reject; idempotency pre-flight.

**Out of scope:**

- `examples/lambda-durable-agent/` example crate + LocalStack integration test — follow-on work (Phase 3).
- SQS extended-client (S3-backed messages >256 KiB) — deferred; current ship-path rejects oversized payloads at enqueue time with a clear error.
- Non-SQS transports (Kinesis, SNS, self-invoke, Step Functions task-token). Trait is generic so they can be added without ADR changes; no implementation now.
- Cross-instance **live** cancellation of an SQS executor that has already started. **Pre-dispatch** cancellation of queued tasks IS handled by this ADR: §"SQS invocation (dequeue side)" step 4 reads the persisted cancel marker via `supervisor_get_cancel_requested` and commits CANCELED directly before invoking the executor. What remains out of scope is the post-dispatch case: once `run_executor_for_existing_task` is running inside an SQS Lambda invocation, a cancel marker written by a different invocation is only observed via the executor's cooperative `CancellationToken` loop (existing ADR-012 limitation — the Lambda adapter does not run the persistent cancel-marker poller that the binary server uses). Adding a poller inside the SQS invocation would add cross-instance-latency overhead to every record and is deferred.

## Compliance

- A2A 1.0 (`proto/a2a.proto:155-160`): `return_immediately` is a server capability, not a MUST-accept. A Lambda deployment that wires Pattern B honours the capability; one that does not still correctly refuses with `UnsupportedOperationError` (ADR-017 unchanged).
- ADR-004: error model unchanged. Enqueue-failure compensation reuses ADR-017 Bug 2's FAILED-with-reason shape.
- ADR-009 (durable event coordination): unchanged. Terminal commits in the SQS invocation go through the same `A2aAtomicStore::update_task_status_with_events` CAS path. Duplicate SQS deliveries produce `TerminalStateAlreadySet` on the second attempt, which the commit path already handles.
- ADR-011 (push delivery): unchanged. Terminal events in the SQS invocation produce pending-dispatch markers; the existing `lambda-stream-worker` / `lambda-scheduled-worker` fan out callbacks as today.
- ADR-013 (Lambda push-delivery parity): strengthened. Previously push delivery on Lambda was only correct for blocking sends. After ADR-018, push delivery is also correct for `return_immediately=true` when the adopter wires SQS.
- ADR-017: Pattern B is the concrete shape ADR-017 reserved. No spec-surface change.
- ADR-009 (same-backend requirement): not applicable to `DurableExecutorQueue`. The queue is a work queue, not storage. ADR-009's invariant governs `A2aAtomicStore` and `A2aPushDeliveryStore` because they share CAS semantics and event coordination. An SQS queue composes with DynamoDB / PostgreSQL / in-memory storage identically; there is no cross-consistency constraint between the queue and the backing store.

## Security

- **Trust boundary.** The enqueuing HTTP invocation and the consuming SQS invocation share an IAM identity (same Lambda function). Payload provenance is guaranteed by AWS IAM; no framework-level signing is added. Adopters deploying the SQS queue MUST restrict `sqs:SendMessage` to the request Lambda's role and `sqs:ReceiveMessage`/`DeleteMessage` to the same role.
- **Payload at rest.** SQS server-side encryption **MUST** be enabled when payloads carry JWT claims or user-supplied `Message` content (i.e., always in practice for this ADR). Customer-managed KMS (SSE-KMS with a CMK) is **RECOMMENDED** for tenant-sensitive deployments where the additional access-boundary and audit-trail guarantees of a customer-owned key are worth the operational cost; AWS-managed SSE (SSE-SQS) is acceptable for lower-risk or internal deployments where the AWS-owned-key boundary is sufficient. The framework does not enforce either choice — the decision is adopter / compliance-team territory. This ADR states the recommendation so the decision is documented in the deployment shape, not the default.
- **Claim expiry.** JWT claims ride with the envelope and are not re-validated by the SQS invocation. This is correct — the claims represent the caller's authorisation at the time the task was accepted, not at the time the executor runs. Tasks are authz-gated by `owner` on the HTTP side; the SQS invocation trusts the envelope.
- **Replay / duplicate delivery.** SQS at-least-once guarantees duplicate delivery can occur. Idempotency is enforced by: (a) pre-flight terminal check in the SQS handler, (b) CAS-guarded terminal commits in the storage layer. No separate dedup store is needed.

## Test obligations

All in `crates/turul-a2a-aws-lambda/tests/` unless noted.

- **Envelope round-trip** — `QueuedExecutorJob` serializes and deserializes through `serde_json` with all fields preserved, **including `claims`**; unknown envelope `version` rejects at dequeue.
- **Claims reach executor on both paths (Phase 1)** — a spy executor records `ctx.claims` on each call. Assert the blocking-send path observes `Some(original_claims)` (fixing the pre-existing `None` bug) AND the SQS-dequeue path observes `Some(original_claims)` after the envelope round-trip. Same assertion, two invocation paths.
- **Oversize payload does not create task** — construct a `QueuedExecutorJob` whose encoded length exceeds `max_payload_bytes()`; drive the HTTP path; assert (a) response is `A2aError::InvalidRequest` (HTTP 400), (b) `list_tasks` shows zero rows, (c) the fake queue's `enqueue` was never called.
- **Enqueue-failure compensation** — inject a `DurableExecutorQueue` fake whose `enqueue` always errors (but `check_payload_size` succeeds); assert the HTTP response is the error, the task is `TASK_STATE_FAILED` with an agent `Message` reading `durable executor enqueue failed: <error>`, and the in-flight registry is empty.
- **Idempotency** — fire the same SQS record twice: first invocation runs executor to terminal; second invocation observes terminal state on pre-flight load and returns success (SQS deletes the record).
- **Cancelled queued task commits CANCELED without executor invocation** — set the cancel marker before SQS dispatch; drive the SQS handler; assert (a) the task is `TASK_STATE_CANCELED`, (b) the terminal status `message` contains the text `canceled before durable executor dispatch`, (c) a spy executor's `execute` counter is `0` (never called), (d) the SQS handler returns success (record deleted, no retry).
- **Cancelled queued task, `TerminalStateAlreadySet` races as success** — set the cancel marker, drive the SQS handler concurrently with a competing terminal commit; assert one writer wins (either CANCELED from the SQS handler or the competing terminal); the SQS handler returns success regardless; no retry, no DLQ.
- **Concurrent duplicate** — two concurrent SQS invocations of the same record both reach the executor (neither observed the cancel marker); only one terminal commit succeeds; the loser sees `TerminalStateAlreadySet` and returns success.
- **SQS event handler — partial batch response** — mixed batch with one poison record returns `batch_item_failures` naming only that record.
- **`LambdaEvent::classify`** — HTTP-shaped event, SQS-shaped event, unknown shape each classify correctly.
- **Builder wiring** — `with_sqs_return_immediately(...)` sets `runtime_config.supports_return_immediately = true`; `with_durable_executor(fake)` does the same with a mock trait impl; absent call leaves the flag `false`.

## Migration

Adopters on Lambda who want `return_immediately=true`:

> Wire an SQS queue and configure the builder:
>
> ```rust
> let sqs = Arc::new(aws_sdk_sqs::Client::new(&aws_config));
> let handler = LambdaA2aServerBuilder::new(executor, storage)
>     .with_sqs_return_immediately(env::var("A2A_EXECUTOR_QUEUE_URL")?, sqs)
>     .build()
>     .await?;
> ```
>
> Deploy the queue, an event source mapping from the queue to this Lambda function, DLQ, and a visibility timeout at least as large as the executor's worst-case runtime. Grant the Lambda role `sqs:SendMessage` + `sqs:ReceiveMessage` / `DeleteMessage` / `GetQueueAttributes` on that queue. The Lambda binary's `main.rs` must route between HTTP and SQS events via `LambdaEvent::classify(event)`; see `examples/lambda-durable-agent/` (Phase 3).

No migration for:
- Non-Lambda deployments — no change.
- Lambda deployments that do not call `with_sqs_return_immediately` / `with_durable_executor` — no change; `return_immediately=true` continues to reject with `UnsupportedOperationError` per ADR-017.
- Lambda deployments using Pattern A (invoke workflow from inside skill handler) — no change; continue with `return_immediately=false`.

## Alternatives considered

- **Step Functions task token as the continuation mechanism.** Strictly more capable than SQS (built-in orchestration, visibility into long workflows). Rejected for the MVP: larger blast radius, requires adopters to design a state machine around every task, and the `DurableExecutorQueue` trait keeps the door open for a Step-Functions impl later.
- **Async self-invoke (Lambda invokes itself via AWS SDK).** Lower-latency than SQS (no queue hop) but no retry semantics, no DLQ, no batching, no visibility timeout. Rejected — loses the robustness the ADR is about.
- **Kinesis Data Streams.** Ordered per-partition delivery and better throughput ceiling than SQS. Rejected for Phase 2: adds complexity (shard management, checkpointing) the MVP does not need. Trait keeps it open.
- **SNS fan-out with Lambda consumer.** Duplicate-at-least-once plus no per-message retry without a downstream queue — worse shape than SQS directly.
- **Public `supports_return_immediately(bool)` setter.** Rejected in ADR-017 as a footgun. The capability-taking builder method shape here is the canonical opt-in.
- **Separate Lambda functions for HTTP and SQS paths.** Cleaner separation of concerns, worse developer experience (two deployables, two IAM roles, duplicate configuration). Rejected in favour of single-function dual-event-shape (matches user direction).
- **A `turul-a2a-aws-sqs` crate (new workspace member).** Would isolate the SQS dep. Rejected — feature-gating under `turul-a2a-aws-lambda/sqs` is lighter; adopters already depend on `turul-a2a-aws-lambda`.

## Risks and open questions

- **Visibility timeout tuning is adopter-owned.** If the timeout is shorter than the executor's worst-case runtime, SQS will redeliver mid-execution and two invocations race. CAS-guarded commits handle correctness, but the wasted work is real. README guidance calls this out; no framework-level enforcement because we cannot know the executor's runtime bounds.
- **Payload size ceiling.** 256 KiB is a hard SQS limit. Large user messages + claims can exceed it. MVP rejects at enqueue time; SQS extended-client (S3 offload) is deferred.
- **Cold-start latency.** SQS → Lambda invocation adds on the order of seconds of latency to executor start. Pattern A (workflow from inside the skill handler) has no extra cold-start on the executor side because there is no executor to run. Document the tradeoff; adopters pick per use case.
- **Observability.** The SQS hop introduces a new place where work can stall (queue backlog, DLQ accumulation). Framework emits `tracing::info!` on enqueue and dequeue; adopters are expected to wire CloudWatch metrics on the queue depth and DLQ size.

## Roll-out

- **Phase 1 — preparatory (next minor, 0.1.14).** Four itemised steps:
  1. Extract `run_executor_body` → `run_executor_for_existing_task` in `crates/turul-a2a/src/server/spawn.rs`.
  2. Thread `claims: Option<serde_json::Value>` through `SpawnScope` → `ExecutionContext` (fixes the pre-existing blocking-send claim-drop bug at `spawn.rs:249`).
  3. Change `core_send_message` signature from `(state, tenant, owner, body)` to `(state, tenant, owner, claims, body)` in `crates/turul-a2a/src/router.rs`.
  4. Update HTTP, JSON-RPC, and gRPC call sites (`router.rs:703`, `jsonrpc.rs:522-533`, `grpc/service.rs:111`) to pass claims (HTTP + JSON-RPC) or `None` (gRPC, until a future ADR wires gRPC auth).

  Still additive semver because (a) `core_send_message` is `#[doc(hidden)]`, (b) `ExecutionContext` is `#[non_exhaustive]`, (c) `SpawnScope` is crate-private. The behaviour delta (claims reaching executors where previously they saw `None`) is documented in the 0.1.14 CHANGELOG so adopter executors that branched on `ctx.claims.is_none()` get a warning before upgrade.
- **Phase 2 — this ADR (minor after Phase 1).** Trait, SQS impl, builder methods, SQS handler, `LambdaEvent` classifier, tests.
- **Phase 3 — example + docs (follow-on).** `examples/lambda-durable-agent/` demonstrating end-to-end wiring against LocalStack; README updates to the three Lambda example READMEs; CHANGELOG migration note.
