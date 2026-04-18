//! Wiremock integration tests for the push delivery worker
//! (ADR-011 §13).
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
//! on `127.0.0.1`, which is private per ADR-011 SSRF rules. Backoff
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
use turul_a2a::storage::{A2aStorageError, InMemoryA2aStorage};
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
fn fast_worker(
    store: Arc<InMemoryA2aStorage>,
    max_attempts: u32,
) -> PushDeliveryWorker {
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
    PushDeliveryWorker::new(store, cfg, None, format!("instance-{}", uuid::Uuid::now_v7()))
        .expect("worker build must succeed")
}

fn sentinel_target(base_url: &str) -> PushTarget {
    PushTarget {
        tenant: "tenant-1".into(),
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
    assert_eq!(target.auth_credentials.expose().as_str(), SENTINEL_CREDENTIAL);
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
    // ADR-011 §5b pins this contract.
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
    fn resolve(
        &self,
        _host: &str,
        _port: u16,
    ) -> BoxFuture<'_, Result<Vec<IpAddr>, String>> {
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
// which at-least-once semantics already allow per ADR-011 §5a).
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
        self.inner.backend_name()
    }

    async fn claim_delivery(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        config_id: &str,
        claimant: &str,
        claim_expiry: Duration,
    ) -> Result<DeliveryClaim, A2aStorageError> {
        self.inner
            .claim_delivery(tenant, task_id, event_sequence, config_id, claimant, claim_expiry)
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
        self.inner.list_failed_deliveries(tenant, since, limit).await
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
    let worker = PushDeliveryWorker::new(store, cfg, None, "instance-fail".into())
        .expect("worker build");

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
    let worker = PushDeliveryWorker::new(store, cfg, None, "instance-blip".into())
        .expect("worker build");
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
    let worker = PushDeliveryWorker::new(store, cfg, None, "instance-stale".into())
        .expect("worker build");
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
// Unauthenticated config: no Authorization header (ADR-011 §4).
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

