//! Wiremock integration tests for the push delivery worker
//!.
//!
//! Covers a representative subset of the normative scenarios:
//! - §13.1 Basic delivery on terminal event (happy path)
//! - §13.3 Retry on 5xx with backoff
//! - §13.5 No retry on 4xx (non-408/429)
//! - §13.4 Giveup after max attempts
//! - §13.16 Secret redaction coverage
//!
//! These tests drive `PushDeliveryWorker::deliver` directly against a
//! wiremock `MockServer`. The dispatcher (event → configs fan-out) is
//! not exercised here — its scope is the server integration. Keeping
//! the worker's contract wiremock-verifiable is an explicit ADR-011
//! goal: the delivery module must be exerciseable without standing up
//! the full server, so the redaction + retry properties stay pinned to
//! fast unit-level tests.
//!
//! All tests use `allow_insecure_urls = true` because wiremock listens
//! on `127.0.0.1`, which is private SSRF rules. Backoff
//! knobs are shrunk to milliseconds so wall-clock assertions stay
//! under a second.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures::future::BoxFuture;
use turul_a2a::push::claim::{
    A2aPushDeliveryStore, DeliveryClaim, DeliveryOutcome, FailedDelivery,
};
use turul_a2a::push::delivery::{
    DeliveryReport, PushDeliveryConfig, PushDeliveryWorker, PushDnsResolver, PushTarget,
};
use turul_a2a::push::secret::Secret;
use turul_a2a::push::{DeliveryErrorClass, GaveUpReason};
use turul_a2a::storage::{A2aPushNotificationStorage, A2aStorageError, InMemoryA2aStorage};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Sentinels — used across redaction coverage.
// ---------------------------------------------------------------------------

const SENTINEL_CREDENTIAL: &str = "SECRET-CREDENTIAL-DO-NOT-LEAK";
const SENTINEL_TOKEN: &str = "SECRET-TOKEN-DO-NOT-LEAK";

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

/// Build a worker with a fast retry schedule suitable for integration
/// tests. Jitter is zeroed so backoff is deterministic.
fn fast_worker(store: Arc<InMemoryA2aStorage>, max_attempts: u32) -> PushDeliveryWorker {
    // `PushDeliveryConfig` is `#[non_exhaustive]` — can't be built via
    // struct literal from an external test crate, so mutate a default.
    let mut cfg = PushDeliveryConfig::default();
    cfg.max_attempts = max_attempts;
    cfg.backoff_base = Duration::from_millis(1);
    cfg.backoff_cap = Duration::from_millis(8);
    cfg.backoff_jitter = 0.0;
    cfg.request_timeout = Duration::from_secs(2);
    cfg.connect_timeout = Duration::from_secs(1);
    cfg.read_timeout = Duration::from_secs(2);
    cfg.claim_expiry = Duration::from_secs(60);
    cfg.max_payload_bytes = 64 * 1024;
    cfg.allow_insecure_urls = true; // wiremock lives on 127.0.0.1
    PushDeliveryWorker::new(
        store,
        cfg,
        None,
        format!("instance-{}", uuid::Uuid::now_v7()),
    )
    .expect("worker build must succeed")
}

fn sentinel_target(base_url: &str) -> PushTarget {
    PushTarget {
        tenant: "tenant-1".into(),
        owner: "anonymous".into(),
        task_id: format!("task-{}", uuid::Uuid::now_v7()),
        event_sequence: 1,
        config_id: "cfg-A".into(),
        url: Url::parse(&format!("{base_url}/webhook")).unwrap(),
        auth_scheme: "Bearer".into(),
        auth_credentials: Secret::new(SENTINEL_CREDENTIAL.into()),
        token: Some(Secret::new(SENTINEL_TOKEN.into())),
    }
}

// ---------------------------------------------------------------------------
// §13.1 — Happy path: single POST arrives, claim ends Succeeded.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_single_post_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1) // wiremock asserts on drop
        .mount(&server)
        .await;

    let store = Arc::new(InMemoryA2aStorage::new());
    let worker = fast_worker(store.clone(), 8);
    let target = sentinel_target(&server.uri());

    let report = worker.deliver(&target, br#"{"status":"completed"}"#).await;
    assert_eq!(report, DeliveryReport::Succeeded(200));

    // No failed-delivery record should be stored for a successful
    // delivery — only giveups/abandons are surfaced there.
    let failed = store
        .list_failed_deliveries(&target.tenant, SystemTime::UNIX_EPOCH, 10)
        .await
        .unwrap();
    assert!(
        failed.is_empty(),
        "successful delivery must not appear in failed list, got {failed:?}"
    );
}

// ---------------------------------------------------------------------------
// §13.3 — Retry on 5xx: 503 twice then 200, exactly 3 POSTs, Succeeded.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_on_5xx_then_success() {
    let server = MockServer::start().await;
    // wiremock-rs evaluates mocks by priority (lower number wins) and
    // then insertion order. We put the 503-limited mock at priority 1
    // so it always runs first while it still has capacity; after its
    // `up_to_n_times(2)` budget is exhausted, requests fall through to
    // the default-priority 200 mock.
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .with_priority(1)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let store = Arc::new(InMemoryA2aStorage::new());
    let worker = fast_worker(store.clone(), 8);
    let target = sentinel_target(&server.uri());

    let report = worker.deliver(&target, b"{}").await;
    assert_eq!(
        report,
        DeliveryReport::Succeeded(200),
        "two 503s followed by 200 must end in Succeeded"
    );

    // No failed-delivery record when the terminal outcome is a success,
    // even after intermediate 5xx attempts.
    let failed = store
        .list_failed_deliveries(&target.tenant, SystemTime::UNIX_EPOCH, 10)
        .await
        .unwrap();
    assert!(failed.is_empty());

    // Wiremock `.expect(N)` on both mounts is asserted on drop — this
    // proves exactly 3 POSTs arrived (2× 503 + 1× 200).
    drop(server);
}

