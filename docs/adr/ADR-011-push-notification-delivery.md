# ADR-011: Push Notification Delivery

- **Status:** Accepted, amended 2026-10-22 to what ships (see the revision log)
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

**Worker composition**: a single `PushDeliveryWorker` task per instance drives dispatch. N per-config delivery tasks (spawned on demand) do the actual HTTP work. This isolates: one slow config does not block the dispatcher.

> **Amendment 2026-10-22 — what ships.** There is no long-lived worker subscribing to the event stream and no mpsc channel. A terminal status commit writes a pending-dispatch marker inside the same atomic commit (ADR-013 §4.3); the committing code path then calls `PushDispatcher::dispatch`, which spawns one fan-out task that lists the configs eligible for that event (ADR-013 §4.5) and spawns one `PushDeliveryWorker::deliver` per config. Recovery is the server's reclaim loop in `A2aServer::run` (stale markers and expired claims), or the Lambda stream / scheduled workers (ADR-013). No panic supervision exists: a panic in a per-config task is absorbed by its `JoinHandle`; a claim row it left non-terminal is reclaimed after `push_claim_expiry`, and a marker the fan-out did not delete is redispatched by the reclaim loop.

### 2. Which Events Trigger Delivery

> **Amendment 2026-10-22 — Accepted.** The 2026-04 text below said every status update is delivered. The code has only ever delivered **terminal** status events: the pending-dispatch marker is written inside the atomic commit for terminal states only, on all four backends, and the dispatcher filters on `is_terminal`. This amendment states what ships. Whether interrupted states (INPUT_REQUIRED, AUTH_REQUIRED) should also be pushed is a separate, open decision.

**In scope for 0.1.x**:
- **Terminal TaskStatusUpdateEvent commits** (COMPLETED, FAILED, CANCELED, REJECTED). This includes framework-committed CANCELED (ADR-012 §8) and framework-committed FAILED (ADR-010 §4.1 timeout path).
- Terminal events MUST be delivered (as long as retry budget allows). The spec expects adopters to receive notification when a task finishes.
- WORKING and interrupted states are **not** delivered today. A client using `returnImmediately` + push is therefore not told when its task needs input; it has to poll `GetTask` or subscribe.

**Deferred to future ADR**:
- **TaskArtifactUpdateEvent**: artifact chunks fire at whatever cadence the executor emits. Delivering every chunk floods webhooks. For 0.1.x, artifact events are NOT delivered via push. An optional `deliver_artifacts: bool` field on `TaskPushNotificationConfig` could opt in later, but that's a wire format addition and requires a separate proposal.
- **Explicit filtering** (e.g., "only terminal", "only WORKING+terminal"). Current behavior is "terminal only." Configurability can be added later without wire changes if done via a new config field.

**Rationale**: the A2A spec §9 wording treats push as "notification of task state change." Sticking to `TaskStatusUpdateEvent` events matches this spec intent (each status update IS a state change) and avoids flooding receivers with per-chunk artifact traffic. If our reading proves narrower than the spec intends — i.e., if reviewers of the implementation find §9 mandates artifact delivery — we widen scope via a minor bump. The 0.1.x choice is conservative-by-default.

**Relationship to `AgentCapabilities.push_notifications` (`a2a.proto:410`)**: if an agent's card declares `push_notifications = false`, this ADR does NOT specify framework-level config rejection. The current CRUD handler accepts registrations regardless of the capability flag. A future tightening — reject `CreateTaskPushNotificationConfig` with `UnsupportedOperationError` when the agent card declares `push_notifications = false` — is worth doing but is out of scope for ADR-011. Tracking item for a follow-up.

### 3. Payload — `StreamResponse` Carrying the Task Snapshot

> **Amendment 2026-10-22 — Accepted and implemented.** A2A v1.0.1 §4.3.3 requires the webhook body to be a `StreamResponse`, "the same format as streaming operations". The original decision POSTed a bare `Task`, which is not a `StreamResponse`. One function, `push::dispatcher::push_payload`, builds the body for both the fresh-dispatch and the reclaim path.

