//! Delivery coordination trait and types (ADR-011 §10).
//!
//! [`A2aPushDeliveryStore`] is the contract every backend must
//! implement so a multi-instance deployment can share the "who is
//! delivering which event to which config" state without double-posting.
//!
//! # Claim model
//!
//! Deliveries are coordinated by an atomic **claim** on the tuple
//! `(tenant, task_id, event_sequence, config_id)`. The first writer
//! wins; later writers observe
//! [`A2aStorageError::ClaimAlreadyHeld`]. A claim carries a
//! `claimant` identifier (typically the server instance id), a
//! monotonic `generation` fencing token, a `claimed_at` timestamp, a
//! `delivery_attempt_count` (cluster-wide HTTP POST attempts —
//! advanced only by [`A2aPushDeliveryStore::record_attempt_started`],
//! never by `claim_delivery`), and a [`ClaimStatus`].
//!
//! Claims are **expirable**: a claim whose `claimed_at +
//! claim_expiry` has passed and whose status is still `Pending` or
//! `Attempting` is eligible for re-claim by any other instance. On
//! re-claim, `generation` increments and `claimant` is replaced;
//! `delivery_attempt_count` is **not** changed by re-claim (a worker
//! that crashed before POSTing must not be billed against the budget).
//! This bounds total POST attempts to `push_max_attempts` even across
//! worker crashes: the new claimant picks up at whatever count the
//! prior claimant left on the row, then advances it only when it
//! actually starts a POST.
//!
//! Claims with terminal status (`Succeeded`, `GaveUp`, `Abandoned`)
//! are **not** re-claimable. `claim_delivery` MUST return
//! `ClaimAlreadyHeld` against such rows regardless of how long ago
//! they were written. This is load-bearing for at-least-once
//! semantics: once a receiver has confirmed the POST, the framework
//! does not re-POST.
//!
//! # Fencing — outcome recording is pinned to the claim identity
//!
//! [`A2aPushDeliveryStore::record_delivery_outcome`] takes the
//! `claimant` and `claim_generation` that were returned by the
//! original `claim_delivery`. The store compares them against the
//! currently-stored claim identity and returns
//! [`A2aStorageError::StaleDeliveryClaim`] if they do not match.
//!
//! This closes the stalled-worker race: if worker A claims, GC-pauses
//! past expiry, worker B re-claims with `generation = g+1`, and A
//! then wakes and attempts to record an outcome against `generation =
//! g`, the store rejects A's write. A terminal state committed by
//! B cannot be overwritten by a stale A, and A's retry loop aborts on
//! the `StaleDeliveryClaim` error.
//!
//! # Outcome model
//!
//! A worker reports progress via
//! [`A2aPushDeliveryStore::record_delivery_outcome`] with one of the
//! four variants of [`DeliveryOutcome`]. There is no separate
//! `release_claim` method — reporting the outcome IS the release,
//! and the outcome variant determines whether the claim freezes
//! (`Succeeded`, `GaveUp`, `Abandoned`) or stays open for the next
//! retry (`Retry`).
//!
//! # Secret redaction (ADR-011 §4a)
//!
//! Neither this trait surface nor its persisted row shape carries
//! credentials, tokens, request bodies, or receiver response bodies.
//! Failure diagnostics flow through [`DeliveryErrorClass`] — an enum
//! of classifications, not free-text strings. Terminal reason fields
//! on `GaveUp` and `Abandoned` are enum-typed
//! ([`GaveUpReason`], [`AbandonedReason`]), not free-text; the trait
//! surface offers no slot into which a worker could accidentally
//! thread variable user data. `config_id` is the stable handle
//! operators use to cross-reference a failure back to the push
//! config CRUD row.
//!
//! # Observability
//!
//! `claim_delivery` is called once per (event, config) pair. Callers
//! are expected to emit `framework.push_claim_attempts` /
//! `_conflicts` tracing events around the call (see ADR-011 §9). The
//! store does not emit telemetry itself — it only provides the
//! race-free primitive.
//!
//! # Expiry sweep
//!
//! [`A2aPushDeliveryStore::sweep_expired_claims`] runs on a
//! deployment-configured cadence. It reopens `Pending` / `Attempting`
//! claims whose expiry has passed — not `Succeeded` / `GaveUp` /
//! `Abandoned` — and returns the count for operator visibility.
//! DynamoDB implementations may defer reopening to the next
//! `claim_delivery` call's ConditionExpression; such a lazy sweep
//! returns 0.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use crate::storage::A2aStorageError;