// ---------------------------------------------------------------------------
// §13.5 — No retry on 4xx (non-408/429): exactly 1 POST, GaveUp.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_retry_on_4xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&server)
        .await;

    let store = Arc::new(InMemoryA2aStorage::new());
    let worker = fast_worker(store.clone(), 8);
    let target = sentinel_target(&server.uri());

    let report = worker.deliver(&target, b"{}").await;
    // Non-retryable 4xx is NOT `MaxAttemptsExhausted` — the retry
    // budget is never consumed. The dedicated reason lets operators
    // distinguish "receiver said no" from "we retried until we gave
    // up".
    assert_eq!(
        report,
        DeliveryReport::GaveUp(GaveUpReason::NonRetryableHttpStatus),
        "400 must produce GaveUp(NonRetryableHttpStatus), got {report:?}"
    );

    let failed = store
        .list_failed_deliveries(&target.tenant, SystemTime::UNIX_EPOCH, 10)
        .await
        .unwrap();
    assert_eq!(failed.len(), 1, "one failed-delivery record for 4xx");
    assert_eq!(failed[0].last_http_status, Some(400));
    assert!(matches!(
        failed[0].last_error_class,
        DeliveryErrorClass::HttpError4xx { status: 400 }
    ));
    assert_eq!(failed[0].delivery_attempt_count, 1);
}

// ---------------------------------------------------------------------------
// §13.4 — Giveup after max_attempts on sustained 500s.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn giveup_after_max_attempts_on_sustained_5xx() {
    let server = MockServer::start().await;
    let max_attempts: u32 = 3;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(500))
        .expect(max_attempts as u64)
        .mount(&server)
        .await;

    let store = Arc::new(InMemoryA2aStorage::new());
    let worker = fast_worker(store.clone(), max_attempts);
    let target = sentinel_target(&server.uri());

    let report = worker.deliver(&target, b"{}").await;
    assert_eq!(
        report,
        DeliveryReport::GaveUp(GaveUpReason::MaxAttemptsExhausted),
        "sustained 500 must terminate in MaxAttemptsExhausted"
    );

    let failed = store
        .list_failed_deliveries(&target.tenant, SystemTime::UNIX_EPOCH, 10)
        .await
        .unwrap();
    assert_eq!(failed.len(), 1, "one failed-delivery record for giveup");
    assert_eq!(
        failed[0].delivery_attempt_count, max_attempts,
        "delivery_attempt_count must equal configured max"
    );
    // Regression: the final GaveUp used to clobber the retry
    // diagnostics with `NetworkError` + `None`. With the fix, the
    // failed-delivery row preserves the last-seen HTTP status and
    // the classified 5xx error — exactly what an operator needs to
    // triage the webhook receiver.
    assert_eq!(
        failed[0].last_http_status,
        Some(500),
        "final FailedDelivery must carry the last HTTP status"
    );
    assert!(
        matches!(
            failed[0].last_error_class,
            DeliveryErrorClass::HttpError5xx { status: 500 }
        ),
        "final FailedDelivery must carry the classified 5xx, got {:?}",
        failed[0].last_error_class
    );

    // wiremock's `.expect(max_attempts)` is asserted on drop — this
    // proves exactly `max_attempts` POSTs arrived, no more, no less.
    drop(server);
}

// ---------------------------------------------------------------------------
// §13.16 — Secret redaction coverage.
//
// The worker must never surface the raw credential or push token in
// any observable form (Debug, Display, stored failed-delivery
// records). The worker does not currently emit tracing events on
// failure — when that's added, this test should be extended to capture
// log output too. For now it enforces the invariant at every surface
// the worker does touch.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn secret_sentinel_never_leaks_on_giveup() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&server)
        .await;

    let store = Arc::new(InMemoryA2aStorage::new());
    let worker = fast_worker(store.clone(), 4);
    let target = sentinel_target(&server.uri());

    // Sanity: the sentinels are present in the Secret values the
    // worker actually sends on the wire (via `expose()` at header
    // build time). If this assertion ever fires, the test is
    // mis-configured and the whole redaction check is meaningless.
    assert_eq!(
        target.auth_credentials.expose().as_str(),
        SENTINEL_CREDENTIAL
    );
    assert_eq!(
        target.token.as_ref().map(|t| t.expose().as_str()),
        Some(SENTINEL_TOKEN)
    );

    let _report = worker.deliver(&target, b"{}").await;

    // 1) Debug-rendering the Secret values must not expose them.
    let dbg_cred = format!("{:?}", target.auth_credentials);
    let dbg_tok = format!("{:?}", target.token);
    assert!(
        !dbg_cred.contains(SENTINEL_CREDENTIAL),
        "Secret Debug leaked credential: {dbg_cred}"
    );
    assert!(
        !dbg_tok.contains(SENTINEL_TOKEN),
        "Secret Debug leaked token: {dbg_tok}"
    );

    // 2) Stored failed-delivery record is free of secret payloads.
    // The row holds error class + HTTP status + timestamps only —
    // b pins this contract.
    let failed = store
        .list_failed_deliveries(&target.tenant, SystemTime::UNIX_EPOCH, 10)
        .await
        .unwrap();
    assert_eq!(failed.len(), 1);
    let rendered = format!("{:?}", failed[0]);
    assert!(
        !rendered.contains(SENTINEL_CREDENTIAL),
        "FailedDelivery leaked credential: {rendered}"
    );
    assert!(
        !rendered.contains(SENTINEL_TOKEN),
        "FailedDelivery leaked token: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// §13.18 — DNS rebinding defence.
//
// The worker must connect to the IP that passed SSRF validation, not
// whatever a subsequent DNS lookup would return. We prove this by:
//
// 1. Crafting a URL with a hostname that no real DNS zone can resolve
//    (`.invalid` is reserved for exactly this purpose, RFC 2606).
// 2. Injecting a `PushDnsResolver` that returns the wiremock IP for
//    that hostname — this is the "validation" step's output.
// 3. Running delivery. With the rebinding pin in place (reqwest
//    `resolve()` override), the POST reaches wiremock. Without the
//    pin, reqwest falls back to the system resolver, the `.invalid`
//    hostname fails to resolve, and the POST errors out before hitting
//    wiremock at all.
//
// Also tracks DNS resolver calls: the worker must resolve once per
// attempt. In the one-attempt happy path that's exactly one call.
// ---------------------------------------------------------------------------

/// DNS resolver that returns a fixed IP for any hostname and counts calls.
struct PinResolver {
    ip: IpAddr,
    call_count: Arc<Mutex<u32>>,
}

impl PushDnsResolver for PinResolver {
    fn resolve(&self, _host: &str, _port: u16) -> BoxFuture<'_, Result<Vec<IpAddr>, String>> {
        *self.call_count.lock().unwrap() += 1;
        let ip = self.ip;
        Box::pin(async move { Ok(vec![ip]) })
    }
}

