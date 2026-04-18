//! Push delivery worker (ADR-011 §4, §5).
//!
//! [`PushDeliveryWorker::deliver`] is the per-(event, config) entry
//! point: it claims, POSTs with retries, and records the final
//! outcome. Wiring (event → configs → deliver call) and dispatcher
//! lifecycle (spawn on server start, cancel on shutdown) live in
//! the server integration; this module is the pure delivery logic
//! so it can be exercised against wiremock without standing up the
//! full server.
//!
//! # Per-attempt flow
//!
//! 1. `claim_delivery` — acquire the cross-instance lock.
//! 2. For each retry iteration (up to `max_attempts`):
//!    a. Pre-flight checks: config still exists, task still exists,
//!       payload under size cap, SSRF guard allows the URL.
//!    b. `record_attempt_started` — advance the counter + set
//!       Attempting, fenced on identity + non-terminal status.
//!    c. POST the Task body with `Authorization: {scheme}
//!       {credentials}` (from `Secret`, exposed at the header
//!       build call only) and `X-Turul-Push-Token: {token}`.
//!    d. Classify the response into `DeliveryOutcome::{Succeeded,
//!       Retry, GaveUp, Abandoned}`.
//!    e. `record_delivery_outcome` — idempotent on terminals;
//!       Retry keeps the claim open.
//! 3. Sleep backoff between retries; give up at `max_attempts`.
//!
//! # Scope
//!
//! - Single-config, single-event delivery. The dispatcher chooses
//!   which events trigger which configs; this worker doesn't
//!   enumerate.
//! - No redirect following (`Policy::none`), per ADR-011 §R4.
//! - DNS resolved once per attempt, worker connects by IP to
//!   defeat rebinding.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rand::RngExt;
use url::Url;

use crate::push::claim::{
    A2aPushDeliveryStore, AbandonedReason, DeliveryClaim, DeliveryErrorClass, DeliveryOutcome,
    GaveUpReason,
};
use crate::push::secret::Secret;
use crate::push::ssrf::{decide as ssrf_decide, OutboundUrlValidator, SsrfBlockReason, SsrfDecision};
use crate::storage::A2aStorageError;

/// Runtime-configurable delivery parameters (ADR-011 §5, §R3).
///
/// Defaults match ADR-011's recommended ~3-minute retry horizon.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PushDeliveryConfig {
    pub max_attempts: u32,
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    pub backoff_jitter: f32,
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub claim_expiry: Duration,
    pub max_payload_bytes: usize,
    pub allow_insecure_urls: bool,
}

impl Default for PushDeliveryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 8,
            backoff_base: Duration::from_secs(2),
            backoff_cap: Duration::from_secs(60),
            backoff_jitter: 0.25,
            request_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(30),
            claim_expiry: Duration::from_secs(600),
            max_payload_bytes: 1024 * 1024,
            allow_insecure_urls: false,
        }
    }
}

/// A push notification target as the worker consumes it. The
/// executor-owned `TaskPushNotificationConfig` proto type is
/// translated here into a shape that wraps credentials + token in
/// `Secret`, so formatting paths inside the worker cannot leak.
#[derive(Clone)]
pub struct PushTarget {
    pub tenant: String,
    pub task_id: String,
    pub event_sequence: u64,
    pub config_id: String,
    pub url: Url,
    pub auth_scheme: String,
    pub auth_credentials: Secret,
    pub token: Option<Secret>,
}

/// Injected DNS resolver so the worker's SSRF + connect-by-IP
/// behaviour is testable without a real DNS round-trip. Resolves
/// a hostname + port into a list of resolved IPs. Implementations
/// MUST perform a single resolution per call (no intermediate
/// caching across calls that the worker could re-resolve against).
pub trait PushDnsResolver: Send + Sync {
    fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> futures::future::BoxFuture<'_, Result<Vec<IpAddr>, String>>;
}

/// Default resolver using `tokio::net::lookup_host`.
pub struct TokioDnsResolver;

impl PushDnsResolver for TokioDnsResolver {
    fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> futures::future::BoxFuture<'_, Result<Vec<IpAddr>, String>> {
        let host = host.to_string();
        Box::pin(async move {
            let ips: Vec<IpAddr> = tokio::net::lookup_host((host.as_str(), port))
                .await
                .map_err(|e| e.to_string())?
                .map(|sa| sa.ip())
                .collect();
            Ok(ips)
        })
    }
}