/// Delivery-coordination storage. Separate from
/// [`crate::storage::A2aPushNotificationStorage`] (config CRUD).
/// Implemented alongside the other storage traits by the same
/// backend struct; same-backend enforcement in the server builder
/// includes this trait when a `PushDeliveryWorker` is configured.
#[async_trait]
pub trait A2aPushDeliveryStore: Send + Sync {
    /// Backend identifier used by the server builder's same-backend
    /// enforcement. Matches [`crate::storage::A2aTaskStorage::backend_name`]
    /// on the same backend struct.
    fn backend_name(&self) -> &'static str;

    /// Atomically claim delivery of `event_sequence` to `config_id`.
    ///
    /// Returns:
    /// - `Ok(DeliveryClaim)` on the first claim for this tuple, with
    ///   `generation = 1`, `delivery_attempt_count = 0`,
    ///   `status = Pending`, and `claimant` / `claimed_at` set from
    ///   the arguments.
    /// - `Ok(DeliveryClaim)` on a re-claim after expiry, with
    ///   `generation` incremented by 1, `claimant` replaced,
    ///   `claimed_at` refreshed, `delivery_attempt_count`
    ///   **unchanged** from whatever value the prior claimant left
    ///   on the row, and `status` reset to `Pending`.
    /// - `Err(A2aStorageError::ClaimAlreadyHeld)` if a prior claim
    ///   exists and either (a) is still within its expiry with
    ///   `Pending` / `Attempting` status or (b) has terminal status
    ///   (`Succeeded` / `GaveUp` / `Abandoned`).
    ///
    /// **Claim does not consume budget.** `delivery_attempt_count`
    /// is advanced only by [`Self::record_attempt_started`], which
    /// workers call immediately before a POST. A worker that
    /// crashes between claim and record-started contributes zero
    /// to the budget; the next claimant resumes from whatever count
    /// is on the row.
    ///
    /// `claim_expiry` is passed per-call so deployments can tune it
    /// without reconstructing the store. The worker's retry horizon
    /// (derived from `push_max_attempts` + `push_backoff_cap`) MUST
    /// be shorter than `claim_expiry` so live retries never race
    /// re-claim; the server builder validates this constraint.
    // `claim_delivery` takes 8 args because it's the canonical
    // primary-key + identity + expiry tuple the atomic claim table
    // needs. Bundling into a struct would force every call site to
    // repeat the builder boilerplate with no real readability gain
    // — the arguments are the PK.
    #[allow(clippy::too_many_arguments)]
    async fn claim_delivery(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        config_id: &str,
        claimant: &str,
        owner: &str,
        claim_expiry: Duration,
    ) -> Result<DeliveryClaim, A2aStorageError>;

    /// Record that an HTTP POST attempt is about to be made.
    ///
    /// This is the sole place `delivery_attempt_count` advances. The
    /// worker calls it immediately before the POST, after any
    /// pre-flight checks (SSRF guard, payload size, config
    /// freshness) have passed. Fenced on `(claimant,
    /// claim_generation)` identically to
    /// [`Self::record_delivery_outcome`]: a stale caller receives
    /// [`A2aStorageError::StaleDeliveryClaim`] and MUST abort its
    /// retry loop.
    ///
    /// On success:
    /// - `delivery_attempt_count` advances by 1.
    /// - `status` transitions to `Attempting` (if it was `Pending`
    ///   or already `Attempting`).
    /// - Returns the new `delivery_attempt_count`.
    ///
    /// On stale claim: no row mutation; returns
    /// `StaleDeliveryClaim`.
    ///
    /// **Budget-check contract (precise)**: the worker MUST check
    /// `claim.delivery_attempt_count < push_max_attempts` BEFORE
    /// calling this method, and skip the call (immediately recording
    /// `DeliveryOutcome::GaveUp { MaxAttemptsExhausted, … }`) when
    /// the count has reached the ceiling. This method increments
    /// by one and returns the new count, so on a successful call
    /// the returned value satisfies `1 <= new_count <= push_max_attempts`.
    /// The store does not itself enforce the ceiling — it has no
    /// knowledge of `push_max_attempts`. Workers that call this
    /// method twice with the same `(claimant, claim_generation)`
    /// without an intervening outcome will have billed two POST
    /// attempts against the budget; that is the correct accounting
    /// for a dispatcher that actually issued two POSTs, and a bug
    /// for a dispatcher that issued one.
    ///
    /// **Terminal rows are frozen.** If the row has already reached
    /// a terminal status (`Succeeded`, `GaveUp`, or `Abandoned`),
    /// this method returns `StaleDeliveryClaim` without mutating
    /// the counter or status — the worker's retry loop must abort.
    /// This is the same signal a genuinely stale claimant receives,
    /// which is appropriate: in both cases the worker no longer
    /// owns an active attempt-start opportunity.
    async fn record_attempt_started(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        config_id: &str,
        claimant: &str,
        claim_generation: u64,
    ) -> Result<u32, A2aStorageError>;