#[tokio::test]
async fn reqwest_connects_to_validated_ip_not_system_dns() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let server_addr = *server.address();
    let store = Arc::new(InMemoryA2aStorage::new());

    let call_count = Arc::new(Mutex::new(0u32));
    let resolver = Arc::new(PinResolver {
        ip: server_addr.ip(),
        call_count: call_count.clone(),
    });
    let worker = fast_worker(store.clone(), 1).with_dns_resolver(resolver);

    // `.invalid` is RFC-2606 reserved — guaranteed NXDOMAIN via system
    // DNS. If the worker bypassed our pin, reqwest would fail to
    // connect and wiremock's `.expect(1)` would fail on drop.
    let url = format!(
        "http://rebind-defence.invalid:{}/webhook",
        server_addr.port()
    );

    let target = PushTarget {
        tenant: "tenant-1".into(),
        owner: "anonymous".into(),
        task_id: format!("task-{}", uuid::Uuid::now_v7()),
        event_sequence: 1,
        config_id: "cfg-A".into(),
        url: Url::parse(&url).unwrap(),
        auth_scheme: "Bearer".into(),
        auth_credentials: Secret::new("cred".into()),
        token: None,
    };

    let report = worker.deliver(&target, b"{}").await;
    assert_eq!(
        report,
        DeliveryReport::Succeeded(200),
        "pinned resolver must let the POST reach wiremock despite .invalid hostname"
    );

    // DNS is resolved exactly once per attempt (ADR-011 §5, §R4).
    assert_eq!(
        *call_count.lock().unwrap(),
        1,
        "resolver must be consulted exactly once per attempt"
    );

    // Wiremock `.expect(1)` asserts on drop.
    drop(server);
}

// ---------------------------------------------------------------------------
// Terminal-outcome persistence.
//
// If the POST succeeds but the terminal `record_delivery_outcome`
// write fails, the worker must NOT return
// `DeliveryReport::Succeeded`: the claim row is still non-terminal,
// and a sweeper would legitimately re-claim and issue a duplicate
// POST while the dispatcher treats the event as resolved. Instead
// the worker reports `TransientStoreError` (caller/sweeper retries)
// or `ClaimLostOrFinal` on `StaleDeliveryClaim` (another writer
// already finalised the row — receiver may observe one extra POST,
// which at-least-once semantics already allowa).
// ---------------------------------------------------------------------------

/// Wrapper that delegates every call to the inner in-memory store
/// EXCEPT terminal `record_delivery_outcome` writes, which return a
/// configurable error up to `fail_count` times. Retry (non-terminal)
/// writes still pass through so the rest of the worker's bookkeeping
/// is unchanged. After `fail_count` terminal attempts, subsequent
/// terminal writes go through to the inner store — lets tests probe
/// the bounded-retry path (0 for "never fail", N for "succeed on
/// attempt N+1", usize::MAX for "always fail").
struct FailingTerminalStore {
    inner: Arc<InMemoryA2aStorage>,
    error_template: A2aStorageError,
    remaining_failures: Mutex<usize>,
    /// Observed count of terminal writes (including the ones that
    /// failed). Tests assert this against the worker's bounded
    /// retry budget.
    terminal_writes_seen: Mutex<usize>,
}

impl FailingTerminalStore {
    fn new(inner: Arc<InMemoryA2aStorage>, err: A2aStorageError, fail_count: usize) -> Self {
        Self {
            inner,
            error_template: err,
            remaining_failures: Mutex::new(fail_count),
            terminal_writes_seen: Mutex::new(0),
        }
    }
}

/// Cheap `Clone` for the enum variants we actually exercise. The
/// real `A2aStorageError` is not `Clone` (anyhow-ish payloads), so
/// we reconstruct each variant explicitly.
fn clone_storage_error(e: &A2aStorageError) -> A2aStorageError {
    match e {
        A2aStorageError::DatabaseError(s) => A2aStorageError::DatabaseError(s.clone()),
        A2aStorageError::StaleDeliveryClaim {
            tenant,
            task_id,
            event_sequence,
            config_id,
        } => A2aStorageError::StaleDeliveryClaim {
            tenant: tenant.clone(),
            task_id: task_id.clone(),
            event_sequence: *event_sequence,
            config_id: config_id.clone(),
        },
        other => A2aStorageError::DatabaseError(format!(
            "FailingTerminalStore: unsupported error variant {other:?}"
        )),
    }
}

#[async_trait]
impl A2aPushDeliveryStore for FailingTerminalStore {
    fn backend_name(&self) -> &'static str {
        A2aPushDeliveryStore::backend_name(&*self.inner)
    }

    async fn claim_delivery(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        config_id: &str,
        claimant: &str,
        owner: &str,
        claim_expiry: Duration,
    ) -> Result<DeliveryClaim, A2aStorageError> {
        self.inner
            .claim_delivery(
                tenant,
                task_id,
                event_sequence,
                config_id,
                claimant,
                owner,
                claim_expiry,
            )
            .await
    }

    async fn list_reclaimable_claims(
        &self,
        limit: usize,
    ) -> Result<Vec<turul_a2a::push::claim::ReclaimableClaim>, A2aStorageError> {
        self.inner.list_reclaimable_claims(limit).await
    }

    async fn record_pending_dispatch(
        &self,
        tenant: &str,
        owner: &str,
        task_id: &str,
        event_sequence: u64,
    ) -> Result<(), A2aStorageError> {
        self.inner
            .record_pending_dispatch(tenant, owner, task_id, event_sequence)
            .await
    }

    async fn delete_pending_dispatch(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
    ) -> Result<(), A2aStorageError> {
        self.inner
            .delete_pending_dispatch(tenant, task_id, event_sequence)
            .await
    }

    async fn list_stale_pending_dispatches(
        &self,
        older_than_recorded_at: std::time::SystemTime,
        limit: usize,
    ) -> Result<Vec<turul_a2a::push::claim::PendingDispatch>, A2aStorageError> {
        self.inner
            .list_stale_pending_dispatches(older_than_recorded_at, limit)
            .await
    }

    async fn record_attempt_started(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        config_id: &str,
        claimant: &str,
        claim_generation: u64,
    ) -> Result<u32, A2aStorageError> {
        self.inner
            .record_attempt_started(
                tenant,
                task_id,
                event_sequence,
                config_id,
                claimant,
                claim_generation,
            )
            .await
    }

    async fn record_delivery_outcome(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        config_id: &str,
        claimant: &str,
        claim_generation: u64,
        outcome: DeliveryOutcome,
    ) -> Result<(), A2aStorageError> {
        let is_terminal = matches!(
            outcome,
            DeliveryOutcome::Succeeded { .. }
                | DeliveryOutcome::GaveUp { .. }
                | DeliveryOutcome::Abandoned { .. }
        );
        if is_terminal {
            *self.terminal_writes_seen.lock().unwrap() += 1;
            let mut remaining = self.remaining_failures.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
                return Err(clone_storage_error(&self.error_template));
            }
        }
        self.inner
            .record_delivery_outcome(
                tenant,
                task_id,
                event_sequence,
                config_id,
                claimant,
                claim_generation,
                outcome,
            )
            .await
    }

    async fn sweep_expired_claims(&self) -> Result<u64, A2aStorageError> {
        self.inner.sweep_expired_claims().await
    }

    async fn list_failed_deliveries(
        &self,
        tenant: &str,
        since: SystemTime,
        limit: usize,
    ) -> Result<Vec<FailedDelivery>, A2aStorageError> {
        self.inner
            .list_failed_deliveries(tenant, since, limit)
            .await
    }
}

