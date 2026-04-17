# ADR-011: Push Notification Delivery

- **Status:** Accepted
- **Date:** 2026-04-18 (drafted 2026-04-17, accepted 2026-04-18 after two rev cycles)
- **Depends on:** ADR-009 (durable event coordination), ADR-010 (EventSink + task lifecycle), ADR-012 (cancellation propagation)
- **Spec reference:** A2A v1.0 §9 "Push Notifications"

## Context

turul-a2a ships CRUD for push notification configurations (`A2aPushNotificationStorage` trait, handlers for `CreateTaskPushNotificationConfig`, `GetTaskPushNotificationConfig`, `ListTaskPushNotificationConfigs`, `DeleteTaskPushNotificationConfig`) but has no delivery mechanism. Configs are stored and never fire. This is the last remaining capability gap called out in the 2026-04-17 advanced-lifecycle coverage audit.

ADR-010 §6 committed to a specific design direction: **push delivery consumes the durable event stream (ADR-009)**, not per-execute() callbacks. ADR-012 locked down when and how cancellation events reach the store (handler-written terminals, executor-emitted sink events, framework-committed terminals on grace expiry). ADR-011 fills in the remaining design: the delivery worker, retry policy, authentication, cross-instance coordination, and observability.

**What the A2A proto gives us** (`a2a.proto:464–478`):

```
message TaskPushNotificationConfig {
  string tenant = 1;
  string id = 2;
  string task_id = 3;
  string url = 4 [REQUIRED];
  string token = 5;            // unique per task/session, for receiver validation
  AuthenticationInfo authentication = 6;
}

message AuthenticationInfo {
  string scheme = 1 [REQUIRED]; // IANA HTTP auth scheme: Bearer, Basic, Digest, etc.
  string credentials = 2;
}
```

The authentication model is `Authorization: {scheme} {credentials}` per HTTP. No OAuth2 flow, no token exchange. Simpler than a naive reading would suggest.

## Decision

### 1. Architecture — Background Delivery Worker per Instance

Delivery is performed by a **background tokio task set** per server instance, not inline in the atomic-store commit path. Inline delivery would (a) couple webhook latency to executor progress, (b) block the atomic-store transaction with network I/O, and (c) prevent failure isolation — one down webhook would stall all state transitions.

The worker subscribes to the same durable event stream that SSE subscribers use (ADR-009):

```
Producer (executor / framework / handler commit)
  → A2aAtomicStore write (durable, ordered)
  → broker wake-up for task_id
  → PushDeliveryWorker wakes, queries event store for new events
  → for each new event, resolves matching push configs for (tenant, task_id)
  → enqueues per-config delivery jobs (bounded mpsc channel)
  → per-config delivery task: HTTP POST with retry
```

**Why this works**: the event store is already the source of truth for streaming. Extending its consumer set to include push delivery is the same pattern SSE uses — no coherence worries, no new event channel.

**Worker composition**: a single `PushDeliveryWorker` task per instance drives dispatch. N per-config delivery tasks (spawned on demand) do the actual HTTP work. This isolates: one slow config does not block the dispatcher; one dispatcher panic is contained by the same sentinel pattern from ADR-012 §3.

### 2. Which Events Trigger Delivery

**In scope for 0.1.x**:
- All **TaskStatusUpdateEvent** commits (state changes). This includes framework-committed CANCELED (ADR-012 §8), framework-committed FAILED (ADR-010 §4.1 timeout path), and all executor-emitted terminal / interrupted / WORKING events.
- Terminal events MUST be delivered (as long as retry budget allows). The spec expects adopters to receive notification when a task finishes.

**Deferred to future ADR**:
- **TaskArtifactUpdateEvent**: artifact chunks fire at whatever cadence the executor emits. Delivering every chunk floods webhooks. For 0.1.x, artifact events are NOT delivered via push. An optional `deliver_artifacts: bool` field on `TaskPushNotificationConfig` could opt in later, but that's a wire format addition and requires a separate proposal.
- **Explicit filtering** (e.g., "only terminal", "only WORKING+terminal"). Current behavior is "all status updates." Configurability can be added later without wire changes if done via a new config field.

**Rationale**: the A2A spec §9 wording treats push as "notification of task state change." Sticking to `TaskStatusUpdateEvent` events matches this spec intent (each status update IS a state change) and avoids flooding receivers with per-chunk artifact traffic. If our reading proves narrower than the spec intends — i.e., if reviewers of the implementation find §9 mandates artifact delivery — we widen scope via a minor bump. The 0.1.x choice is conservative-by-default.

**Relationship to `AgentCapabilities.push_notifications` (`a2a.proto:410`)**: if an agent's card declares `push_notifications = false`, this ADR does NOT specify framework-level config rejection. The current CRUD handler accepts registrations regardless of the capability flag. A future tightening — reject `CreateTaskPushNotificationConfig` with `UnsupportedOperationError` when the agent card declares `push_notifications = false` — is worth doing but is out of scope for ADR-011. Tracking item for a follow-up.

### 3. Payload — Task Object, Per Spec §9

POST body is the current `Task` object (full snapshot at the time of the triggering event), serialized via proto JSON mapping (camelCase keys). This is what spec §9 prescribes.

**Not in the body**: event-specific data beyond what's in `Task.status` and `Task.history`. The receiver can derive which transition occurred by comparing against prior snapshots if they care. If receivers genuinely need the specific triggering event, they can additionally subscribe via SSE — push delivery is a "something changed, here's the latest state" signal, not a streaming transport.

**Serialization path**: `serde_json::to_vec(&task.as_proto())` via pbjson's serialization. Produces spec-compliant camelCase JSON. Compliance agent will verify.

**History size note**: `Task.history` can grow unbounded over long conversations. Receivers SHOULD NOT assume they receive the full history in every POST — the payload may be truncated to respect `push_max_payload_bytes` (R5). For authoritative history, receivers can `GetTask` the canonical server using the delivered `task_id`. The POST is a "something changed" signal with a best-effort snapshot, not a message bus.