    /// Record the outcome of the latest delivery attempt on this
    /// claim.
    ///
    /// **Fencing**: the `claimant` and `claim_generation` arguments
    /// MUST match the stored claim's current `claimant` and
    /// `generation`. On mismatch the store returns
    /// [`A2aStorageError::StaleDeliveryClaim`] and does not mutate
    /// the row. This is the guarantee that protects a re-claimant's
    /// terminal commit from being overwritten by a stale prior
    /// claimant's outcome.
    ///
    /// On matching identity:
    /// - `Succeeded { http_status }` → status transitions to
    ///   `Succeeded`; subsequent `claim_delivery` on this tuple
    ///   returns `ClaimAlreadyHeld` regardless of expiry. Idempotent:
    ///   a second `Succeeded` call by the same claimant on the same
    ///   generation is a no-op.
    /// - `Retry { next_attempt_at, http_status, error_class }` →
    ///   status stays `Attempting`; `last_http_status` and
    ///   `last_error_class` update. `delivery_attempt_count` is
    ///   **not** changed (it was advanced by the preceding
    ///   `record_attempt_started`). The caller schedules the retry
    ///   at `next_attempt_at`; the store does not dispatch retries.
    /// - `GaveUp { reason, last_error_class, last_http_status }` →
    ///   status transitions to `GaveUp`; the failure record is
    ///   retained per `push_failed_delivery_retention`.
    ///   `list_failed_deliveries` surfaces it.
    /// - `Abandoned { reason }` → status transitions to `Abandoned`;
    ///   the row is retained briefly for audit. `list_failed_deliveries`
    ///   does NOT surface `Abandoned` rows — nothing failed on the
    ///   delivery side, the surrounding context (config / task / URL
    ///   policy) caused the abandon.
    ///
    /// **Terminal rows are frozen.** Once any terminal status
    /// (`Succeeded`, `GaveUp`, `Abandoned`) has been recorded on
    /// this tuple, subsequent `record_delivery_outcome` calls —
    /// whether the same terminal (idempotency) or a different
    /// outcome (cross-over) — return `Ok(())` without mutating the
    /// row. The first terminal wins and the row is never rolled
    /// back. This protects against:
    /// - dispatcher double-dispatch emitting the same outcome twice,
    /// - a worker that confuses its own local state and tries to
    ///   commit a different terminal than the one it already
    ///   persisted, and
    /// - any retry-loop bug that would otherwise overwrite a
    ///   terminal state with a non-terminal `Retry`.
    ///
    /// `Retry` against a non-terminal row (the normal case) keeps
    /// the claim open; `Retry` against a terminal row is also a
    /// silent no-op.
    #[allow(clippy::too_many_arguments)]
    async fn record_delivery_outcome(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        config_id: &str,
        claimant: &str,
        claim_generation: u64,
        outcome: DeliveryOutcome,
    ) -> Result<(), A2aStorageError>;