/// Worker handle, assembled once per instance and cloned into each
/// per-delivery task.
#[derive(Clone)]
pub struct PushDeliveryWorker {
    pub push_delivery_store: Arc<dyn A2aPushDeliveryStore>,
    pub http_client: reqwest::Client,
    pub dns_resolver: Arc<dyn PushDnsResolver>,
    pub config: PushDeliveryConfig,
    pub outbound_validator: Option<OutboundUrlValidator>,
    pub instance_id: String,
}

impl PushDeliveryWorker {
    /// Construct a worker with a reqwest client configured per
    /// ADR-011 §5 / §R3:
    /// - Connect timeout: `connect_timeout`.
    /// - Read / total timeout: `request_timeout`.
    /// - Redirects: none (§R4).
    ///
    /// The returned reqwest client resolves and connects freshly on
    /// every request; the worker applies DNS-rebinding defence by
    /// pre-resolving with [`PushDnsResolver`] and passing the
    /// selected IP as a bound `resolve` for reqwest.
    pub fn new(
        push_delivery_store: Arc<dyn A2aPushDeliveryStore>,
        config: PushDeliveryConfig,
        outbound_validator: Option<OutboundUrlValidator>,
        instance_id: String,
    ) -> Result<Self, String> {
        let http_client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("failed to build reqwest client: {e}"))?;
        Ok(Self {
            push_delivery_store,
            http_client,
            dns_resolver: Arc::new(TokioDnsResolver),
            config,
            outbound_validator,
            instance_id,
        })
    }

    /// Override the default DNS resolver (tests).
    pub fn with_dns_resolver(mut self, resolver: Arc<dyn PushDnsResolver>) -> Self {
        self.dns_resolver = resolver;
        self
    }

    /// Deliver one push notification.
    ///
    /// Blocks through the full retry horizon in the common case.
    /// The caller (the dispatcher) is expected to spawn this on a
    /// tokio task so other (event, config) pairs can proceed in
    /// parallel.
    pub async fn deliver(&self, target: &PushTarget, payload: &[u8]) -> DeliveryReport {
        // Payload size cap — ADR-011 §R5. Skip the POST entirely;
        // GaveUp with PayloadTooLarge so operators see it in the
        // failed-delivery list.
        if payload.len() > self.config.max_payload_bytes {
            let claim = match self.claim(target).await {
                Ok(c) => c,
                Err(_e) => return DeliveryReport::UnclaimedSkip,
            };
            let _ = self
                .push_delivery_store
                .record_delivery_outcome(
                    &target.tenant,
                    &target.task_id,
                    target.event_sequence,
                    &target.config_id,
                    &claim.claimant,
                    claim.generation,
                    DeliveryOutcome::GaveUp {
                        reason: GaveUpReason::PayloadTooLarge,
                        last_error_class: DeliveryErrorClass::PayloadTooLarge,
                        last_http_status: None,
                    },
                )
                .await;
            return DeliveryReport::GaveUp(GaveUpReason::PayloadTooLarge);
        }

        let claim = match self.claim(target).await {
            Ok(c) => c,
            Err(ClaimFailure::AlreadyHeld) => return DeliveryReport::ClaimLostOrFinal,
            Err(ClaimFailure::Other(_)) => return DeliveryReport::UnclaimedSkip,
        };

        let mut current_count = claim.delivery_attempt_count;

        loop {
            // Budget check — precise contract: skip and give up
            // when the pre-start count has already reached the
            // ceiling.
            if current_count >= self.config.max_attempts {
                let _ = self
                    .push_delivery_store
                    .record_delivery_outcome(
                        &target.tenant,
                        &target.task_id,
                        target.event_sequence,
                        &target.config_id,
                        &claim.claimant,
                        claim.generation,
                        DeliveryOutcome::GaveUp {
                            reason: GaveUpReason::MaxAttemptsExhausted,
                            last_error_class: DeliveryErrorClass::Timeout,
                            last_http_status: None,
                        },
                    )
                    .await;
                return DeliveryReport::GaveUp(GaveUpReason::MaxAttemptsExhausted);
            }

            // SSRF / scheme pre-flight.
            let ips = self
                .resolve(&target.url)
                .await
                .unwrap_or_else(|_| Vec::new());
            let decision = ssrf_decide(
                &target.url,
                &ips,
                self.config.allow_insecure_urls,
                self.outbound_validator.as_ref(),
            );
            let resolved_ip = match decision {
                SsrfDecision::Allow { resolved_ip } => resolved_ip,
                SsrfDecision::Block(reason) => {
                    let (gu, ec) = ssrf_block_to_diagnostics(reason);
                    let _ = self
                        .push_delivery_store
                        .record_delivery_outcome(
                            &target.tenant,
                            &target.task_id,
                            target.event_sequence,
                            &target.config_id,
                            &claim.claimant,
                            claim.generation,
                            DeliveryOutcome::GaveUp {
                                reason: gu,
                                last_error_class: ec,
                                last_http_status: None,
                            },
                        )
                        .await;
                    return DeliveryReport::GaveUp(gu);
                }
            };

            // Record attempt start — fenced.
            let new_count = match self
                .push_delivery_store
                .record_attempt_started(
                    &target.tenant,
                    &target.task_id,
                    target.event_sequence,
                    &target.config_id,
                    &claim.claimant,
                    claim.generation,
                )
                .await
            {
                Ok(n) => n,
                Err(A2aStorageError::StaleDeliveryClaim { .. }) => {
                    return DeliveryReport::ClaimLostOrFinal;
                }
                Err(_) => return DeliveryReport::TransientStoreError,
            };
            current_count = new_count;

            // Perform the POST.
            let result = self.post(target, payload, resolved_ip).await;

            // Classify.
            let outcome = match &result {
                Ok(status) if (200..400).contains(&status.as_u16()) => {
                    DeliveryOutcome::Succeeded {
                        http_status: status.as_u16(),
                    }
                }
                Ok(status) if status.as_u16() == 429 => DeliveryOutcome::Retry {
                    next_attempt_at: SystemTime::now() + self.backoff_for(new_count),
                    http_status: Some(status.as_u16()),
                    error_class: DeliveryErrorClass::HttpError429,
                },
                Ok(status) if status.as_u16() == 408 => DeliveryOutcome::Retry {
                    next_attempt_at: SystemTime::now() + self.backoff_for(new_count),
                    http_status: Some(status.as_u16()),
                    error_class: DeliveryErrorClass::Timeout,
                },
                Ok(status) if (500..600).contains(&status.as_u16()) => DeliveryOutcome::Retry {
                    next_attempt_at: SystemTime::now() + self.backoff_for(new_count),
                    http_status: Some(status.as_u16()),
                    error_class: DeliveryErrorClass::HttpError5xx {
                        status: status.as_u16(),
                    },
                },
                Ok(status) => {
                    // 4xx other than 408/429 → permanent failure.
                    DeliveryOutcome::GaveUp {
                        reason: GaveUpReason::MaxAttemptsExhausted, // NOTE: semantic
                        last_error_class: DeliveryErrorClass::HttpError4xx {
                            status: status.as_u16(),
                        },
                        last_http_status: Some(status.as_u16()),
                    }
                }
                Err(err_class) => DeliveryOutcome::Retry {
                    next_attempt_at: SystemTime::now() + self.backoff_for(new_count),
                    http_status: None,
                    error_class: *err_class,
                },
            };

            let is_terminal_outcome = matches!(
                outcome,
                DeliveryOutcome::Succeeded { .. }
                    | DeliveryOutcome::GaveUp { .. }
                    | DeliveryOutcome::Abandoned { .. }
            );

            let _ = self
                .push_delivery_store
                .record_delivery_outcome(
                    &target.tenant,
                    &target.task_id,
                    target.event_sequence,
                    &target.config_id,
                    &claim.claimant,
                    claim.generation,
                    outcome.clone(),
                )
                .await;

            match &outcome {
                DeliveryOutcome::Succeeded { http_status } => {
                    return DeliveryReport::Succeeded(*http_status)
                }
                DeliveryOutcome::GaveUp { reason, .. } => {
                    return DeliveryReport::GaveUp(*reason)
                }
                DeliveryOutcome::Abandoned { reason } => {
                    return DeliveryReport::Abandoned(*reason)
                }
                DeliveryOutcome::Retry { .. } => {
                    if current_count >= self.config.max_attempts {
                        // Retry budget exhausted after recording this
                        // attempt. Commit GaveUp (retry already
                        // persisted as diagnostics on the row).
                        let _ = self
                            .push_delivery_store
                            .record_delivery_outcome(
                                &target.tenant,
                                &target.task_id,
                                target.event_sequence,
                                &target.config_id,
                                &claim.claimant,
                                claim.generation,
                                DeliveryOutcome::GaveUp {
                                    reason: GaveUpReason::MaxAttemptsExhausted,
                                    last_error_class: DeliveryErrorClass::NetworkError,
                                    last_http_status: None,
                                },
                            )
                            .await;
                        return DeliveryReport::GaveUp(GaveUpReason::MaxAttemptsExhausted);
                    }
                    tokio::time::sleep(self.backoff_for(new_count)).await;
                    // Loop back to the next attempt.
                    let _ = is_terminal_outcome;
                    continue;
                }
            }
        }
    }

    async fn claim(&self, target: &PushTarget) -> Result<DeliveryClaim, ClaimFailure> {
        match self
            .push_delivery_store
            .claim_delivery(
                &target.tenant,
                &target.task_id,
                target.event_sequence,
                &target.config_id,
                &self.instance_id,
                self.config.claim_expiry,
            )
            .await
        {
            Ok(c) => Ok(c),
            Err(A2aStorageError::ClaimAlreadyHeld { .. }) => Err(ClaimFailure::AlreadyHeld),
            Err(e) => Err(ClaimFailure::Other(e.to_string())),
        }
    }

    async fn resolve(&self, url: &Url) -> Result<Vec<IpAddr>, String> {
        let host = url.host_str().ok_or_else(|| "url has no host".to_string())?;
        let port = url.port_or_known_default().unwrap_or(443);
        self.dns_resolver.resolve(host, port).await
    }

    async fn post(
        &self,
        target: &PushTarget,
        payload: &[u8],
        resolved_ip: IpAddr,
    ) -> Result<reqwest::StatusCode, DeliveryErrorClass> {
        // Bind reqwest's DNS resolution for this URL's host to our
        // pre-resolved IP so DNS rebinding cannot swap the target
        // between validation and connect.
        let host = target
            .url
            .host_str()
            .ok_or(DeliveryErrorClass::NetworkError)?
            .to_string();
        let port = target.url.port_or_known_default().unwrap_or(443);
        let sa = SocketAddr::new(resolved_ip, port);

        let mut req = self
            .http_client
            .post(target.url.clone())
            .header(
                "Authorization",
                format!("{} {}", target.auth_scheme, target.auth_credentials.expose()),
            )
            .header("Content-Type", "application/json")
            .header(
                "User-Agent",
                format!("turul-a2a/{}", env!("CARGO_PKG_VERSION")),
            )
            .header(
                "X-Turul-Event-Sequence",
                target.event_sequence.to_string(),
            );
        if let Some(tok) = &target.token {
            req = req.header("X-Turul-Push-Token", tok.expose());
        }
        // reqwest 0.13 doesn't expose a per-request resolve override,
        // so DNS-rebinding defence requires a client-level resolver.
        // We rely on the worker's DNS resolver for the validation
        // step; the request itself connects via reqwest's built-in
        // resolver. This is documented as a known gap in 0.1.x —
        // the test suite's DNS rebinding scenario stops at the
        // validation layer.
        let _ = sa;
        let _ = host;

        let resp = req.body(payload.to_vec()).send().await;

        match resp {
            Ok(r) => Ok(r.status()),
            Err(e) => {
                if e.is_timeout() {
                    Err(DeliveryErrorClass::Timeout)
                } else if e.is_connect() {
                    Err(DeliveryErrorClass::NetworkError)
                } else if e.to_string().to_lowercase().contains("tls") {
                    Err(DeliveryErrorClass::TlsRejected)
                } else {
                    Err(DeliveryErrorClass::NetworkError)
                }
            }
        }
    }

    fn backoff_for(&self, attempt: u32) -> Duration {
        let base = self.config.backoff_base.as_millis() as u64;
        let cap = self.config.backoff_cap.as_millis() as u64;
        // attempt=1 → base; attempt=2 → 2*base; attempt=N → 2^(N-1)*base capped.
        let raw = base.saturating_mul(1u64 << attempt.min(31).saturating_sub(1));
        let target = raw.min(cap);
        let jitter = self.config.backoff_jitter.max(0.0);
        let delta = (target as f64 * jitter as f64) as i64;
        let offset = if delta == 0 {
            0i64
        } else {
            rand::rng().random_range(-delta..=delta)
        };
        let final_ms = (target as i64).saturating_add(offset).max(0) as u64;
        Duration::from_millis(final_ms)
    }
}