### 4. Authentication — Literal Scheme + Credentials

Request headers:
- `Authorization: {authentication.scheme} {authentication.credentials}` — if `authentication` is present. Empty `credentials` means scheme-only (rare but legal for some IANA schemes).
- `X-Turul-Push-Token: {token}` — if `token` is set on the config. Purpose: the receiver can verify the delivery corresponds to the expected task/session. **This is a turul-specific header**, not standardized by the A2A spec. Spec §9 does not mandate a conveyance mechanism for `config.token`, so each A2A implementation chooses its own. The `X-Turul-` prefix makes the non-interop nature explicit: receivers expecting to switch between A2A server implementations without code changes MUST NOT rely on this header name. If the spec later standardizes a header name (e.g., `X-Turul-Push-Token`), we will migrate with a deprecation cycle.
- `Content-Type: application/json`
- `X-Turul-Event-Sequence: {event_sequence}` — the durable event sequence that triggered this delivery. Implementation-specific; receivers MAY use this alongside `{task_id, status.state, status.timestamp}` as an idempotency key to dedup retries.
- `User-Agent: turul-a2a/{version}` (informational).

**Receiver-side idempotency guidance**: retries mean receivers MAY see the same terminal POST multiple times (e.g., network cut after the receiver committed its side but before the 2xx reached us). Receivers SHOULD dedup on the tuple `(task_id, status.state, status.timestamp)` — the triggering event's identity is fully captured there. `X-Turul-Event-Sequence` provides a more convenient dedup key when both sides are turul.

**No outgoing JWT signing for 0.1.x**. If a deployment wants signed payloads, it puts the signed JWT in `authentication.credentials`. The server doesn't mint tokens on behalf of clients.

**No OAuth2 flow**. If a deployment needs OAuth2, it provides a valid Bearer token in `credentials` (refreshed out-of-band). Proto's `AuthenticationInfo` is just scheme+credentials; nothing in the spec requires the server to run an OAuth2 client. This is a deliberate scope limit.

### 4a. Secret Handling — Redaction Invariants

The following values are **secrets** and MUST be treated as such end-to-end:

- `AuthenticationInfo.credentials` (proto `a2a.proto:331`) — the material after the HTTP auth scheme.
- `TaskPushNotificationConfig.token` (proto `a2a.proto:475`) — the receiver-validation token.
- Any OAuth / API-key material embedded in `credentials` by deployments using those schemes.

**Internal representation**: credentials and tokens are held in a `Secret<String>` newtype (e.g., via the `secrecy` crate, or a thin in-crate equivalent) whose `Debug` and `Display` impls print `[REDACTED]`. The type cannot be formatted with `{:?}` or `{}` without an explicit `.expose_secret()` call — this makes accidental leakage a compile-time / explicit-call decision, not a silent happy-path mistake.

**Redaction invariants**: secrets MUST NOT appear in any of the following:

| Surface | Invariant |
|---|---|
| Structured logs | No secret in log fields. Log `config_id` instead — operators can cross-reference. |
| Metrics / metric labels | No secret in labels. Use `config_id` or anonymized `tenant` hash. |
| Distributed traces | No secret in span attributes. |
| Panic output | `Debug` derive MUST skip secret fields (custom `Debug` impl or `#[debug(skip)]`). Panic context emitted by framework MUST redact. |
| Test snapshots / fixtures | Integration tests that capture HTTP requests redact `Authorization` and `X-Turul-Push-Token` before asserting or snapshotting. |
| Delivery error messages | Error enum variants carry `config_id` and error class (§5b), NOT the raw credential or failure body. |
| Failed delivery records | Claim rows do NOT persist credentials, tokens, or request bodies (§5b). `config_id` suffices for cross-referencing. |
| Configuration dumps / introspection endpoints | If the server exposes config introspection (currently CRUD does), the wire format returns the credential verbatim per proto; that's the CRUD contract. But logs, metrics, and diagnostic dumps of the same config MUST redact. |