    /// Maintenance / observability: report expired-but-not-terminal
    /// claim rows.
    ///
    /// **Re-claimability is derived, not stored.** A claim row is
    /// eligible for re-claim when both conditions hold at query time:
    /// - `claimed_at + claim_expiry < now` (where `claim_expiry` is
    ///   the duration passed to the most recent `claim_delivery`
    ///   that produced this row), AND
    /// - `status ∈ {Pending, Attempting}`.
    ///
    /// No dedicated `Reclaimable` / `Expired` status variant exists;
    /// `claim_delivery` itself checks the derivation atomically on
    /// its next call, so re-claim happens without a prior sweep.
    /// This keeps backends from diverging on whether "expired" is a
    /// stored status or a computed property.
    ///
    /// Returns the count of claim rows that currently satisfy the
    /// derivation above. Intended for periodic metric emission
    /// (`framework.push_claims_expired_gauge` and similar) so
    /// operators can alert on stuck-claim accumulation. Backends
    /// MAY piggy-back retention cleanup of terminal claims past
    /// `push_failed_delivery_retention` as a side effect of this
    /// call, or handle retention via native mechanisms (e.g.,
    /// DynamoDB TTL); the return value is the eligibility count
    /// only, not a cleanup count. Terminal-status rows are never
    /// counted regardless of age.
    async fn sweep_expired_claims(&self) -> Result<u64, A2aStorageError>;

    /// Enumerate expired-but-non-terminal claim rows so the server's
    /// reclaim-and-redispatch loop can retry stuck deliveries.
    ///
    /// A row is returned when, at query time:
    /// - `claimed_at + claim_expiry < now` (expired relative to the
    ///   `claim_expiry` that was in effect for the most recent
    ///   claim), AND
    /// - `status ∈ {Pending, Attempting}` (non-terminal).
    ///
    /// `Succeeded` / `GaveUp` / `Abandoned` rows are never returned.
    ///
    /// Results are returned in any order; the framework sweeper
    /// bounds the workload by passing a `limit` and iterating tick
    /// by tick until the load catches up. Backends MAY return fewer
    /// than `limit` rows even when more are available (e.g., a
    /// DynamoDB scan page boundary) — callers treat any row count
    /// below `limit` as the normal stopping signal for this tick.
    ///
    /// The returned [`ReclaimableClaim`] carries exactly the key
    /// identifiers plus `owner` so the sweeper can call
    /// `A2aTaskStorage::get_task` and `A2aPushNotificationStorage::get_config`
    /// to reassemble the delivery target without opening an
    /// unscoped read path on the task store.
    async fn list_reclaimable_claims(
        &self,
        limit: usize,
    ) -> Result<Vec<ReclaimableClaim>, A2aStorageError>;

    /// Record a durable "this terminal event needs fan-out" marker.
    ///
    /// The dispatcher writes a marker for `(tenant, task_id,
    /// event_sequence)` BEFORE calling `list_configs`. If
    /// `list_configs` fails persistently, the marker is the only
    /// evidence the event ever wanted to be dispatched — claim
    /// rows haven't been created yet, so `list_reclaimable_claims`
    /// has nothing to return. The server's reclaim loop enumerates
    /// stale markers via [`Self::list_stale_pending_dispatches`]
    /// and invokes a redispatch that re-runs `list_configs` +
    /// fan-out.
    ///
    /// Idempotent: writing a marker for a tuple that already has
    /// one is a no-op (or refreshes `recorded_at` — backends choose,
    /// as long as repeated calls do not fail). The dispatcher may
    /// call this on every dispatch attempt for defence in depth.
    async fn record_pending_dispatch(
        &self,
        tenant: &str,
        owner: &str,
        task_id: &str,
        event_sequence: u64,
    ) -> Result<(), A2aStorageError>;

    /// Delete a pending-dispatch marker after fan-out completes.
    ///
    /// Called once all per-config deliveries have had a chance to
    /// create their claim rows — whether each POST succeeded or
    /// not. After deletion, the event's recovery is handled by the
    /// claim-row reclaim path; the marker's job of ensuring
    /// *something* survives `list_configs` failure is done.
    ///
    /// Idempotent: deleting a non-existent marker is a no-op.
    async fn delete_pending_dispatch(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
    ) -> Result<(), A2aStorageError>;