#[tokio::test]
async fn terminal_persist_failure_becomes_transient_not_succeeded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let inner = Arc::new(InMemoryA2aStorage::new());
    // usize::MAX → every attempt, including the bounded retry
    // budget inside persist_terminal, hits the error.
    let failing = Arc::new(FailingTerminalStore::new(
        inner.clone(),
        A2aStorageError::DatabaseError("simulated storage outage".into()),
        usize::MAX,
    ));
    let store: Arc<dyn A2aPushDeliveryStore> = failing.clone();

    // Build the worker directly so we can inject the wrapper store.
    let mut cfg = PushDeliveryConfig::default();
    cfg.max_attempts = 1;
    cfg.backoff_base = Duration::from_millis(1);
    cfg.backoff_cap = Duration::from_millis(1);
    cfg.backoff_jitter = 0.0;
    cfg.request_timeout = Duration::from_secs(1);
    cfg.connect_timeout = Duration::from_millis(500);
    cfg.read_timeout = Duration::from_secs(1);
    cfg.claim_expiry = Duration::from_secs(60);
    cfg.max_payload_bytes = 64 * 1024;
    cfg.allow_insecure_urls = true;
    let worker =
        PushDeliveryWorker::new(store, cfg, None, "instance-fail".into()).expect("worker build");

    let target = sentinel_target(&server.uri());

    let report = worker.deliver(&target, b"{}").await;
    assert_eq!(
        report,
        DeliveryReport::TransientStoreError,
        "worker must NOT report Succeeded when the terminal write fails"
    );

    // The worker's bounded retry in persist_terminal exercised all
    // three attempts before surfacing the failure. Observing
    // `terminal_writes_seen == 3` locks the bound — a regression
    // that reverts the retry would drop this to 1.
    assert_eq!(
        *failing.terminal_writes_seen.lock().unwrap(),
        3,
        "persist_terminal must try three times before reporting Transient"
    );

    // Wiremock still saw the POST — the receiver received the push,
    // which is the at-least-once contract. What matters is that the
    // worker does not mislead the dispatcher into thinking the claim
    // row is final.
    drop(server);
}