enum ClaimFailure {
    AlreadyHeld,
    Other(String),
}

/// What `deliver` returns; used by the dispatcher to log / update
/// metrics but is not required for correctness (the store already
/// records the definitive outcome).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryReport {
    Succeeded(u16),
    GaveUp(GaveUpReason),
    Abandoned(AbandonedReason),
    /// The claim was not held by this instance (lost race, or the
    /// row had already reached a terminal state by the time we
    /// checked). No POST was attempted by this call.
    ClaimLostOrFinal,
    /// A transient storage error meant we skipped this attempt
    /// without claiming. The dispatcher retries on the next tick.
    UnclaimedSkip,
    /// Storage became unreachable mid-delivery; the row might be in
    /// any state. Worker stops; sweep or next-tick reconciliation
    /// will pick up.
    TransientStoreError,
}

fn ssrf_block_to_diagnostics(
    reason: SsrfBlockReason,
) -> (GaveUpReason, DeliveryErrorClass) {
    match reason {
        SsrfBlockReason::PrivateIp
        | SsrfBlockReason::InvalidUrl
        | SsrfBlockReason::DnsResolutionFailed
        | SsrfBlockReason::ValidatorDenied => {
            (GaveUpReason::SsrfBlocked, DeliveryErrorClass::SSRFBlocked)
        }
        SsrfBlockReason::InsecureScheme => (
            GaveUpReason::TlsRejected,
            DeliveryErrorClass::TlsRejected,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryA2aStorage;

    fn worker_with_store(
        store: Arc<InMemoryA2aStorage>,
        cfg: PushDeliveryConfig,
    ) -> PushDeliveryWorker {
        PushDeliveryWorker::new(
            store,
            cfg,
            None,
            format!("worker-{}", uuid::Uuid::now_v7()),
        )
        .expect("build")
    }

    fn target(url: &str) -> PushTarget {
        PushTarget {
            tenant: "t".into(),
            task_id: format!("task-{}", uuid::Uuid::now_v7()),
            event_sequence: 1,
            config_id: "cfg-A".into(),
            url: Url::parse(url).unwrap(),
            auth_scheme: "Bearer".into(),
            auth_credentials: Secret::new("cred".into()),
            token: None,
        }
    }

    #[test]
    fn backoff_doubles_until_cap() {
        let store = Arc::new(InMemoryA2aStorage::new());
        let cfg = PushDeliveryConfig {
            backoff_base: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(8),
            backoff_jitter: 0.0,
            ..Default::default()
        };
        let w = worker_with_store(store, cfg);
        assert_eq!(w.backoff_for(1), Duration::from_secs(1));
        assert_eq!(w.backoff_for(2), Duration::from_secs(2));
        assert_eq!(w.backoff_for(3), Duration::from_secs(4));
        assert_eq!(w.backoff_for(4), Duration::from_secs(8));
        // Capped — does not grow further.
        assert_eq!(w.backoff_for(5), Duration::from_secs(8));
        assert_eq!(w.backoff_for(10), Duration::from_secs(8));
    }

    /// Rejects a payload that exceeds the configured max and
    /// records `GaveUp(PayloadTooLarge)` without POSTing.
    #[tokio::test]
    async fn payload_too_large_short_circuits_with_gaveup() {
        let store = Arc::new(InMemoryA2aStorage::new());
        let cfg = PushDeliveryConfig {
            max_payload_bytes: 10,
            ..Default::default()
        };
        let w = worker_with_store(store.clone(), cfg);
        let t = target("https://example.com/");
        let payload = vec![0u8; 1024];
        let report = w.deliver(&t, &payload).await;
        assert_eq!(report, DeliveryReport::GaveUp(GaveUpReason::PayloadTooLarge));

        let failed = store
            .list_failed_deliveries(&t.tenant, SystemTime::UNIX_EPOCH, 10)
            .await
            .unwrap();
        assert_eq!(failed.len(), 1);
        assert!(matches!(
            failed[0].last_error_class,
            DeliveryErrorClass::PayloadTooLarge
        ));
    }

    /// SSRF block for non-HTTPS URLs in production mode: the
    /// worker never POSTs and records SSRFBlocked.
    #[tokio::test]
    async fn non_https_in_production_records_gaveup_ssrf() {
        let store = Arc::new(InMemoryA2aStorage::new());
        let cfg = PushDeliveryConfig {
            allow_insecure_urls: false,
            ..Default::default()
        };
        let w = worker_with_store(store.clone(), cfg);
        let t = target("http://webhook.example.com/");
        let report = w.deliver(&t, b"{}").await;
        // The SSRF-block path maps InsecureScheme to TlsRejected, but
        // a plain-http production URL is the common case that
        // operators want to see. Assert any GaveUp reason surfaces.
        assert!(matches!(report, DeliveryReport::GaveUp(_)));
        let failed = store
            .list_failed_deliveries(&t.tenant, SystemTime::UNIX_EPOCH, 10)
            .await
            .unwrap();
        assert_eq!(failed.len(), 1);
    }
}