    /// Enumerate pending-dispatch markers whose `recorded_at` is
    /// older than the given threshold.
    ///
    /// The threshold MUST exceed the dispatcher's bounded
    /// `list_configs` retry horizon (typically set to
    /// `push_claim_expiry`) so in-progress dispatches don't get
    /// prematurely "redispatched". The server's reclaim sweep
    /// uses this to recover events whose initial fan-out died
    /// before producing any claim rows.
    async fn list_stale_pending_dispatches(
        &self,
        older_than_recorded_at: SystemTime,
        limit: usize,
    ) -> Result<Vec<PendingDispatch>, A2aStorageError>;

    /// Operator inspection: list failed deliveries for a tenant.
    ///
    /// Returns rows with status `GaveUp` whose `gave_up_at >= since`,
    /// ordered newest-first, capped at `limit`. Rows with status
    /// `Succeeded`, `Abandoned`, `Pending`, or `Attempting` are
    /// excluded.
    ///
    /// Records do NOT carry credentials, tokens, or receiver
    /// response bodies — [`FailedDelivery`] has no fields for those
    /// values. Adopters use this to surface failed-delivery state
    /// via their own admin APIs; the framework does not expose an
    /// HTTP endpoint for inspection.
    async fn list_failed_deliveries(
        &self,
        tenant: &str,
        since: SystemTime,
        limit: usize,
    ) -> Result<Vec<FailedDelivery>, A2aStorageError>;
}

/// Snapshot of a claim row as returned by
/// [`A2aPushDeliveryStore::claim_delivery`].
///
/// `#[non_exhaustive]` so additional observability fields can land
/// in patch releases without breaking callers. Workers read
/// `claimant` and `generation` from this struct and pass them back
/// verbatim to [`A2aPushDeliveryStore::record_delivery_outcome`]
/// as the fencing token.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DeliveryClaim {
    /// Instance identifier that currently holds the claim. Passed
    /// back on `record_delivery_outcome` as one half of the fencing
    /// identity.
    pub claimant: String,

    /// Owner string recorded on the claim row. Kept here (rather than
    /// looked up via the push-config row) so the reclaim sweep path
    /// can call `A2aTaskStorage::get_task` without introducing a new
    /// unscoped read method. Persisted from the first
    /// `claim_delivery` call and not changed on re-claim.
    pub owner: String,

    /// Monotonic per-tuple fencing token. Starts at `1` on the first
    /// successful claim; increments by `1` on each re-claim after
    /// expiry. Passed back on `record_delivery_outcome` as the
    /// second half of the fencing identity. A worker whose
    /// `(claimant, generation)` pair no longer matches the stored
    /// row receives `StaleDeliveryClaim` on its outcome write.
    pub generation: u64,

    /// When the claim was acquired (or re-acquired). Combined with
    /// the `claim_expiry` argument passed to `claim_delivery`,
    /// determines re-claim eligibility.
    pub claimed_at: SystemTime,

    /// Cluster-wide count of HTTP POST attempts for this tuple.
    /// `0` on the first claim before any POST is started. Advanced
    /// ONLY by [`A2aPushDeliveryStore::record_attempt_started`];
    /// `claim_delivery` (initial and re-claim) and
    /// `record_delivery_outcome` do not change it. Workers compare
    /// this against `push_max_attempts` to decide whether to start
    /// another POST or go straight to `GaveUp`.
    pub delivery_attempt_count: u32,

    /// Current status. New and re-claimed claims start `Pending`;
    /// workers transition to `Attempting` on the first POST.
    pub status: ClaimStatus,
}

/// Lifecycle states of a delivery claim.
///
/// Five states, by design:
/// - `Pending` — claim acquired; no POST attempted yet.
/// - `Attempting` — POST in flight OR a retry is scheduled. The
///   store does not distinguish "POST in flight" from "retry
///   scheduled" — those are worker-local states.
/// - `Succeeded` — delivery confirmed (2xx / 3xx).
/// - `GaveUp` — final failure after `push_max_attempts` OR a
///   pre-POST rejection that operators should see
///   (`SsrfBlocked`, `PayloadTooLarge`, `TlsRejected`). Row
///   retained for operator inspection per
///   `push_failed_delivery_retention`.
/// - `Abandoned` — terminal but not failed (config deleted, task
///   deleted, non-HTTPS URL in production). Row retained briefly
///   for audit; not surfaced by `list_failed_deliveries`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClaimStatus {
    Pending,
    Attempting,
    Succeeded,
    GaveUp,
    Abandoned,
}