**Outbound HTTP**: the actual POST includes the credential in the `Authorization` header (that's the whole point). The `reqwest::Client` logs-by-default is disabled via `set_global_default` tracing config; any framework-level tracing of outbound HTTP redacts the Authorization header before emitting.

**Test guidance**: the parity-test suite includes redaction-coverage tests (§13 test #16) that spawn a delivery attempt, capture ALL framework log output and error values, and assert that no known-secret substring appears. Failures here are compile/test-time; production adopters never see a leaked secret because of test gaps.

**Cross-reference**: failed deliveries (§5b) and metrics (§9) both reference secrets-by-config_id rather than by credential. That pattern is load-bearing for operator inspection without secret exposure.

### 5. Retry Policy — Exponential Backoff with Jitter

**Delivery model**: push is **at-least-once, best-effort-within-horizon**. The retry horizon is tuned to cover common transient failures (receiver restart, rolling deploy, brief rate limit, short DNS blip) without being a reliable message queue substitute. Delivery guarantees are bounded by the retry horizon; beyond it, the framework gives up, persists the final failure record (§6), and moves on. Push delivery failure does NOT affect task state — the task's lifecycle is driven by executor and atomic-store writes, never by whether a webhook responded (§6).

Per-delivery retry schedule (default; deployment-configurable):

| Attempt | Delay before attempt | Total elapsed |
|---|---|---|
| 1 | 0 (immediate) | 0s |
| 2 | 2s ± 25% jitter | ~2s |
| 3 | 4s ± 25% jitter | ~6s |
| 4 | 8s ± 25% jitter | ~14s |
| 5 | 16s ± 25% jitter | ~30s |
| 6 | 32s ± 25% jitter | ~62s |
| 7 | 60s (capped) ± 25% jitter | ~122s |
| 8 | 60s (capped) ± 25% jitter | ~182s |

**Default: 8 attempts, ~3 minute horizon**. Covers typical receiver restarts and rate-limit windows. Exponential base 2s, doubling, capped at 60s per-wait. Jitter ±25% prevents thundering herd when many failed webhooks recover simultaneously.

**Configuration** (builder):
- `push_max_attempts: usize` — default 8.
- `push_backoff_base: Duration` — default 2s (first wait).
- `push_backoff_cap: Duration` — default 60s (max single wait).
- `push_backoff_jitter: f32` — default 0.25.
- `push_request_timeout: Duration` — default 30s (single POST).
- `push_claim_expiry: Duration` — default 10 minutes. **MUST exceed the retry horizon** so re-claim by another instance does not race live retries. The builder validates this constraint at construction time.

Deployments that want near-message-queue reliability (covering longer outages) should extend `push_max_attempts` and `push_claim_expiry` together. Deployments that want fire-and-forget best-effort can shorten both. The framework refuses configurations where `push_claim_expiry < max_horizon`.

**What triggers a retry**:
- Network error (connection refused, timeout, DNS failure) → retry.
- 5xx response → retry.
- 408 Request Timeout → retry.
- 429 Too Many Requests → retry (with `Retry-After` header respected if present, capped at `push_backoff_cap`).

**What does not retry**:
- 2xx response → success, stop.
- 3xx → treated as success; do NOT follow redirects automatically (§R3b / SSRF).
- 4xx other than 408/429 → permanent failure, no retry. The receiver explicitly rejected the delivery; retrying is wasteful.

### 5a. At-Least-Once Semantics (Explicit)

Push delivery is **at-least-once**. The framework does not guarantee exactly-once — that property is not achievable without receiver-side coordination beyond HTTP semantics, and the claim-then-crash failure mode is inherent to any worker-based delivery system.

**The redelivery failure mode**:
1. Instance A claims `(task_id, event_sequence, config_id)`.
2. Instance A successfully POSTs; receiver commits its side; receiver returns 200.
3. Instance A crashes *before* calling `record_delivery_outcome(Succeeded)`.
4. Claim expiry elapses. Instance B's sweep re-opens the claim.
5. Instance B POSTs; receiver sees the same event again.

This is correct-by-design, not a bug. The alternative (exactly-once) requires either (a) receiver-side dedup OR (b) a distributed transaction between our claim store and the receiver's application state — which HTTP cannot provide.

**Receiver obligations**: receivers MUST be idempotent for any state transition they care about. Recommended dedup keys:
- **Primary (spec-derivable, interop-portable)**: `(Task.id, Task.status.state, Task.status.timestamp)`. The `timestamp` field is on the proto `TaskStatus` message and changes with each status update — stable across implementations.
- **Convenience (turul-specific)**: `X-Turul-Event-Sequence` header. More convenient when both sides are turul, NOT a substitute for the primary key above.
- Do NOT use the webhook's own arrival timestamp; that varies with retry.

**What at-least-once buys us**: bounded failure blast radius. A crashed worker does not lose deliveries; another worker picks them up. Retry budget is cluster-wide bounded (§7) so redelivery is controlled, not open-ended.

### 5b. Failure Inspection — Where Failed Deliveries Live

When a delivery gives up after `push_max_attempts`, the claim record transitions to status `GaveUp` and is **retained for inspection**:

- **Where**: the same `a2a_push_deliveries` table/region as active claims. No separate dead-letter table.
- **Retention**: `push_failed_delivery_retention`, default 7 days. Successful claims can TTL sooner (24 hours by default) since there's no reason to hold them. Failed claims stick around for operator investigation.
- **Fields stored** (beyond the claim basics):
  - `first_attempted_at` — when the first POST was tried.
  - `last_attempted_at` — when the final POST failed.
  - `attempt_count` — final count (up to `push_max_attempts`).
  - `last_http_status: Option<u16>` — status of the last POST, if any.
  - `last_error_class` — enum: `NetworkError`, `Timeout`, `HttpError(5xx)`, `HttpError(4xx)`, `HttpError(429)`, `SSRFBlocked`, `PayloadTooLarge`, `ConfigDeleted`, etc. **Not a free-text error string** — classifications only, to avoid leaking secrets or PII from errors (see §4a).
  - `gave_up_at` — timestamp.
- **What is NOT stored**: the request body (privacy), the `Authorization` header value (secret), the `X-Turul-Push-Token` value (secret), receiver response body. The config can be re-fetched from the config CRUD store if needed.

**Operator inspection**: `A2aPushDeliveryStore::list_failed_deliveries(tenant, since, limit)` returns the failed delivery records in the tenant, scoped by time. Adopters expose this via their own admin API or direct database query. No framework-level HTTP endpoint in 0.1.x.

**Manual retry**: explicitly deferred to a future admin ADR. 0.1.x has no public API to re-trigger a gave-up delivery. Adopters who need it can write directly to the claim store (reset `status = Pending`, bump `attempt_count` if they want to respect retry budgets), but this is intentionally unsupported and may break in later versions when an admin surface lands.

**Effect on task state**: **none**. A failed push does not change task state, does not cancel, does not retry the underlying event in the durable event stream, does not affect SSE subscribers. Task lifecycle is driven exclusively by executor + framework writes to the atomic store (ADR-010/012). Push delivery is an observational side-channel; its success or failure is decoupled from task correctness.

**Why this matters for adopters**: a task can complete successfully, be observed by the agent's own logic, be returned to a blocking client, and separately have its push delivery give up because a webhook was down. The client sees the COMPLETED task; the webhook receiver sees nothing; both are correct. Adopters who want "reliable notification or task fails" need to build that on top (e.g., a post-task admin process that queries failed deliveries and takes action).

### 6. Failure Isolation

Per-config delivery runs in its own tokio task, spawned by the dispatcher. One config's failure/slowness does not affect others. A shared `reqwest::Client` is used across all delivery tasks for connection pooling.

**If a config repeatedly fails**: retry attempts per delivery are bounded (§5). There is no circuit-breaker in 0.1.x — every event triggers the full retry chain. Adopters whose webhooks are persistently down will see sustained background load, logged at WARN on each giveup. A future ADR can add a circuit-breaker (e.g., "skip delivery for this config for 10 minutes after N consecutive giveups") if needed.

**Dispatcher panic**: wrapped in the same `SupervisorSentinel` pattern from ADR-012 §3. Panic logs at ERROR, increments `framework.push_dispatcher_panics` metric, does not crash the server. In-flight delivery tasks continue on their own; the dispatcher is respawned by the server runtime.

### 7. Cross-Instance Coordination — Storage-Claim Dedup

Every instance runs its own `PushDeliveryWorker`. Without coordination, every instance would fire every webhook for every event. With shared storage, claim-first-writer-wins prevents this.

**Claim model**: before POSTing, a worker atomically writes a delivery claim record to storage:

```
(tenant, task_id, event_sequence, config_id) → (claimed_by_instance, claimed_at, attempt_count, status)
```

Atomic claim: `INSERT ... ON CONFLICT DO NOTHING` on SQL backends; `PutItem` with `ConditionExpression attribute_not_exists(pk)` on DynamoDB. If the write succeeds, this instance delivers. If it fails (another instance already claimed), skip.

**Claim expiry**: if a claim is older than `push_claim_expiry` (default 10 minutes; MUST exceed the retry horizon per §5) and still in `pending` or `attempting` status, another instance MAY re-claim and retry. This handles the case where the original claimant crashed mid-retry. The new claimant must increment `attempt_count`; when it exceeds `push_max_attempts`, the delivery is given up.

**Terminal delivery guarantee** under worker crash: if instance A claims and crashes between attempt 2 and 3, after claim expiry instance B picks up at attempt 3 and continues. Total attempts across the cluster are bounded by `max_attempts`, not `max_attempts × instance_count`. This relies on claim expiry being longer than the full retry window (16s typical) but shorter than "clearly abandoned" (5 minutes). Both configurable.

**Same-instance worker within its own retry loop** checks the claim is still owned by self before each retry. If another instance re-claimed (e.g., this instance's own heartbeat lapsed — unlikely but possible under GC pauses), this instance abandons and does not double-send.

### 8. Config Lifecycle Edge Cases

- **Config deleted between event commit and delivery attempt**: worker reads the config from storage just before each POST attempt. If the config no longer exists, the delivery is abandoned cleanly. No error, no retry.
- **Config deleted mid-retry chain**: next retry tick sees the config gone, abandons. Claim marked `abandoned`.
- **Config added after events exist (backfill)**: no backfill. Configs capture events commit-timestamped after registration. This avoids weird catch-up storms when a client registers many configs against long-running tasks.
- **Task deleted while push pending**: same as config deletion — next check finds task missing (`get_task` returns None), delivery abandoned.
- **Orphaned task with push configs** (task in WORKING but no executor; see ADR-012 §5): events only fire on state changes. An orphaned non-terminal task with configs attached produces no deliveries until something forces a terminal (cancel, TTL-cleanup, admin action). This is correct — the config fires on changes, not on "still running."

### 9. Observability

Metrics (emitted via tracing or structured logs for 0.1.x; adopters wire up their own metric backend). **All labels are secret-free** — see §4a redaction invariants.

**Delivery metrics**:
- `framework.push_deliveries_attempted` — counter, labeled by `tenant` and `config_id`.
- `framework.push_deliveries_succeeded` — counter.
- `framework.push_deliveries_failed` — counter, labeled by failure class (`NetworkError`, `Timeout`, `HttpError4xx`, `HttpError5xx`, `HttpError429`, `SSRFBlocked`, `PayloadTooLarge`, `ConfigDeleted`).
- `framework.push_deliveries_gave_up` — counter, final-failure after max attempts.
- `framework.push_delivery_latency` — histogram, per attempt.

**Claim-contention metrics** (cross-instance health):
- `framework.push_claim_attempts` — counter, every claim-write call.
- `framework.push_claim_conflicts` — counter, claim-writes that lost the race (another instance already claimed).
- `framework.push_claim_reopens` — counter, expired claims that this instance re-picked-up (§7).
- `framework.push_claim_age_on_success` — histogram, time from initial claim to successful delivery record.

Claim-conflict rate is a health signal: in a two-instance deployment under steady event load, roughly 50% conflict rate is expected (either instance may win the race on any event). Sustained >80% conflict rate indicates an imbalance (one instance running hot while the other drains most events). Sustained zero conflict rate in a multi-instance deployment indicates a coordination bug (each instance is processing its own subset without racing) and SHOULD be investigated.

**Infrastructure metrics**:
- `framework.push_dispatcher_panics` — counter, per §6.

**Logs**: structured at INFO on every attempt, WARN on every retry, ERROR on every giveup. Always include `task_id`, `tenant`, `config_id`, `event_sequence`, `attempt`, `error_class` (for failures). **Never** includes `Authorization` header value, `X-Turul-Push-Token`, `credentials`, or receiver response body (§4a).

### 10. Storage Contract — New `A2aPushDeliveryStore` Trait

The existing `A2aPushNotificationStorage` (CRUD for configs) is insufficient for delivery coordination. A new trait handles claim bookkeeping.

```rust
/// Delivery coordination. Separate from push config CRUD (which remains on
/// A2aPushNotificationStorage). Implemented alongside other storage traits
/// by the same backend struct.
#[async_trait]
pub trait A2aPushDeliveryStore: Send + Sync {
    /// Attempt to claim delivery of an event to a config. Returns the claim
    /// on success, or `ClaimAlreadyHeld` if another instance holds it and
    /// it is not yet expired.
    async fn claim_delivery(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        config_id: &str,
        claimant: &str,             // instance identifier
        claim_expiry: Duration,
    ) -> Result<DeliveryClaim, A2aStorageError>;

    /// Record the outcome of a delivery attempt. Updates the claim's
    /// attempt count and status. If status is `Succeeded` or `GaveUp`,
    /// subsequent claims on this (tenant, task_id, event_sequence, config_id)
    /// return `ClaimAlreadyHeld` without allowing re-claim.
    async fn record_delivery_outcome(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        config_id: &str,
        outcome: DeliveryOutcome,
    ) -> Result<(), A2aStorageError>;

    /// Sweep expired claims — called periodically by server maintenance.
    /// Returns number of claims re-openable for re-attempt.
    async fn sweep_expired_claims(&self) -> Result<u64, A2aStorageError>;

    /// Operator inspection: list failed deliveries (status = GaveUp) within
    /// a tenant, ordered newest-first, bounded by `limit`. Used by adopters
    /// to surface failed-delivery state via their own admin endpoints.
    /// Does NOT return credentials, tokens, or receiver response bodies —
    /// those are never persisted (§4a, §5b).
    async fn list_failed_deliveries(
        &self,
        tenant: &str,
        since: SystemTime,
        limit: usize,
    ) -> Result<Vec<FailedDelivery>, A2aStorageError>;
}

pub struct FailedDelivery {
    pub task_id: String,
    pub config_id: String,
    pub event_sequence: u64,
    pub first_attempted_at: SystemTime,
    pub last_attempted_at: SystemTime,
    pub gave_up_at: SystemTime,
    pub attempt_count: u32,
    pub last_http_status: Option<u16>,
    pub last_error_class: DeliveryErrorClass,  // enum — NOT free-text error string
}

pub enum DeliveryErrorClass {
    NetworkError,
    Timeout,
    HttpError4xx { status: u16 },
    HttpError5xx { status: u16 },
    HttpError429,
    SSRFBlocked,
    PayloadTooLarge,
    ConfigDeleted,
    TaskDeleted,
    TlsRejected,
}

pub struct DeliveryClaim {
    pub claimant: String,
    pub claimed_at: SystemTime,
    pub attempt_count: u32,
    pub status: ClaimStatus,
}

pub enum ClaimStatus { Pending, Attempting, Succeeded, Failed, GaveUp, Abandoned }

pub enum DeliveryOutcome {
    Succeeded { http_status: u16 },
    Retry { next_attempt_at: SystemTime, http_status: Option<u16>, error: String },
    GaveUp { reason: String },
    Abandoned { reason: String },  // config deleted, task deleted, etc.
}
```

**Per-backend shape**:
- **In-memory**: `DashMap<(tenant, task_id, event_sequence, config_id), DeliveryClaim>`.
- **SQLite / PostgreSQL**: new table `a2a_push_deliveries` with composite PK, status enum, attempt counter, claim timestamp. Indexed on `(claim_expires_at)` for sweep efficiency.
- **DynamoDB**: new table `a2a_push_deliveries` with composite PK `{tenant}#{task_id}#{event_sequence}#{config_id}`. TTL for natural expiry.

**Additive**: existing `A2aPushNotificationStorage` trait is unchanged. Adopters who don't wire up a `PushDeliveryWorker` don't need the new trait (configs stored, not delivered — matches current behavior).

### 11. Config Resolution — Matching Configs to Events

For each event on `(tenant, task_id)`, the worker lists push configs for the task via the existing CRUD method. The list is small (typical: 1-3 configs per task) and lookups are cached briefly (`push_config_cache_ttl`, default 5s) to avoid hammering storage on high-event-rate tasks.

If no configs are registered, the event is effectively dropped from the push perspective (SSE subscribers still receive it). This is normal and expected — most tasks don't have push configs.

### 12. Proto and Wire Compliance

- Delivery body: `Task` proto message serialized via pbjson (camelCase JSON).
- Wire headers: `Authorization`, `X-Turul-Push-Token` (if token set), `X-Turul-Event-Sequence`, `Content-Type`, `User-Agent`. The `X-Turul-*` headers are implementation-prefixed and not A2A-spec standardized; see §4 and R6 for scope.
- Delivery path is outbound; no changes to the A2A inbound wire surface.
- No new proto fields. All delivery coordination is storage-internal. Spec-compliant.

### 13. Required Tests Before GREEN

1. **Basic delivery on terminal event**
   Non-blocking send, executor completes. Wiremock webhook registered via push config before send. Assert: exactly one POST arrives at the wiremock within 1s of terminal commit. Body is the Task JSON with status.state = "TASK_STATE_COMPLETED". `Authorization` header matches config.authentication. `X-Turul-Push-Token` matches config.token.

2. **Multiple configs fire independently**
   Three push configs on same task. Executor completes. Assert: three POSTs, one to each URL. Orderings are not guaranteed across configs. Each config receives the same Task body.

3. **Retry on 5xx**
   Wiremock responds 503 twice, then 200. Assert: exactly 3 POST attempts, first two at 0ms and ~1s (±jitter), third at ~3s. Final claim status is `Succeeded`. Metrics: `succeeded` incremented once, `failed` incremented twice with label `5xx`.

4. **Giveup after max attempts**
   Wiremock responds 500 forever. Assert: exactly `push_max_attempts` (8 default) POSTs occur across the full retry schedule (~182s total), then no more. Claim ends in `GaveUp` state. ERROR log emitted. `push_deliveries_gave_up` metric incremented. (This test runs against a faster configured horizon in CI — e.g. `push_max_attempts=4, push_backoff_base=100ms` — so total wall-clock stays under 2s. The default-timing assertion is integration-level only.)

5. **No retry on 4xx (non-408/429)**
   Wiremock responds 400. Assert: exactly 1 POST. Claim ends in `Failed` state. `failed` metric with label `4xx`. No WARN retry logs.

6. **429 respects Retry-After**
   Wiremock responds 429 with `Retry-After: 2` header. Assert: next attempt is at least 2s later. Retry-After capped at backoff maximum (8s); wiremock returning Retry-After: 60 would be clipped.

7. **Config deleted mid-retry abandons cleanly**
   Wiremock responds 500. After attempt 2, delete the push config. Assert: attempt 3 does not fire. Claim ends in `Abandoned` with reason "config deleted." No error logs; INFO log for the abandon.

8. **Cross-instance claim dedup**
   Two instances sharing storage. Both observe the same terminal event. Assert: exactly one POST (from whichever instance wins the claim race). Second instance observes `ClaimAlreadyHeld` and skips. Total event store writes: unchanged (claims are in their own table).

9. **Claim expiry re-picks up abandoned work**
   Instance A claims, makes attempt 1, then is killed (simulated: delete the worker task). Wait for claim expiry (test uses short expiry: 2s). Instance B's sweep re-claims, makes attempt 2. Assert: total POSTs across cluster = attempts 1 (A) + attempts 2–5 (B if 500s continue) = 5 max. Terminal event delivery completes without loss.

10. **Task deletion between event and delivery abandons**
    Executor completes, terminal event commits, POST in retry 2 pending. Admin deletes task via storage. Next retry checks task presence, finds gone, abandons. Claim status: `Abandoned`.

11. **Artifact events are NOT delivered (0.1.x scope)**
    Executor emits 3 `ArtifactUpdate` events + terminal. Push config registered. Assert: exactly one POST (for the terminal status update). Artifact chunks did not trigger deliveries. This enforces §2 scope.

12. **Authentication variants — Bearer, Basic, API-key**
    Three sub-cases. For each auth scheme: verify wiremock received the exact `Authorization` header value. Schemes tested: `Bearer <token>`, `Basic <base64>`, `ApiKey <key>` (custom scheme per IANA registry).

13. **Framework-committed CANCELED triggers delivery**
    Orphaned task: create in storage as WORKING with no executor, register push config, send `CancelTask`, let grace expire so framework commits CANCELED (per ADR-012 §2.5). Assert: webhook receives POST with task.status.state = "TASK_STATE_CANCELED" — framework-committed terminals deliver identically to executor-emitted.

14. **Push config CRUD parity — unchanged**
    Existing CRUD tests from `handler_tests::push_config_crud_through_http` continue to pass. ADR-011 is purely additive to the CRUD surface.

15. **Dispatcher panic recovery**
    Inject a panic in the dispatcher task. Assert: `push_dispatcher_panics` metric incremented, ERROR log emitted, server remains healthy, in-flight delivery tasks complete successfully, new events post-panic resume delivery after the dispatcher respawns.

16. **Secret redaction coverage**
    Delivery attempt fires with a known-sentinel credential (e.g., `"SECRET-CREDENTIAL-DO-NOT-LEAK"`) and a known-sentinel push token (e.g., `"SECRET-TOKEN-DO-NOT-LEAK"`). Capture all framework log output, all metric labels, all span attributes, and all error values surfaced during the attempt and its subsequent retries. Assert: neither sentinel substring appears in any captured output. This test is the compliance gate for §4a — any future refactor that accidentally logs credentials will fail CI.

17. **SSRF blocklist rejects private destinations**
    Register push configs with URLs resolving to: `127.0.0.1`, `10.0.0.5`, `169.254.169.254` (AWS metadata), `192.168.1.1`, `::1`, `fe80::1234`. For each, assert: delivery attempt is refused pre-POST with error class `SSRFBlocked`, record in `list_failed_deliveries` shows `last_error_class = SSRFBlocked`, no outbound packet was sent (use a packet sniffer or wiremock listening on 0.0.0.0 assertion).

18. **DNS rebinding defense**
    Test-only DNS resolver returns `203.0.113.10` (public IP) on first lookup, then `127.0.0.1` on subsequent lookups. The framework should resolve once per attempt, connect to the first-returned `203.0.113.10`, and not be swapped to `127.0.0.1` by DNS. Assert: POST goes to `203.0.113.10`'s wiremock instance, the second DNS answer was never used.

19. **Outbound allowlist hook**
    Deployment configures `outbound_url_validator` to reject all hosts outside `*.example.com`. Register a push config pointing at `evil.attacker.com`. Assert: delivery blocks pre-DNS with `SSRFBlocked`, validator's reason string appears in the failed-delivery record's error class (as a `SSRFBlocked` with operator-visible metadata via structured log — the class itself is a fixed enum).

20. **Claim contention metrics in two-instance deployment**
    Two test instances share in-memory storage (or SQLite in-memory DB). Emit N=20 terminal events. Assert: cumulative `push_claim_attempts` ≈ 2N (both instances try to claim each event), `push_claim_conflicts` ≈ N (the losers), `push_deliveries_attempted` = N (only winners deliver). Sum of successes + giveups across instances = N. No duplicate deliveries.

21. **At-least-once redelivery after claim-then-crash**
    Instance A claims event, successfully POSTs to wiremock, then crashes (simulated) before `record_delivery_outcome(Succeeded)`. Wait for claim expiry (test config: 500ms expiry for fast testing). Instance B sweep re-claims. Assert: second POST arrives at wiremock within 1s of expiry. Total POSTs = 2 for the same event. Receiver-side dedup (via `(task_id, status.state, status.timestamp)`) would collapse these — framework does not dedup; responsibility is on the receiver. Metrics: `push_claim_reopens` incremented by 1.

22. **Retry horizon covers receiver restart (3 minute window)**
    Wiremock initially responds with connection-refused. After 90 seconds, wiremock starts responding 200. Assert: delivery succeeds on attempt 6 or 7 (depending on jitter), total elapsed ≈ 90–120s, well within the default ~3min horizon. `push_deliveries_succeeded` incremented once; `push_deliveries_failed` incremented 5–6 times with class `NetworkError`.

23. **Failed delivery record is inspectable and secret-free**
    Configure wiremock to 500 forever; push config has sentinel credential + token. Wait for full retry horizon + giveup. Call `list_failed_deliveries(tenant, since=T0, limit=100)`. Assert: returns 1 record with correct `task_id`, `config_id`, `event_sequence`, `attempt_count = 8`, `last_http_status = 500`, `last_error_class = HttpError5xx`. The record does NOT contain the credential or token sentinels (cross-check with test #16's redaction assertion). `gave_up_at` is within the expected window from first attempt.

### 14. What Changes in Existing Code

**New**:
- `crates/turul-a2a/src/push/delivery.rs`: `PushDeliveryWorker`, per-config delivery task, retry loop. Uses shared `reqwest::Client`.
- `crates/turul-a2a/src/push/claim.rs`: claim abstractions, `A2aPushDeliveryStore` trait, `DeliveryClaim`, `ClaimStatus`, `DeliveryOutcome`.
- `crates/turul-a2a/src/storage/{memory,sqlite,postgres,dynamodb}.rs`: each backend implements `A2aPushDeliveryStore`.
- `crates/turul-a2a/src/storage/parity_tests.rs`: parity tests for `claim_delivery`, `record_delivery_outcome`, `sweep_expired_claims`.
- `crates/turul-a2a/src/server/builder.rs`: new config fields (all optional with sane defaults):
  - Retry / timeouts: `push_max_attempts`, `push_backoff_base`, `push_backoff_cap`, `push_backoff_jitter`, `push_request_timeout`, `push_connect_timeout`, `push_read_timeout`.
  - Claim lifecycle: `push_claim_expiry`, `push_config_cache_ttl`, `push_failed_delivery_retention`.
  - Safety: `push_max_payload_bytes`, `allow_insecure_push_urls` (dev only), `outbound_url_validator` (optional hook).
  - Builder-time validation: the builder rejects configurations where `push_claim_expiry < max_horizon_from_max_attempts_and_cap`. Construction fails with a clear error rather than producing a runtime-unsafe server.
- `crates/turul-a2a/tests/push_delivery_tests.rs`: wiremock-backed integration tests per §13.

**Modified**:
- `AppState`: new `push_delivery_store: Arc<dyn A2aPushDeliveryStore>` field, wired by builder from the same backend.
- `A2aServer::run()` (or equivalent startup): spawns `PushDeliveryWorker` on server start; cancels on shutdown.
- Test harness shared helpers may need a "without push delivery" mode for tests that don't care about it (most existing tests).

**Unchanged**:
- `A2aPushNotificationStorage` CRUD trait signatures.
- Proto wire format — no new fields, no modified fields, nothing.
- Handler behavior for push CRUD — unchanged; passes existing tests.
- Executor contract — unchanged; `EventSink` emissions end up at push webhooks the same way they end up at SSE streams (via the durable event store).

### 15. Risks and Mitigations

**R1 — Webhook receiver is slow / flaky, generates sustained retry load**. Bounded by `push_max_attempts` (8 default; ~3min total horizon). Sustained load is logged and metric'd (§9). Future ADR can add circuit-breaker. For 0.1.x, a consistently-failing webhook just produces up to 8 failed deliveries per event and gives up; failed-delivery rows are retained for `push_failed_delivery_retention` (default 7 days, §5b) for operator inspection, then cleaned up. Healthy-completion claim rows TTL sooner (24h by default).

**R2 — Cross-instance claim race starves slow-winner**. Mitigated by claim expiry (§7): if the winner takes too long, another instance re-claims. Bounded by `push_claim_expiry` (default 10m). Tune in concert with `push_max_attempts` — the builder enforces `push_claim_expiry > retry_horizon`. Shorter claim expiry requires shorter retry horizon.

**R3 — SSRF and outbound safety**. Push delivery is a server-initiated outbound HTTP request whose destination is client-controlled (the `url` field of a push config). Without defenses, this is a textbook SSRF vector — a malicious or misconfigured client registers an internal URL and the agent becomes an attacker-controlled proxy into the deployment's private network.

Framework-enforced production protections:

1. **Destination-IP validation (post-resolution)**:
   - Reject loopback: `127.0.0.0/8`, `::1`.
   - Reject link-local: `169.254.0.0/16`, `fe80::/10`.
   - Reject RFC1918 private: `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`.
   - Reject IPv6 unique local: `fc00::/7`.
   - Reject multicast: `224.0.0.0/4`, `ff00::/8`.
   - Reject the broadcast address `255.255.255.255`.
   - Reject the unspecified addresses `0.0.0.0`, `::`.
   - Reject AWS / GCP metadata service: `169.254.169.254`, `fd00:ec2::254`.

2. **DNS-rebinding defense**:
   - Resolve the hostname exactly once per POST attempt.
   - Validate each resolved IP against the blocklist above.
   - Connect to a specific IP (using a custom `reqwest::Resolver`), not the hostname, so a subsequent DNS swap cannot swap the destination between the check and the connect.
   - Preserve `Host:` header + SNI for virtual hosting on the real destination.

3. **Redirects**: not followed automatically (reqwest config `.redirect(Policy::none())`). Any 3xx is recorded as success per §5 (rationale: some webhook services 301 to a success page). Never follow — the redirect target could be internal.

4. **Timeouts** (each configurable):
   - `push_connect_timeout`: 5 seconds default. Fails fast on unreachable hosts.
   - `push_read_timeout`: 30 seconds default. Hard cap on receiving response.
   - Total timeout (connect + TLS + write + read): enforced by `push_request_timeout` (already defined), 30s default.

5. **Request body size cap**: `push_max_payload_bytes`, 1 MiB default. If the serialized Task exceeds this, the delivery records `PayloadTooLarge` (§5b error class) and is given up without POSTing. Prevents the framework from spraying multi-MB payloads at webhooks that can't handle them and from consuming excessive network bandwidth on chunk-heavy tasks.

6. **Outbound allowlist hook** (optional): deployments may provide `outbound_url_validator: Arc<dyn Fn(&url::Url) -> Result<(), String> + Send + Sync>`. Called before DNS resolution. Returning `Err` aborts the delivery with error class `SSRFBlocked` and the provided reason. Examples: allow only a known set of vendor hostnames; require a specific port; require a TLS cert pin (validated inside the validator using whatever out-of-band mechanism the deployment has).

7. **`allow_insecure_push_urls`** remains the dev-only flag (default `false`, §R3b). When set `true`, ALSO disables the private-IP blocklist — otherwise `http://localhost:8080` tests would fail the new blocklist. The flag is documented as mutually-exclusive with production use. The server emits a WARN log at startup if this flag is set in a build with `cfg!(not(debug_assertions))` — making accidental production misconfiguration loud.

**R3b — TLS requirement for delivery URLs**. A2A spec §7.1 mandates TLS 1.2+ for A2A transport. Push delivery is an outbound A2A-adjacent HTTP flow. Policy:
- **Production**: the framework enforces `https://` at delivery time. A config registered with `http://` succeeds at CRUD (no validation at registration to keep CRUD simple and backend-portable) but the delivery worker refuses to POST, logs WARN, and records the claim as `Abandoned { reason: "non-https URL in production mode" }`.
- **Development / test**: `allow_insecure_push_urls` (see R3 point 7) permits `http://` delivery for localhost testing. Must be set explicitly per-deployment.
- No automatic cert pinning, no mTLS delivery in 0.1.x. The reqwest default (rustls + system roots) handles standard TLS.

**R4 — Outbound redirects hit internal endpoints (open redirect into SSRF)**. Mitigated: delivery does NOT follow redirects (§5). Non-2xx/3xx are errors; 3xx treated as opaque success per §5. Webhooks that legitimately 301-redirect will receive a single POST and a logged "unexpected 3xx" info — this is intentional.

**R5 — Payload size for large tasks exceeds limits**. A task with hundreds of history messages could produce a multi-megabyte POST body. Mitigation: `push_max_payload_bytes` limit (default 1 MiB), configurable. If the task's serialized form exceeds the limit, delivery records an `Abandoned` claim with reason "payload too large" and emits WARN. Future optimization: deliver a compact "task_id + state + version" envelope instead of the full Task — requires spec alignment.

**R6 — Receiver verifies `token` but config's token field was never persisted**. Per proto, `token` is optional. If absent, `X-Turul-Push-Token` header is not sent. Receivers that require the token must validate config registration accordingly — not the framework's concern.

**R7 — Dispatcher is a single point of failure on each instance**. Mitigated by sentinel cleanup (§6) and server-runtime respawn. Future scaling: replace single dispatcher with sharded dispatcher set (e.g., hash task_id mod N). Deferred; 0.1.x single-dispatcher handles expected workloads.

## Consequences

**Positive**:
- The final piece of the advanced lifecycle matrix (push delivery) is resolved. Adopters can now register webhooks and trust they fire.
- Reuses the same durable event store that SSE consumes — no new event channel, no coherence burden.
- Cross-instance correctness via storage-claim dedup, matching the same "storage is truth" philosophy as ADR-009 and ADR-012.
- Claim expiry + re-claim handle worker crashes cleanly; terminal deliveries are not silently lost.
- No wire format changes required — entire design rides within proto's `TaskPushNotificationConfig`.

**Negative**:
- New storage table/region (`a2a_push_deliveries`) on SQL and DynamoDB backends. Additive migration.
- New background worker per instance. Small CPU + memory footprint for idle instances; scales with event rate + config count.
- `A2aPushDeliveryStore` is a fourth storage trait (after `A2aTaskStorage`, `A2aEventStore`, `A2aAtomicStore`, `A2aCancellationSupervisor`). External backend implementers now implement five traits for full push support.
- Retry policy defaults (exponential backoff, 8 attempts, ~3 min horizon) are fixed at startup per the builder. Deployments can tune within the builder-enforced safety constraints (`push_claim_expiry > retry_horizon`). No circuit-breaker in 0.1.x. Deployments with persistently failing webhooks will generate sustained retry traffic until the config is deleted or the failed-delivery retention expires.

**Neutral**:
- Artifact events do not trigger push. Adopters who need them will eventually get a config flag (future ADR). Documented as scope limit now.

## Follow-Ups (Not in 0.1.x)

- **Circuit-breaker**: skip delivery for a config after N consecutive giveups, re-enable after a cooldown. Prevents persistent bad configs from generating sustained load.
- **Artifact event delivery opt-in**: `deliver_artifacts: bool` on `TaskPushNotificationConfig`. Requires proto coordination.
- **DLQ / replay**: expose the delivery claim table as an admin surface so deployments can inspect and manually retry gave-up deliveries.
- **Sharded dispatcher**: replace the single per-instance dispatcher with hash-sharded tasks for very-high-throughput deployments.
- **OAuth2 client**: if adopters commonly need OAuth2-authenticated webhooks, add a token-refresh layer. Scope creep for 0.1.x.

## Alternatives Considered

- **Inline delivery in atomic-store commit hook**: rejected (§1) — couples webhook latency to executor progress, breaks failure isolation.
- **No cross-instance dedup; every instance fires every webhook**: rejected — produces N× delivery volume, receivers would see duplicates for every event.
- **Leader election (one "push leader" instance)**: rejected — adds coordination complexity (raft, distributed lock, etc.). Storage-claim dedup is simpler and matches ADR-009's philosophy.
- **Per-event dedicated delivery tasks (no dispatcher)**: rejected — hard to bound concurrency, harder to reason about backpressure. Single dispatcher + per-config delivery tasks gives both bounded concurrency and failure isolation.
- **Synchronous delivery blocking the executor**: rejected for all the reasons in §1. Also violates the spirit of "push is a side-effect signal," not "client's response depends on it."