POST body is a `StreamResponse` whose `task` member is the current `Task` (full snapshot at the time of the triggering event): `{"task": { … }}`, serialized via proto JSON mapping (camelCase keys). This keeps every property of the original decision (the receiver still gets the full latest state) while matching the spec's envelope. `Content-Type` is `application/a2a+json`, the registered A2A media type used in the §4.3.3 example.

Using the `task` member rather than `statusUpdate` is deliberate: a terminal push is the receiver's last word on the task, and the snapshot carries artifacts and status message that a bare status event would not.

**Not in the body**: event-specific data beyond what's in `Task.status` and `Task.history`. The receiver can derive which transition occurred by comparing against prior snapshots if they care. If receivers genuinely need the specific triggering event, they can additionally subscribe via SSE — push delivery is a "something changed, here's the latest state" signal, not a streaming transport.

**Serialization path**: the body is the proto `StreamResponse` with its `task` member set, serialised via pbjson (camelCase JSON) — exactly one top-level member, `task`.

**History size note**: `Task.history` can grow unbounded over long conversations. Receivers SHOULD NOT assume they receive the full history in every POST — the payload may be truncated to respect `push_max_payload_bytes` (R5). For authoritative history, receivers can `GetTask` the canonical server using the delivered `task_id`. The POST is a "something changed" signal with a best-effort snapshot, not a message bus.

### 4. Authentication — Literal Scheme + Credentials

Request headers:
- `Authorization: {authentication.scheme} {authentication.credentials}` — if `authentication` is present. Empty `credentials` means scheme-only (rare but legal for some IANA schemes).
- `X-Turul-Push-Token: {token}` — if `token` is set on the config. A2A v1.0.1 still names no header for conveying `token`, so this stays turul-specific. Purpose: the receiver can verify the delivery corresponds to the expected task/session. **This is a turul-specific header**, not standardized by the A2A spec. Spec §9 does not mandate a conveyance mechanism for `config.token`, so each A2A implementation chooses its own. The `X-Turul-` prefix makes the non-interop nature explicit: receivers expecting to switch between A2A server implementations without code changes MUST NOT rely on this header name. If the spec later standardizes a header name (e.g., `X-Turul-Push-Token`), we will migrate with a deprecation cycle.
- `Content-Type: application/a2a+json` (A2A v1.0.1 §4.3.3).
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
| Metrics / metric labels | The framework emits no push metrics (§9). An adopter adding them labels by `config_id`, never by credential. |
| Distributed traces | No secret in span attributes. |
| Panic output | `Debug` derive MUST skip secret fields (custom `Debug` impl or `#[debug(skip)]`). Panic context emitted by framework MUST redact. |
| Test snapshots / fixtures | Tests use sentinel secrets and assert they appear only in the outbound request headers, never in any other captured surface. |
| Delivery error messages | Error enum variants carry `config_id` and error class (§5b), NOT the raw credential or failure body. |
| Failed delivery records | Claim rows do NOT persist credentials, tokens, or request bodies (§5b). `config_id` suffices for cross-referencing. |
| Configuration dumps / introspection endpoints | If the server exposes config introspection (currently CRUD does), the wire format returns the credential verbatim per proto; that's the CRUD contract. But logs, metrics, and diagnostic dumps of the same config MUST redact. |