/// Outcome reported via [`A2aPushDeliveryStore::record_delivery_outcome`].
///
/// Secret-safety invariant (ADR-011 §4a): no variant carries
/// free-text derived from user input, receiver responses, or
/// credential material. Retry diagnostics use
/// [`DeliveryErrorClass`]; terminal reasons use framework-owned
/// enums ([`GaveUpReason`], [`AbandonedReason`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DeliveryOutcome {
    /// Delivery confirmed. `http_status` is typically 2xx or 3xx
    /// (3xx is treated as success per ADR-011 §5; redirects are NOT
    /// followed).
    Succeeded { http_status: u16 },

    /// Transient failure; retry scheduled at `next_attempt_at`. Only
    /// updates diagnostics (`last_http_status`, `last_error_class`);
    /// does NOT change `delivery_attempt_count` — that was already
    /// advanced by [`A2aPushDeliveryStore::record_attempt_started`]
    /// before the POST.
    ///
    /// `http_status` is present on HTTP-error retries (429, 5xx,
    /// 408) and absent on pre-HTTP failures (DNS, connect, SSRF,
    /// TLS). `error_class` is the classified failure reason —
    /// enum-only by construction.
    Retry {
        next_attempt_at: SystemTime,
        http_status: Option<u16>,
        error_class: DeliveryErrorClass,
    },

    /// Final failure. `reason` identifies the framework-level cause
    /// (retry budget exhausted, SSRF block, payload too large, etc.).
    /// `last_http_status` / `last_error_class` carry the diagnostic
    /// from the final attempt (or from the pre-POST rejection, when
    /// no POST was attempted). The store persists all three on the
    /// resulting [`FailedDelivery`] row.
    GaveUp {
        reason: GaveUpReason,
        last_error_class: DeliveryErrorClass,
        last_http_status: Option<u16>,
    },

    /// Terminal but not failed — delivery abandoned without a final
    /// POST attempt. Used for "config deleted", "task deleted",
    /// "non-HTTPS URL in production mode". `list_failed_deliveries`
    /// does NOT surface `Abandoned` rows.
    Abandoned { reason: AbandonedReason },
}

/// Framework-controlled reasons a delivery was given up.
///
/// Enum-only (no free-text variant) so a worker cannot thread
/// variable data — credentials, URLs, response bodies — into a
/// persisted claim row. `#[non_exhaustive]` so additional reasons
/// can be added in patch releases.
///
/// All variants here correspond to cases that SHOULD be visible
/// via [`A2aPushDeliveryStore::list_failed_deliveries`] — operators
/// see these and investigate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GaveUpReason {
    /// `push_max_attempts` reached without a 2xx/3xx response.
    /// Use [`Self::NonRetryableHttpStatus`] instead for 4xx
    /// responses that are rejected on the first attempt — those
    /// didn't exhaust the retry budget, they short-circuited it.
    MaxAttemptsExhausted,
    /// Receiver returned a permanent HTTP error (4xx other than
    /// 408/429). No retry was attempted; `last_http_status` on the
    /// failed-delivery row pinpoints the exact code.
    NonRetryableHttpStatus,
    /// Destination IP blocked by the SSRF guard. No POST attempted;
    /// listed as a failed delivery so operators can correct the
    /// config.
    SsrfBlocked,
    /// Serialised Task body exceeded `push_max_payload_bytes`. No
    /// POST attempted; operators see it here to adjust the limit or
    /// investigate the oversized task.
    PayloadTooLarge,
    /// TLS handshake rejected by the destination (cert invalid,
    /// protocol mismatch). Treated as a final failure rather than a
    /// transient retry, since TLS misconfiguration rarely resolves
    /// on its own.
    TlsRejected,
}