// ---------------------------------------------------------------------------
// Reclaim-and-redispatch recovery.
//
// Truly persistent terminal-write failures — failures that outlast
// the worker's bounded persist retry — must not leave the claim row
// stuck forever. The server's reclaim loop calls
// `list_reclaimable_claims` and hands each row to
// `PushDispatcher::redispatch_one`, which re-invokes `deliver()`.
// Because the stored row is expired and non-terminal,
// `claim_delivery` increments the generation and the worker proceeds
// as if it were a fresh delivery. The wiremock therefore sees two
// POSTs for the same event — the receiver is responsible for
// deduplication per the at-least-once contract.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reclaim_sweep_redispatches_after_persistent_terminal_failure() {
    use turul_a2a::push::dispatcher::PushDispatcher;

    let server = MockServer::start().await;
    // Two POSTs arrive: one for the initial (stuck) delivery,
    // one for the redispatch after the claim expires and the sweep
    // picks it up.
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&server)
        .await;

    let inner = Arc::new(InMemoryA2aStorage::new());
    // Fail the first three terminal writes → first deliver exhausts
    // persist_terminal's bounded retry (3 attempts) and returns
    // Transient. Subsequent terminal writes succeed.
    let failing = Arc::new(FailingTerminalStore::new(
        inner.clone(),
        A2aStorageError::DatabaseError("simulated persistent store outage".into()),
        3,
    ));
    let delivery_store: Arc<dyn A2aPushDeliveryStore> = failing.clone();

    // Worker config: short claim_expiry so the row becomes
    // reclaimable quickly, short backoffs to keep the test fast.
    // `max_attempts=2` leaves the redispatched deliver enough retry
    // budget to issue the second POST — the budget is cluster-wide
    // (ADR-011 §10 — `delivery_attempt_count` is NOT reset on
    // re-claim, so count=1 after the first deliver's POST and the
    // redispatch can do exactly one more).
    let mut cfg = PushDeliveryConfig::default();
    cfg.max_attempts = 2;
    cfg.backoff_base = Duration::from_millis(1);
    cfg.backoff_cap = Duration::from_millis(1);
    cfg.backoff_jitter = 0.0;
    cfg.request_timeout = Duration::from_secs(1);
    cfg.connect_timeout = Duration::from_millis(500);
    cfg.read_timeout = Duration::from_secs(1);
    cfg.claim_expiry = Duration::from_millis(100);
    cfg.max_payload_bytes = 64 * 1024;
    cfg.allow_insecure_urls = true;
    let worker = Arc::new(
        PushDeliveryWorker::new(delivery_store.clone(), cfg, None, "instance-A".into())
            .expect("worker build"),
    );

    let push_storage: Arc<dyn turul_a2a::storage::A2aPushNotificationStorage> = inner.clone();
    let task_storage: Arc<dyn turul_a2a::storage::A2aTaskStorage> = inner.clone();
    let dispatcher =
        PushDispatcher::new(worker.clone(), push_storage.clone(), task_storage.clone());

    // Seed a task in storage so the redispatch path's get_task
    // succeeds. Use the owner-scoped atomic store since that's what
    // the server would do.
    let tenant = "tenant-reclaim";
    let task_id = format!("task-{}", uuid::Uuid::now_v7());
    let owner = "anonymous";
    let task = turul_a2a_types::Task::new(
        &task_id,
        turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Working),
    )
    .with_context_id("ctx-reclaim");
    let submitted = turul_a2a::streaming::StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: task_id.clone(),
            context_id: "ctx-reclaim".into(),
            status: serde_json::to_value(turul_a2a_types::TaskStatus::new(
                turul_a2a_types::TaskState::Submitted,
            ))
            .unwrap(),
        },
    };
    use turul_a2a::storage::A2aAtomicStore;
    inner
        .create_task_with_events(tenant, owner, task, vec![submitted])
        .await
        .expect("seed task");

    // Create a push config so redispatch_one's get_config returns.
    let cfg_proto = turul_a2a_proto::TaskPushNotificationConfig {
        tenant: tenant.into(),
        id: "cfg-reclaim".into(),
        task_id: task_id.clone(),
        url: format!("{}/webhook", server.uri()),
        token: String::new(),
        authentication: Some(turul_a2a_proto::AuthenticationInfo {
            scheme: "Bearer".into(),
            credentials: "cred".into(),
        }),
    };
    push_storage
        .create_config(tenant, cfg_proto.clone())
        .await
        .expect("create_config");

    // --- Step 1: first deliver exhausts persist_terminal retries.
    let target =
        turul_a2a::push::delivery::PushTarget::from_config(tenant, owner, &task_id, 1, &cfg_proto)
            .expect("valid target");
    let payload = b"{}".to_vec();
    let first = worker.deliver(&target, &payload).await;
    assert_eq!(
        first,
        DeliveryReport::TransientStoreError,
        "first delivery must return Transient after persist retry budget"
    );
    // Persist retry burned all three attempts against the terminal write.
    assert_eq!(*failing.terminal_writes_seen.lock().unwrap(), 3);

    // --- Step 2: wait for the claim to expire, then verify the
    // reclaim enumeration surfaces the row. Using delivery_store
    // directly; the wrapper delegates list_reclaimable_claims to
    // the inner store.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let reclaimable = delivery_store
        .list_reclaimable_claims(16)
        .await
        .expect("list_reclaimable_claims");
    assert_eq!(
        reclaimable.len(),
        1,
        "exactly one reclaimable row expected, got {reclaimable:?}"
    );
    assert_eq!(reclaimable[0].owner, owner);
    assert_eq!(reclaimable[0].task_id, task_id);
    assert_eq!(reclaimable[0].config_id, "cfg-reclaim");

    // --- Step 3: redispatch. The wrapper's fail counter is now
    // exhausted, so the terminal write succeeds on attempt 1 of the
    // second deliver's persist_terminal invocation.
    let row = reclaimable.into_iter().next().unwrap();
    dispatcher.redispatch_one(row).await;

    // --- Step 4: the row should now be terminal — no longer
    // reclaimable. Wiremock asserts on drop that exactly 2 POSTs
    // arrived: one from the original (stuck) attempt and one from
    // the redispatch.
    let reclaimable_after = delivery_store
        .list_reclaimable_claims(16)
        .await
        .expect("list_reclaimable_claims");
    assert!(
        reclaimable_after.is_empty(),
        "redispatch should have driven the row terminal; still reclaimable: {reclaimable_after:?}"
    );

    drop(server);
}

#[tokio::test]
async fn terminal_persist_recovers_after_transient_blip() {
    // Two initial failures, third attempt succeeds: the bounded
    // retry inside persist_terminal must absorb the blip and return
    // a proper Succeeded report. If the retry ever regresses this
    // test flips to TransientStoreError.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let inner = Arc::new(InMemoryA2aStorage::new());
    let failing = Arc::new(FailingTerminalStore::new(
        inner.clone(),
        A2aStorageError::DatabaseError("blip".into()),
        2, // fail twice, let attempt 3 through
    ));
    let store: Arc<dyn A2aPushDeliveryStore> = failing.clone();

    let mut cfg = PushDeliveryConfig::default();
    cfg.max_attempts = 1;
    cfg.backoff_base = Duration::from_millis(1);
    cfg.backoff_cap = Duration::from_millis(1);
    cfg.backoff_jitter = 0.0;
    cfg.request_timeout = Duration::from_secs(1);
    cfg.connect_timeout = Duration::from_millis(500);
    cfg.read_timeout = Duration::from_secs(1);
    cfg.claim_expiry = Duration::from_secs(60);
    cfg.max_payload_bytes = 64 * 1024;
    cfg.allow_insecure_urls = true;
    let worker =
        PushDeliveryWorker::new(store, cfg, None, "instance-blip".into()).expect("worker build");
    let target = sentinel_target(&server.uri());

    let report = worker.deliver(&target, b"{}").await;
    assert_eq!(report, DeliveryReport::Succeeded(200));
    assert_eq!(
        *failing.terminal_writes_seen.lock().unwrap(),
        3,
        "all three attempts must be observed (two fail, third succeeds)"
    );

    drop(server);
}

#[tokio::test]
async fn stale_claim_on_terminal_write_becomes_claim_lost() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let inner = Arc::new(InMemoryA2aStorage::new());
    let failing = Arc::new(FailingTerminalStore::new(
        inner.clone(),
        A2aStorageError::StaleDeliveryClaim {
            tenant: "tenant-1".into(),
            task_id: "task-stale".into(),
            event_sequence: 1,
            config_id: "cfg-A".into(),
        },
        usize::MAX,
    ));
    let store: Arc<dyn A2aPushDeliveryStore> = failing.clone();

    let mut cfg = PushDeliveryConfig::default();
    cfg.max_attempts = 1;
    cfg.backoff_base = Duration::from_millis(1);
    cfg.backoff_cap = Duration::from_millis(1);
    cfg.backoff_jitter = 0.0;
    cfg.request_timeout = Duration::from_secs(1);
    cfg.connect_timeout = Duration::from_millis(500);
    cfg.read_timeout = Duration::from_secs(1);
    cfg.claim_expiry = Duration::from_secs(60);
    cfg.max_payload_bytes = 64 * 1024;
    cfg.allow_insecure_urls = true;
    let worker =
        PushDeliveryWorker::new(store, cfg, None, "instance-stale".into()).expect("worker build");
    let target = sentinel_target(&server.uri());

    let report = worker.deliver(&target, b"{}").await;
    assert_eq!(
        report,
        DeliveryReport::ClaimLostOrFinal,
        "StaleDeliveryClaim on terminal write means another writer finalised \
         the row; worker reports ClaimLostOrFinal, not Succeeded"
    );
    // Fencing loss is not retryable — persist_terminal returns on
    // the first StaleDeliveryClaim without consuming the budget.
    assert_eq!(
        *failing.terminal_writes_seen.lock().unwrap(),
        1,
        "StaleDeliveryClaim must short-circuit on attempt 1"
    );
    drop(server);
}

