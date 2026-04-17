# ADR-011: Push Notification Delivery

- **Status:** Proposed
- **Date:** 2026-04-18
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

### 5. Retry Policy — Exponential Backoff with Jitter

Per-delivery retry schedule (default; deployment-configurable):

| Attempt | Delay before attempt | Total elapsed |
|---|---|---|
| 1 | 0 (immediate) | 0s |
| 2 | 1s ± 0.5s jitter | ~1.5s |
| 3 | 2s ± 1s jitter | ~3.5s |
| 4 | 4s ± 2s jitter | ~7.5s |
| 5 | 8s ± 4s jitter | ~15.5s |

Max 5 attempts. Beyond that, the delivery is given up. Timing is approximate; jitter prevents thundering herd when many failed webhooks recover simultaneously.

**What triggers a retry**:
- Network error (connection refused, timeout, DNS failure) → retry.
- 5xx response → retry.
- 408 Request Timeout → retry.
- 429 Too Many Requests → retry (with `Retry-After` header respected if present, capped at the maximum backoff).

**What does not retry**:
- 2xx response → success, stop.
- 3xx → treated as success (some webhooks redirect to success pages). Do NOT follow redirects automatically (avoid SSRF).
- 4xx other than 408/429 → permanent failure, no retry. The receiver explicitly rejected the delivery; retrying is wasteful.

**Per-request timeout**: 30 seconds (configurable via builder). Hard cap on how long a single POST can take regardless of server response behavior.

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

**Claim expiry**: if a claim is older than `push_claim_expiry` (default 5 minutes) and still in `pending` or `attempting` status, another instance MAY re-claim and retry. This handles the case where the original claimant crashed mid-retry. The new claimant must increment `attempt_count`; when it exceeds `max_attempts`, the delivery is given up.

**Terminal delivery guarantee** under worker crash: if instance A claims and crashes between attempt 2 and 3, after claim expiry instance B picks up at attempt 3 and continues. Total attempts across the cluster are bounded by `max_attempts`, not `max_attempts × instance_count`. This relies on claim expiry being longer than the full retry window (16s typical) but shorter than "clearly abandoned" (5 minutes). Both configurable.

**Same-instance worker within its own retry loop** checks the claim is still owned by self before each retry. If another instance re-claimed (e.g., this instance's own heartbeat lapsed — unlikely but possible under GC pauses), this instance abandons and does not double-send.

### 8. Config Lifecycle Edge Cases

- **Config deleted between event commit and delivery attempt**: worker reads the config from storage just before each POST attempt. If the config no longer exists, the delivery is abandoned cleanly. No error, no retry.
- **Config deleted mid-retry chain**: next retry tick sees the config gone, abandons. Claim marked `abandoned`.
- **Config added after events exist (backfill)**: no backfill. Configs capture events commit-timestamped after registration. This avoids weird catch-up storms when a client registers many configs against long-running tasks.
- **Task deleted while push pending**: same as config deletion — next check finds task missing (`get_task` returns None), delivery abandoned.
- **Orphaned task with push configs** (task in WORKING but no executor; see ADR-012 §5): events only fire on state changes. An orphaned non-terminal task with configs attached produces no deliveries until something forces a terminal (cancel, TTL-cleanup, admin action). This is correct — the config fires on changes, not on "still running."

### 9. Observability

Metrics (emitted via tracing or structured logs for 0.1.x; adopters wire up their own metric backend):
- `framework.push_deliveries_attempted` — counter, labeled by `tenant` and `config_id`.
- `framework.push_deliveries_succeeded` — counter.
- `framework.push_deliveries_failed` — counter, labeled by failure class (network / 4xx / 5xx / timeout).
- `framework.push_deliveries_gave_up` — counter, final-failure after max attempts.
- `framework.push_dispatcher_panics` — counter, per §6.
- `framework.push_delivery_latency` — histogram, per attempt.

Structured logs on every attempt at INFO, on every retry at WARN, on every giveup at ERROR. Always include `task_id`, `tenant`, `config_id`, `event_sequence`, `attempt`.

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
   Wiremock responds 500 forever. Assert: exactly `max_attempts` (5 default) POSTs occur across the full retry schedule (~15.5s total), then no more. Claim ends in `GaveUp` state. ERROR log emitted. `gave_up` metric incremented.

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

### 14. What Changes in Existing Code

**New**:
- `crates/turul-a2a/src/push/delivery.rs`: `PushDeliveryWorker`, per-config delivery task, retry loop. Uses shared `reqwest::Client`.
- `crates/turul-a2a/src/push/claim.rs`: claim abstractions, `A2aPushDeliveryStore` trait, `DeliveryClaim`, `ClaimStatus`, `DeliveryOutcome`.
- `crates/turul-a2a/src/storage/{memory,sqlite,postgres,dynamodb}.rs`: each backend implements `A2aPushDeliveryStore`.
- `crates/turul-a2a/src/storage/parity_tests.rs`: parity tests for `claim_delivery`, `record_delivery_outcome`, `sweep_expired_claims`.
- `crates/turul-a2a/src/server/builder.rs`: new config fields `push_max_attempts`, `push_backoff_base`, `push_request_timeout`, `push_claim_expiry`, `push_config_cache_ttl`.
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

**R1 — Webhook receiver is slow / flaky, generates sustained retry load**. Bounded by max-attempts (5). Sustained load is logged and metric'd (§9). Future ADR can add circuit-breaker. For 0.1.x, a consistently-failing webhook just produces 5 failed deliveries per event and gives up; storage load from claim rows is bounded by TTL.

**R2 — Cross-instance claim race starves slow-winner**. Mitigated by claim expiry (§7): if the winner takes too long, another instance re-claims. Bounded by `push_claim_expiry` (default 5m). Tune down for shorter SLAs.

**R3 — SSRF via webhook URL**. Adopter's responsibility: validate URLs at config-registration time (e.g., reject private network ranges). No automatic blocklist in 0.1.x — keeping it out of framework scope because threat model is deployment-specific. Documented in the push-config handler's docstring.

**R3b — TLS requirement for delivery URLs**. A2A spec §7.1 mandates TLS 1.2+ for A2A transport. Push delivery is an outbound A2A-adjacent HTTP flow. Policy:
- **Production**: the framework enforces `https://` at delivery time. A config registered with `http://` succeeds at CRUD (no validation at registration to keep CRUD simple and backend-portable) but the delivery worker refuses to POST, logs WARN, and records the claim as `Abandoned { reason: "non-https URL in production mode" }`.
- **Development / test**: the builder exposes `allow_insecure_push_urls: bool` (default `false`). Setting true permits `http://` delivery for localhost testing with wiremock etc. This flag must be set explicitly per-deployment.
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
- Retry policy is fixed in 0.1.x (exponential backoff, 5 attempts). No circuit-breaker. Deployments with persistently failing webhooks will generate sustained retry traffic until the config is deleted.

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