/// Framework-controlled reasons a delivery was abandoned.
///
/// Enum-only for the same reason as [`GaveUpReason`]. `Abandoned`
/// outcomes are NOT surfaced by `list_failed_deliveries` — they
/// represent "the delivery no longer needs to happen", not "a
/// delivery failed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AbandonedReason {
    /// Push config was deleted while a delivery was queued or
    /// mid-retry. Next retry tick found the config absent.
    ConfigDeleted,
    /// Task was deleted while a delivery was queued or mid-retry.
    TaskDeleted,
    /// Production mode refuses plain `http://` URLs. The config
    /// remains registered, but deliveries are abandoned. Operators
    /// opt into `http://` via `allow_insecure_push_urls` (dev-only
    /// flag).
    NonHttpsUrlInProduction,
}

/// Classified failure reasons. Enum-only so credential / PII
/// leakage through error strings is structurally impossible. Add
/// new variants when a new class of failure is introduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeliveryErrorClass {
    /// DNS failure, connection refused, reset mid-connect.
    NetworkError,
    /// Connect, read, or total request timeout exceeded.
    Timeout,
    /// HTTP 4xx response other than 408 / 429. Permanent; no retry.
    HttpError4xx { status: u16 },
    /// HTTP 5xx response. Transient; retried until the horizon.
    HttpError5xx { status: u16 },
    /// HTTP 429 Too Many Requests. Retried, respecting `Retry-After`.
    HttpError429,
    /// Destination rejected by SSRF guard — private-range IP, DNS
    /// rebinding attempt, allowlist denial.
    SSRFBlocked,
    /// Serialised Task body exceeds `push_max_payload_bytes`.
    PayloadTooLarge,
    /// Push config for this delivery no longer exists in storage.
    ConfigDeleted,
    /// Task for this delivery no longer exists in storage.
    TaskDeleted,
    /// TLS handshake failed (cert invalid, protocol mismatch).
    TlsRejected,
}

/// Public shape of a failed-delivery record returned by
/// [`A2aPushDeliveryStore::list_failed_deliveries`].
///
/// Carries classified diagnostics only — no credentials, no tokens,
/// no request bodies, no receiver response bodies. `config_id`
/// cross-references to the push-config CRUD store if operators
/// need receiver URL / scheme details.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FailedDelivery {
    pub task_id: String,
    pub config_id: String,
    pub event_sequence: u64,
    pub first_attempted_at: SystemTime,
    pub last_attempted_at: SystemTime,
    pub gave_up_at: SystemTime,
    /// Cluster-wide POST-attempt count at the time of giveup.
    pub delivery_attempt_count: u32,
    pub last_http_status: Option<u16>,
    pub last_error_class: DeliveryErrorClass,
}

/// Row returned by
/// [`A2aPushDeliveryStore::list_reclaimable_claims`]. Carries just
/// enough identity for the sweeper to reassemble the target:
/// `(tenant, owner, task_id, event_sequence, config_id)`. Owner is
/// persisted on the claim row by `claim_delivery` so the reclaim
/// path can call the owner-scoped
/// [`crate::storage::A2aTaskStorage::get_task`] without a separate
/// unscoped read method.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ReclaimableClaim {
    pub tenant: String,
    pub owner: String,
    pub task_id: String,
    pub event_sequence: u64,
    pub config_id: String,
}

/// Row returned by
/// [`A2aPushDeliveryStore::list_stale_pending_dispatches`]. Enough
/// identity for the reclaim loop to call
/// [`crate::storage::A2aTaskStorage::get_task`] and re-run the
/// fan-out logic a fresh dispatch would perform.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PendingDispatch {
    pub tenant: String,
    pub owner: String,
    pub task_id: String,
    pub event_sequence: u64,
    pub recorded_at: SystemTime,
}

impl PendingDispatch {
    /// Construct a pending-dispatch handle. External callers (Lambda
    /// stream worker, reclaim-sweep adapters) use this to reconstruct
    /// the struct from a stream record or backend row without
    /// tripping the `#[non_exhaustive]` guard.
    pub fn new(
        tenant: String,
        owner: String,
        task_id: String,
        event_sequence: u64,
        recorded_at: SystemTime,
    ) -> Self {
        Self {
            tenant,
            owner,
            task_id,
            event_sequence,
            recorded_at,
        }
    }
}