**Outbound HTTP**: the actual POST includes the credential in the `Authorization` header (that's the whole point). The framework installs no tracing subscriber and emits no tracing of outbound request headers; the credential is exposed from its `Secret` only where the header is built.

**Test guidance**: the redaction test (§13 test #16) installs a subscriber that records every tracing event and span field at every level, drives a delivery through retries and a giveup, and asserts no sentinel secret appears there, in `Debug` output, or in the failed-delivery record.

**Cross-reference**: failed deliveries (§5b) reference secrets by `config_id` rather than by credential. That pattern is load-bearing for operator inspection without secret exposure.

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
- 429 Too Many Requests → retry on the normal backoff schedule. A `Retry-After` header is not consulted (amended 2026-10-22; the original text promised it, the code never did).

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
- **Retention**: no backend prunes claim rows today. `push_failed_delivery_retention` (default 7 days) is accepted by the builder but nothing reads it (amended 2026-10-22).
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

Per-config delivery runs in its own tokio task, spawned by the dispatcher. One config's failure/slowness does not affect others. Each attempt builds its own `reqwest::Client` pinned to the IP that passed the SSRF check (R3 point 2); there is no shared client or connection pool.

**If a config repeatedly fails**: retry attempts per delivery are bounded (§5). There is no circuit-breaker in 0.1.x — every event triggers the full retry chain. Adopters whose webhooks are persistently down will see sustained background load, logged at WARN on each giveup. A future ADR can add a circuit-breaker (e.g., "skip delivery for this config for 10 minutes after N consecutive giveups") if needed.

**Dispatcher panic**: not supervised and not counted (amended 2026-10-22; the original sentinel, metric and respawn were never built). See the §1 amendment for what a panic leaves behind and how the reclaim loop recovers it.

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

- **Config deleted between event commit and delivery attempt**: worker reads the config from storage just before each POST attempt. If the config no longer exists, the delivery is abandoned cleanly. No error, no retry. This is how the framework honours A2A v1.0.1 §3.1.10: "No further notifications will be sent to the configured webhook after deletion." The dispatcher installs the pre-attempt lookup on its worker (`DeliveryTargetLookup`); a read error is not treated as deletion — the attempt proceeds and the next one checks again.
- **Config deleted mid-retry chain**: next retry tick sees the config gone, abandons. Claim marked `abandoned`.
- **Config added after events exist (backfill)**: no backfill. Configs capture events commit-timestamped after registration. This avoids weird catch-up storms when a client registers many configs against long-running tasks.
- **Task deleted while push pending**: same as config deletion — next check finds task missing (`get_task` returns None), delivery abandoned.
- **Orphaned task with push configs** (task in WORKING but no executor; see ADR-012 §5): events only fire on state changes. An orphaned non-terminal task with configs attached produces no deliveries until something forces a terminal (cancel, TTL-cleanup, admin action). This is correct — the config fires on changes, not on "still running."

### 9. Observability

> **Amended 2026-10-22 to what ships.** The original section specified delivery counters, latency histograms, claim-contention counters and a dispatcher-panic counter, plus INFO/WARN/ERROR logs on every attempt, retry and giveup. None of it was built, and adding metrics is a separate decision. What exists:

- **Failed-delivery records** (§5b) via `A2aPushDeliveryStore::list_failed_deliveries` — the operator surface for giveups.
- **Tracing events** on abnormal paths only, all keyed by `tenant` / `task_id` / `config_id` / `event_sequence`, never by secret: `turul_a2a::push_dispatch_config_list_failed` (ERROR), `turul_a2a::push_delivery_stuck` and `turul_a2a::push_redispatch_stuck` (WARN — POST done, terminal claim write not persisted), `turul_a2a::push_redispatch_read_error`, `turul_a2a::push_redispatch_pending_read_error`, `turul_a2a::push_pending_dispatch_refresh_failed` (WARN), `turul_a2a::push_non_https_url` (WARN, R3b), and the reclaim loop's `turul_a2a::push_claims_reclaimed`, `turul_a2a::push_pending_dispatches_stale`, `turul_a2a::push_sweep_error`, `turul_a2a::push_pending_sweep_error`.
- No event is emitted for a normal attempt, retry or giveup.

### 10. Storage Contract — New `A2aPushDeliveryStore` Trait

The existing `A2aPushNotificationStorage` (CRUD for configs) is insufficient for delivery coordination. A new trait handles claim bookkeeping.

> **Note 2026-10-22.** The sketch below is the original proposal. The trait as shipped (`crates/turul-a2a/src/push/claim.rs`) adds fencing (`claimant` + `generation` on every outcome write), `record_attempt_started`, pending-dispatch markers and `list_reclaimable_claims`, and uses enum-typed reasons (`GaveUpReason`, `AbandonedReason`) instead of strings. There is no `Failed` claim status; a non-retryable 4xx ends `GaveUp` with reason `NonRetryableHttpStatus`.

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

For each terminal event on `(tenant, task_id)`, the dispatcher lists the configs eligible for that event (`list_configs_eligible_at_event`, ADR-013 §4.5), paginating to the end. There is no cache: `push_config_cache_ttl` is accepted by the builder but nothing reads it (amended 2026-10-22). Only terminal events are dispatched, so the listing runs once per terminal commit.

If no configs are registered, the event is effectively dropped from the push perspective (SSE subscribers still receive it). This is normal and expected — most tasks don't have push configs.

### 12. Proto and Wire Compliance

- Delivery body: `StreamResponse` JSON `{"task": <Task>}` serialized via pbjson (camelCase JSON), per A2A §4.3.3.
- Wire headers: `Authorization`, `X-Turul-Push-Token` (if token set), `X-Turul-Event-Sequence`, `Content-Type: application/a2a+json`, `User-Agent`. The `X-Turul-*` headers are implementation-prefixed and not A2A-spec standardized; see §4 and R6 for scope.
- Delivery path is outbound; no changes to the A2A inbound wire surface.
- No new proto fields. All delivery coordination is storage-internal. Spec-compliant.

### 13. Required Tests Before GREEN

> **Amended 2026-10-22.** Each remaining test names the test that pins it. Tests #6 (429 `Retry-After`), #15 (dispatcher panic recovery) and #20 (claim-contention metrics) are dropped: their behaviour does not exist (§5, §6, §9). #22 (90-second receiver restart at default timings) is dropped as a wall-clock test; retry-until-success is #3. Metric and log assertions in the other tests are dropped with §9. `push_delivery_integration.rs` and `push_dispatcher_server_tests.rs` are in `crates/turul-a2a/tests/`.

1. **Basic delivery on terminal event** — `push_dispatcher_server_tests::{framework_cancel_triggers_push_delivery_with_canceled_state, executor_completion_triggers_push_delivery_with_completed_state}`; worker level `push_delivery_integration::happy_path_single_post_succeeds`. Exactly one POST; body is `{"task": …}` with exactly that one member and the terminal state; `Content-Type` is exactly `application/a2a+json`; `Authorization` is exactly `{scheme} {credentials}`; `X-Turul-Push-Token` is exactly the config token.

2. **Multiple configs fire independently** — `push_dispatcher_server_tests::each_config_receives_its_own_post_with_its_own_auth` (three configs, each one POST carrying its own credentials and token) and `dispatcher_paginates_through_all_push_configs` (every page of configs is walked).

3. **Retry on 5xx** — `push_delivery_integration::retry_on_5xx_then_success`: 503 twice then 200 → exactly 3 POSTs, `Succeeded`, no failed-delivery record.

4. **Giveup after max attempts** — `push_delivery_integration::giveup_after_max_attempts_on_sustained_5xx`: exactly `max_attempts` POSTs, `GaveUp(MaxAttemptsExhausted)`, record carries the last status and class.

5. **No retry on 4xx (non-408/429)** — `push_delivery_integration::no_retry_on_4xx`: exactly 1 POST, `GaveUp(NonRetryableHttpStatus)`, class `HttpError4xx { status: 400 }`. 429 is retried: `retry_on_429_then_success`.

7. **Config deleted mid-retry abandons** — `push_delivery_integration::config_deleted_mid_retry_abandons_without_another_post`: 500 on the first POST, config deleted, no further POST, `Abandoned(ConfigDeleted)`, not listed as failed, not reclaimable, cannot be re-claimed.

8. **Cross-instance claim dedup** — `push_delivery_integration::two_instances_sharing_storage_post_exactly_once`: two workers on one store deliver the same event concurrently → exactly one POST; one `Succeeded`, the other `ClaimLostOrFinal`.

9. **Claim expiry re-picks up abandoned work** — `push_delivery_integration::expired_claim_is_resumed_by_another_instance_within_the_budget`: instance A claims and starts one POST, then dies; after expiry instance B re-claims and makes exactly `max_attempts - 1` POSTs; the final record counts `max_attempts`.

10. **Task deletion between event and retry abandons** — `push_delivery_integration::task_deleted_mid_retry_abandons_without_another_post`: as #7, `Abandoned(TaskDeleted)`.

11. **Artifact events are NOT delivered** — `push_dispatcher_server_tests::inline_push_config_on_{blocking,streaming}_send_delivers_one_terminal_post`: three artifact events and a terminal → exactly one POST, whose task carries the three artifacts. Unit level: `push::dispatcher::tests::artifact_events_are_not_dispatched`.

12. **Authentication variants** — Bearer in #1; `push_delivery_integration::basic_and_custom_auth_schemes_are_sent_verbatim` (exact `Basic …` and `ApiKey …` values); no scheme → no header, `unauthenticated_config_sends_no_authorization_header`.

13. **Framework-committed CANCELED triggers delivery** — `push_dispatcher_server_tests::framework_cancel_triggers_push_delivery_with_canceled_state`.

14. **Push config CRUD parity** — unchanged CRUD tests.

16. **Secret redaction coverage** — `push_delivery_integration::secret_sentinel_never_leaks_on_giveup`: every tracing event and span field at every level, `Debug` output and the failed-delivery record are free of the sentinel credential and token, across retries and a giveup.

17. **SSRF blocklist** — `push_delivery_integration::ssrf_blocked_destinations_are_never_posted`: https URLs resolving to `127.0.0.1`, `10.0.0.5`, `169.254.169.254`, `::1`, `::` with insecure URLs disallowed → `GaveUp(SsrfBlocked)`, class `SSRFBlocked`, attempt count 0 (no POST). Range coverage: `push::ssrf::tests`.

18. **DNS rebinding defense** — `push_delivery_integration::reqwest_connects_to_validated_ip_not_system_dns`: the POST connects to the validated IP for an unresolvable hostname, one resolution per attempt.

19. **Outbound allowlist hook** — `push_delivery_integration::outbound_allowlist_refuses_before_dns`: refused host → `GaveUp(SsrfBlocked)`, never resolved, no POST.

21. **At-least-once redelivery after claim-then-crash** — `push_delivery_integration::reclaim_sweep_redispatches_after_persistent_terminal_failure`: POST succeeded, terminal write lost; after expiry the reclaim path re-POSTs (2 POSTs total) and the row ends terminal.

23. **Failed delivery record is inspectable and secret-free** — `push_delivery_integration::giveup_after_max_attempts_on_sustained_5xx` (task, config, sequence, attempt count, last status, class, ordered timestamps) with #16 for secrets.

24. **Non-https URL in production** — `push::delivery::tests::non_https_url_in_production_is_abandoned_without_post` (R3b).

### 14. What Changes in Existing Code

> **Note 2026-10-22.** This section is the original plan. Where it differs from what ships, §1, §6, §9, §11 and §13 as amended are authoritative: per-attempt HTTP clients, no subscribing worker, no config cache, tests in `push_delivery_integration.rs` / `push_dispatcher_server_tests.rs`. `push_config_cache_ttl`, `push_failed_delivery_retention` and `push_read_timeout` are accepted by the builder and have no effect.

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

**R1 — Webhook receiver is slow / flaky, generates sustained retry load**. Bounded by `push_max_attempts` (8 default; ~3min total horizon). Each giveup leaves a failed-delivery record (§5b); there are no metrics (§9). Future ADR can add circuit-breaker. For 0.1.x, a consistently-failing webhook just produces up to 8 failed POSTs per event and gives up. Claim rows are not pruned (§5b).

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

6. **Outbound allowlist hook** (optional): `OutboundUrlValidator = Arc<dyn Fn(&url::Url) -> Result<(), String> + Send + Sync>`, the third argument of `PushDeliveryWorker::new`. Called before DNS resolution — a refused host is never resolved. Returning `Err` gives up the delivery with error class `SSRFBlocked`; the reason string is not recorded (the class is a fixed enum). As shipped, `A2aServerBuilder` and `LambdaA2aServerBuilder` construct their worker with no validator; only a directly constructed worker can use the hook (amended 2026-10-22).

7. **`allow_insecure_push_urls`** remains the dev-only flag (default `false`, §R3b). When set `true`, ALSO disables the private-IP blocklist — otherwise `http://localhost:8080` tests would fail the new blocklist. The flag is documented as mutually-exclusive with production use. No startup warning is emitted when it is set (amended 2026-10-22; the release-build WARN was never built).

**R3b — TLS requirement for delivery URLs**. A2A spec §7.1 mandates TLS 1.2+ for A2A transport. Push delivery is an outbound A2A-adjacent HTTP flow. Policy:
- **Production**: the framework enforces `https://` at delivery time. A config registered with `http://` succeeds at CRUD (no validation at registration to keep CRUD simple and backend-portable) but the delivery worker refuses to POST, logs WARN (`turul_a2a::push_non_https_url`, by `config_id` — the URL may carry secrets), and records the claim as `Abandoned { reason: NonHttpsUrlInProduction }`. Abandoned rows are not failed deliveries, so `list_failed_deliveries` does not list it. Pinned by `push::delivery::tests::non_https_url_in_production_is_abandoned_without_post`.
- **Development / test**: `allow_insecure_push_urls` (see R3 point 7) permits `http://` delivery for localhost testing. Must be set explicitly per-deployment.
- No automatic cert pinning, no mTLS delivery in 0.1.x. The reqwest default (rustls + system roots) handles standard TLS.

**R4 — Outbound redirects hit internal endpoints (open redirect into SSRF)**. Mitigated: delivery does NOT follow redirects (§5). Non-2xx/3xx are errors; 3xx treated as opaque success per §5. Webhooks that legitimately 301-redirect will receive a single POST and a logged "unexpected 3xx" info — this is intentional.

**R5 — Payload size for large tasks exceeds limits**. A task with hundreds of history messages could produce a multi-megabyte POST body. Mitigation: `push_max_payload_bytes` limit (default 1 MiB), configurable. If the serialized body exceeds the limit, the delivery is given up without a POST: `GaveUp { reason: PayloadTooLarge }`, error class `PayloadTooLarge`, listed by `list_failed_deliveries` (amended 2026-10-22; the original text said Abandoned with a WARN). Future optimization: deliver a compact "task_id + state + version" envelope instead of the full Task — requires spec alignment.

**R6 — Receiver verifies `token` but config's token field was never persisted**. Per proto, `token` is optional. If absent, `X-Turul-Push-Token` header is not sent. Receivers that require the token must validate config registration accordingly — not the framework's concern.

**R7 — Dispatcher is a single point of failure on each instance**. There is no long-lived dispatcher task to fail: each terminal commit spawns its own fan-out, and the reclaim loop recovers markers and claims a failed fan-out leaves behind (§1 amendment). Future scaling: sharded dispatch. Deferred.

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

## Revision log

- 2026-10-22 — Proposed: §2 corrected to the terminal-only delivery the code has always had (it said all status updates). §3/§12/§13: body becomes the A2A v1.0.1 §4.3.3 `StreamResponse` envelope `{"task": <Task>}` with `Content-Type: application/a2a+json`, instead of a bare `Task`. `X-Turul-Push-Token` unchanged: the spec names no token header.
- 2026-10-22 — Amended to what ships (maintainer decision), §2/§3 accepted and implemented. Removed promises with no code: §9 metrics and per-attempt logs (replaced by the tracing targets that exist), 429 `Retry-After` (§5), dispatcher panic sentinel/metric/respawn (§1, §6, R7), release-build WARN for `allow_insecure_push_urls` (R3.7), shared `reqwest::Client` (§6), config cache and claim retention (§5b, §11). Corrected: R5 payload-too-large is `GaveUp(PayloadTooLarge)`; R3.6 the allowlist hook runs before DNS and is not exposed by the server builders. Made true in code: §8 pre-attempt config/task re-read (spec §3.1.10), R3b non-https → `Abandoned(NonHttpsUrlInProduction)`, allowlist before DNS. §13 rewritten to the tests that pin shipped behaviour; #6, #15, #20, #22 dropped.