// ---------------------------------------------------------------------------
// Unauthenticated config: no Authorization header.
//
// Configs with no `authentication` carry empty auth_scheme + empty
// credentials. Emitting `Authorization: "{scheme} {credentials}"`
// with both parts empty produces a malformed header that hardened
// receivers reject and muddies the "this config is unauthenticated"
// semantics. The worker omits the header entirely when scheme is
// empty.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthenticated_config_sends_no_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let store = Arc::new(InMemoryA2aStorage::new());
    let worker = fast_worker(store, 1);

    // Empty auth_scheme + empty credentials + no token — the shape a
    // `PushTarget::from_config` produces for a config with
    // `authentication = None` and `token = ""`.
    let target = PushTarget {
        tenant: "t".into(),
        owner: "anonymous".into(),
        task_id: format!("task-{}", uuid::Uuid::now_v7()),
        event_sequence: 1,
        config_id: "cfg-noauth".into(),
        url: Url::parse(&format!("{}/webhook", server.uri())).unwrap(),
        auth_scheme: String::new(),
        auth_credentials: Secret::new(String::new()),
        token: None,
    };

    let report = worker.deliver(&target, b"{}").await;
    assert_eq!(report, DeliveryReport::Succeeded(200));

    // The recorded request must not carry an Authorization header.
    let reqs = server
        .received_requests()
        .await
        .expect("wiremock recording enabled");
    assert_eq!(reqs.len(), 1);
    assert!(
        !reqs[0].headers.contains_key("authorization"),
        "unauthenticated config must NOT set Authorization header; got headers: {:?}",
        reqs[0].headers.keys().collect::<Vec<_>>()
    );

    drop(server);
}

// ---------------------------------------------------------------------------
// Dispatcher error-handling regressions.
//
// Two related defects the reclaim path used to have:
//
// 1. `redispatch_one` conflated `Ok(None)` and `Err(_)` when reading
//    the task or push config, so a transient storage outage would
//    Abandoned-terminalise a still-valid reclaimable row.
//
// 2. The initial terminal-event dispatch returned immediately on
//    `list_configs` failure — with no claim row yet, the reclaim
//    sweeper had nothing to walk and the notification was silently
//    lost.
//
// These wrappers let us simulate both failure modes without spinning
// up a real backend.
// ---------------------------------------------------------------------------

/// Push-config store wrapper that counts every call and can inject
/// errors on `get_config` / `list_configs` up to a configurable
/// attempt budget. Delegates to an inner in-memory store once the
/// fail budget is exhausted so the test can exercise the
/// recovery-after-transient-failure path.
struct FailingPushStorage {
    inner: Arc<InMemoryA2aStorage>,
    get_config_failures_remaining: Mutex<usize>,
    list_configs_failures_remaining: Mutex<usize>,
    list_configs_calls: Mutex<usize>,
}

impl FailingPushStorage {
    fn new(
        inner: Arc<InMemoryA2aStorage>,
        get_config_failures: usize,
        list_configs_failures: usize,
    ) -> Self {
        Self {
            inner,
            get_config_failures_remaining: Mutex::new(get_config_failures),
            list_configs_failures_remaining: Mutex::new(list_configs_failures),
            list_configs_calls: Mutex::new(0),
        }
    }
}

#[async_trait]
impl turul_a2a::storage::A2aPushNotificationStorage for FailingPushStorage {
    fn backend_name(&self) -> &'static str {
        turul_a2a::storage::A2aPushNotificationStorage::backend_name(&*self.inner)
    }
    async fn create_config(
        &self,
        tenant: &str,
        config: turul_a2a_proto::TaskPushNotificationConfig,
    ) -> Result<turul_a2a_proto::TaskPushNotificationConfig, A2aStorageError> {
        self.inner.create_config(tenant, config).await
    }
    async fn get_config(
        &self,
        tenant: &str,
        task_id: &str,
        config_id: &str,
    ) -> Result<Option<turul_a2a_proto::TaskPushNotificationConfig>, A2aStorageError> {
        let should_fail = {
            let mut remaining = self.get_config_failures_remaining.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
                true
            } else {
                false
            }
        };
        if should_fail {
            return Err(A2aStorageError::DatabaseError(
                "simulated push-config store outage".into(),
            ));
        }
        self.inner.get_config(tenant, task_id, config_id).await
    }
    async fn list_configs(
        &self,
        tenant: &str,
        task_id: &str,
        page_token: Option<&str>,
        page_size: Option<i32>,
    ) -> Result<turul_a2a::storage::PushConfigListPage, A2aStorageError> {
        *self.list_configs_calls.lock().unwrap() += 1;
        let should_fail = {
            let mut remaining = self.list_configs_failures_remaining.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
                true
            } else {
                false
            }
        };
        if should_fail {
            return Err(A2aStorageError::DatabaseError(
                "simulated push-config store list failure".into(),
            ));
        }
        self.inner
            .list_configs(tenant, task_id, page_token, page_size)
            .await
    }
    async fn delete_config(
        &self,
        tenant: &str,
        task_id: &str,
        config_id: &str,
    ) -> Result<(), A2aStorageError> {
        self.inner.delete_config(tenant, task_id, config_id).await
    }
    async fn list_configs_eligible_at_event(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        page_token: Option<&str>,
        page_size: Option<i32>,
    ) -> Result<turul_a2a::storage::PushConfigListPage, A2aStorageError> {
        // Mirror the list_configs outage simulation for the
        // eligibility path that the dispatcher now calls.
        *self.list_configs_calls.lock().unwrap() += 1;
        let should_fail = {
            let mut remaining = self.list_configs_failures_remaining.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
                true
            } else {
                false
            }
        };
        if should_fail {
            return Err(A2aStorageError::DatabaseError(
                "simulated push-config store list failure".into(),
            ));
        }
        self.inner
            .list_configs_eligible_at_event(tenant, task_id, event_sequence, page_token, page_size)
            .await
    }
}

