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

use futures::future::BoxFuture;
use turul_a2a::push::claim::A2aPushDeliveryStore;
use turul_a2a::push::delivery::{
    DeliveryReport, PushDeliveryConfig, PushDeliveryWorker, PushDnsResolver, PushTarget,
};
use turul_a2a::push::secret::Secret;
use turul_a2a::push::{DeliveryErrorClass, GaveUpReason};
use turul_a2a::storage::InMemoryA2aStorage;
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