#[tokio::test]
async fn redispatch_read_error_leaves_row_reclaimable() {
    use turul_a2a::push::dispatcher::PushDispatcher;

    let server = MockServer::start().await;
    // NO POST should arrive — the redispatch must abort cleanly on
    // the transient read error without consuming the claim.
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let inner = Arc::new(InMemoryA2aStorage::new());
    // Fail the first get_config call; subsequent calls go through.
    let push_storage = Arc::new(FailingPushStorage::new(inner.clone(), 1, 0));

    // Worker with short claim expiry so we can observe the
    // reclaimable state after redispatch.
    let delivery_store: Arc<dyn A2aPushDeliveryStore> = inner.clone();
    let mut cfg = PushDeliveryConfig::default();
    cfg.max_attempts = 2;
    cfg.backoff_base = Duration::from_millis(1);
    cfg.backoff_cap = Duration::from_millis(1);
    cfg.backoff_jitter = 0.0;
    cfg.request_timeout = Duration::from_secs(1);
    cfg.connect_timeout = Duration::from_millis(500);
    cfg.read_timeout = Duration::from_secs(1);
    cfg.claim_expiry = Duration::from_millis(50);
    cfg.max_payload_bytes = 64 * 1024;
    cfg.allow_insecure_urls = true;
    let worker = Arc::new(
        PushDeliveryWorker::new(delivery_store.clone(), cfg, None, "instance-rd".into())
            .expect("worker build"),
    );

    let task_storage: Arc<dyn turul_a2a::storage::A2aTaskStorage> = inner.clone();
    let push_storage_trait: Arc<dyn turul_a2a::storage::A2aPushNotificationStorage> =
        push_storage.clone();
    let dispatcher = PushDispatcher::new(worker, push_storage_trait, task_storage);

    // Seed task + config.
    let tenant = "tenant-rd";
    let task_id = "task-rd-1";
    let owner = "anonymous";
    let task = turul_a2a_types::Task::new(
        task_id,
        turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Working),
    )
    .with_context_id("ctx-rd");
    let submitted = turul_a2a::streaming::StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: task_id.into(),
            context_id: "ctx-rd".into(),
            status: serde_json::to_value(turul_a2a_types::TaskStatus::new(
                turul_a2a_types::TaskState::Submitted,
            ))
            .unwrap(),
        },
    };
    use turul_a2a::storage::A2aAtomicStore;
    inner
        .create_task_with_events(tenant, owner, task, vec![submitted])
        .await
        .expect("seed task");
    inner
        .create_config(
            tenant,
            turul_a2a_proto::TaskPushNotificationConfig {
                tenant: tenant.into(),
                id: "cfg-rd".into(),
                task_id: task_id.into(),
                url: format!("{}/webhook", server.uri()),
                token: String::new(),
                authentication: None,
            },
        )
        .await
        .expect("seed config");

    // Seed a claim row, then let it expire.
    delivery_store
        .claim_delivery(
            tenant,
            task_id,
            1,
            "cfg-rd",
            "instance-rd",
            owner,
            Duration::from_millis(50),
        )
        .await
        .expect("seed claim");
    tokio::time::sleep(Duration::from_millis(70)).await;

    let reclaimable = delivery_store
        .list_reclaimable_claims(16)
        .await
        .expect("list_reclaimable_claims");
    assert_eq!(reclaimable.len(), 1);
    let row = reclaimable.into_iter().next().unwrap();

    // Invoke redispatch. The first get_config call fails with a
    // transient error. The old behaviour would Abandoned-terminalise
    // the row; the fixed behaviour leaves it reclaimable for the
    // next sweep tick.
    dispatcher.redispatch_one(row).await;

    // Row must still be non-terminal — i.e., still reclaimable.
    let still_reclaimable = delivery_store
        .list_reclaimable_claims(16)
        .await
        .expect("list_reclaimable_claims second call");
    assert_eq!(
        still_reclaimable.len(),
        1,
        "transient get_config error must NOT terminalise the row; \
         it should remain reclaimable for the next sweep tick"
    );

    drop(server);
}

#[tokio::test]
async fn pending_dispatch_marker_recovers_persistent_list_configs_outage() {
    use turul_a2a::push::dispatcher::PushDispatcher;

    let server = MockServer::start().await;
    // Exactly one POST: the first dispatch attempt fails (list_configs
    // keeps returning errors past the bounded-retry budget). The
    // marker survives, the reclaim pulls it, the second dispatch
    // attempt runs list_configs against a recovered store and the
    // POST lands.
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    // opt in to atomic marker writes so the terminal
    // transition below records the pending-dispatch marker in the
    // same atomic boundary as the task/event commit.
    let inner = Arc::new(InMemoryA2aStorage::new().with_push_dispatch_enabled(true));
    // Fail list_configs 4 times: 3 initial retries + 1 more after a
    // potential redispatch to ensure the first full fan-out attempt
    // fails. All later calls pass through.
    let push_storage = Arc::new(FailingPushStorage::new(inner.clone(), 0, 4));

    let delivery_store: Arc<dyn A2aPushDeliveryStore> = inner.clone();
    let mut cfg = PushDeliveryConfig::default();
    cfg.max_attempts = 1;
    cfg.backoff_base = Duration::from_millis(1);
    cfg.backoff_cap = Duration::from_millis(1);
    cfg.backoff_jitter = 0.0;
    cfg.request_timeout = Duration::from_secs(1);
    cfg.connect_timeout = Duration::from_millis(500);
    cfg.read_timeout = Duration::from_secs(1);
    cfg.claim_expiry = Duration::from_millis(50);
    cfg.max_payload_bytes = 64 * 1024;
    cfg.allow_insecure_urls = true;
    let worker = Arc::new(
        PushDeliveryWorker::new(delivery_store.clone(), cfg, None, "instance-pd".into())
            .expect("worker build"),
    );

    let task_storage: Arc<dyn turul_a2a::storage::A2aTaskStorage> = inner.clone();
    let push_storage_trait: Arc<dyn turul_a2a::storage::A2aPushNotificationStorage> =
        push_storage.clone();
    let dispatcher = PushDispatcher::new(worker, push_storage_trait, task_storage);

    let tenant = "tenant-pd";
    let task_id = "task-pd-1";
    let owner = "anonymous";
    // Seed task as Working, register the config, then transition
    // to Completed via the atomic commit — which is where the
    // pending-dispatch marker is now written.
    let working = turul_a2a_types::Task::new(
        task_id,
        turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Working),
    )
    .with_context_id("ctx-pd");
    let completed_status = turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Completed);
    let completed = turul_a2a::streaming::StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: task_id.into(),
            context_id: "ctx-pd".into(),
            status: serde_json::to_value(&completed_status).unwrap(),
        },
    };
    use turul_a2a::storage::A2aAtomicStore;
    inner
        .create_task_with_events(tenant, owner, working, vec![])
        .await
        .expect("seed task");
    inner
        .create_config(
            tenant,
            turul_a2a_proto::TaskPushNotificationConfig {
                tenant: tenant.into(),
                id: "cfg-pd".into(),
                task_id: task_id.into(),
                url: format!("{}/webhook", server.uri()),
                token: String::new(),
                authentication: None,
            },
        )
        .await
        .expect("seed config");

    // Atomic commit: writes task + event + pending-dispatch marker.
    let (terminal_task, seqs) = inner
        .update_task_status_with_events(
            tenant,
            task_id,
            owner,
            completed_status,
            vec![completed.clone()],
        )
        .await
        .expect("terminal commit");
    let terminal_seq = seqs[0];

    // First dispatch: list_configs fails through its bounded retry,
    // so the fan-out aborts. The marker written by the atomic commit
    // remains in the store.
    dispatcher.dispatch(
        tenant.into(),
        owner.into(),
        terminal_task,
        vec![(terminal_seq, completed)],
    );
    tokio::time::sleep(Duration::from_millis(900)).await;

    // Marker must be present. `list_stale_pending_dispatches` with
    // cutoff = now returns everything recorded before now.
    let pending = delivery_store
        .list_stale_pending_dispatches(std::time::SystemTime::now(), 16)
        .await
        .expect("list_stale_pending_dispatches");
    assert_eq!(
        pending.len(),
        1,
        "persistent list_configs outage MUST leave a pending-dispatch marker; got {pending:?}"
    );
    assert_eq!(pending[0].task_id, task_id);
    assert_eq!(pending[0].event_sequence, terminal_seq);

    // Simulate the reclaim sweep tick: drive redispatch. The
    // FailingPushStorage has one retry budget left, but the
    // redispatch's own retry will cycle past it.
    dispatcher
        .redispatch_pending(pending.into_iter().next().unwrap())
        .await;

    // After recovery, the marker is gone and wiremock saw one POST.
    let pending_after = delivery_store
        .list_stale_pending_dispatches(std::time::SystemTime::now(), 16)
        .await
        .expect("list after recovery");
    assert!(
        pending_after.is_empty(),
        "after successful redispatch the marker must be deleted; got {pending_after:?}"
    );

    drop(server);
}

#[tokio::test]
async fn initial_dispatch_retries_list_configs_on_transient_failure() {
    use turul_a2a::push::dispatcher::PushDispatcher;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        // With the retry, the third list_configs call succeeds,
        // configs fan out, and exactly one POST arrives.
        .expect(1)
        .mount(&server)
        .await;

    let inner = Arc::new(InMemoryA2aStorage::new());
    // Fail the first two list_configs calls; third succeeds.
    let push_storage = Arc::new(FailingPushStorage::new(inner.clone(), 0, 2));

    let delivery_store: Arc<dyn A2aPushDeliveryStore> = inner.clone();
    let mut cfg = PushDeliveryConfig::default();
    cfg.max_attempts = 1;
    cfg.backoff_base = Duration::from_millis(1);
    cfg.backoff_cap = Duration::from_millis(1);
    cfg.backoff_jitter = 0.0;
    cfg.request_timeout = Duration::from_secs(1);
    cfg.connect_timeout = Duration::from_millis(500);
    cfg.read_timeout = Duration::from_secs(1);
    cfg.claim_expiry = Duration::from_secs(60);
    cfg.max_payload_bytes = 64 * 1024;
    cfg.allow_insecure_urls = true;
    let worker = Arc::new(
        PushDeliveryWorker::new(delivery_store, cfg, None, "instance-ld".into())
            .expect("worker build"),
    );

    let task_storage: Arc<dyn turul_a2a::storage::A2aTaskStorage> = inner.clone();
    let push_storage_trait: Arc<dyn turul_a2a::storage::A2aPushNotificationStorage> =
        push_storage.clone();
    let dispatcher = PushDispatcher::new(worker, push_storage_trait, task_storage);

    let tenant = "tenant-ld";
    let task_id = "task-ld-1";
    let owner = "anonymous";
    let task = turul_a2a_types::Task::new(
        task_id,
        turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Completed),
    )
    .with_context_id("ctx-ld");
    let completed = turul_a2a::streaming::StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: task_id.into(),
            context_id: "ctx-ld".into(),
            status: serde_json::to_value(turul_a2a_types::TaskStatus::new(
                turul_a2a_types::TaskState::Completed,
            ))
            .unwrap(),
        },
    };
    use turul_a2a::storage::A2aAtomicStore;
    inner
        .create_task_with_events(tenant, owner, task.clone(), vec![])
        .await
        .expect("seed task");
    inner
        .create_config(
            tenant,
            turul_a2a_proto::TaskPushNotificationConfig {
                tenant: tenant.into(),
                id: "cfg-ld".into(),
                task_id: task_id.into(),
                url: format!("{}/webhook", server.uri()),
                token: String::new(),
                authentication: None,
            },
        )
        .await
        .expect("seed config");

    dispatcher.dispatch(tenant.into(), owner.into(), task, vec![(1, completed)]);

    // Wait for bounded retry (50+150+500 ms max) plus a small margin
    // for the POST to reach wiremock and the terminal write to land.
    tokio::time::sleep(Duration::from_millis(900)).await;

    assert_eq!(
        *push_storage.list_configs_calls.lock().unwrap(),
        3,
        "dispatch must retry list_configs three times before succeeding"
    );

    drop(server);
}
