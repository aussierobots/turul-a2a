//! A2aServer builder and runtime.

pub mod in_flight;
pub mod obs;
pub(crate) mod spawn;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::error::A2aError;
use crate::executor::AgentExecutor;
use crate::middleware::{A2aMiddleware, MiddlewareStack, SecurityContribution};
use crate::router::{build_router, AppState};
use crate::storage::{A2aAtomicStore, A2aPushNotificationStorage, A2aTaskStorage, InMemoryA2aStorage};
use crate::streaming::TaskEventBroker;

// ---------------------------------------------------------------------------
// Runtime configuration
// ---------------------------------------------------------------------------

/// Runtime tuning knobs for advanced task lifecycle behaviors.
///
/// Groups: blocking-send two-deadline timeouts
/// (`blocking_task_timeout`, `timeout_abort_grace`); cancellation
/// handler grace / poll intervals and cross-instance cancel-marker
/// poll interval (`cancel_handler_*`,
/// `cross_instance_cancel_poll_interval`); push delivery tuning
/// (`push_*`, `allow_insecure_push_urls`).
///
/// Exposed publicly so tests and advanced consumers can construct
/// [`crate::router::AppState`] directly (e.g., router/transport-level
/// integration tests). The `#[non_exhaustive]` marker reserves the right
/// to add fields in patch releases; callers should use
/// [`RuntimeConfig::default()`] as the construction base and modify
/// specific fields rather than struct-literal construction.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RuntimeConfig {
    pub blocking_task_timeout: Duration,
    pub timeout_abort_grace: Duration,
    pub cancel_handler_grace: Duration,
    pub cancel_handler_poll_interval: Duration,
    pub cross_instance_cancel_poll_interval: Duration,

    pub push_max_attempts: usize,
    pub push_backoff_base: Duration,
    pub push_backoff_cap: Duration,
    pub push_backoff_jitter: f32,
    pub push_request_timeout: Duration,
    pub push_connect_timeout: Duration,
    pub push_read_timeout: Duration,
    pub push_claim_expiry: Duration,
    pub push_config_cache_ttl: Duration,
    pub push_failed_delivery_retention: Duration,
    pub push_max_payload_bytes: usize,
    pub allow_insecure_push_urls: bool,

    /// Interval between reclaim-and-redispatch sweeps (ADR-011 §10.5).
    /// The server background task enumerates expired-but-non-terminal
    /// claim rows via
    /// [`crate::push::A2aPushDeliveryStore::list_reclaimable_claims`]
    /// and drives each through
    /// [`crate::push::PushDispatcher::redispatch_one`]. Shorter
    /// cadence recovers stuck rows faster but increases steady-state
    /// load; default 60s balances recovery latency against scan cost.
    pub push_reclaim_sweep_interval: Duration,

    /// Max reclaimable rows to pull per sweep tick. The sweeper
    /// paginates: each tick fetches up to this many rows, redispatches
    /// them, and returns. The next tick picks up any remainder.
    pub push_reclaim_sweep_batch: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            blocking_task_timeout: Duration::from_secs(30),
            timeout_abort_grace: Duration::from_secs(5),
            cancel_handler_grace: Duration::from_secs(5),
            cancel_handler_poll_interval: Duration::from_millis(100),
            cross_instance_cancel_poll_interval: Duration::from_secs(1),
            push_max_attempts: 8,
            push_backoff_base: Duration::from_secs(2),
            push_backoff_cap: Duration::from_secs(60),
            push_backoff_jitter: 0.25,
            push_request_timeout: Duration::from_secs(30),
            push_connect_timeout: Duration::from_secs(5),
            push_read_timeout: Duration::from_secs(30),
            push_claim_expiry: Duration::from_secs(10 * 60),
            push_config_cache_ttl: Duration::from_secs(5),
            push_failed_delivery_retention: Duration::from_secs(7 * 24 * 60 * 60),
            push_max_payload_bytes: 1024 * 1024,
            allow_insecure_push_urls: false,
            push_reclaim_sweep_interval: Duration::from_secs(60),
            push_reclaim_sweep_batch: 64,
        }
    }
}

/// Builder for configuring and running an A2A server.
pub struct A2aServerBuilder {
    executor: Option<Arc<dyn AgentExecutor>>,
    task_storage: Option<Arc<dyn A2aTaskStorage>>,
    push_storage: Option<Arc<dyn A2aPushNotificationStorage>>,
    event_store: Option<Arc<dyn crate::storage::A2aEventStore>>,
    atomic_store: Option<Arc<dyn A2aAtomicStore>>,
    cancellation_supervisor: Option<Arc<dyn crate::storage::A2aCancellationSupervisor>>,
    push_delivery_store: Option<Arc<dyn crate::push::A2aPushDeliveryStore>>,
    bind_addr: SocketAddr,
    middleware: Vec<Arc<dyn A2aMiddleware>>,
    runtime_config: RuntimeConfig,
}

impl A2aServerBuilder {
    pub fn new() -> Self {
        Self {
            executor: None,
            task_storage: None,
            push_storage: None,
            event_store: None,
            atomic_store: None,
            cancellation_supervisor: None,
            push_delivery_store: None,
            bind_addr: ([0, 0, 0, 0], 3000).into(),
            middleware: vec![],
            runtime_config: RuntimeConfig::default(),
        }
    }

    // -----------------------------------------------------------------
    // Runtime-config setters.
    //
    // Each setter updates the internal [`RuntimeConfig`] and returns
    // `self`. Defaults are in [`RuntimeConfig::default`].
    // -----------------------------------------------------------------

    /// Soft timeout for blocking `SendMessage` requests. On expiry the
    /// framework trips the executor's cancellation token and waits up to
    /// [`Self::timeout_abort_grace`] for cooperative exit. 
    pub fn blocking_task_timeout(mut self, d: Duration) -> Self {
        self.runtime_config.blocking_task_timeout = d;
        self
    }

    /// Grace window between soft cancellation and hard `JoinHandle::abort()`.
    /// 
    pub fn timeout_abort_grace(mut self, d: Duration) -> Self {
        self.runtime_config.timeout_abort_grace = d;
        self
    }

    /// How long the `CancelTask` handler waits for cancellation to resolve
    /// before force-committing CANCELED itself. 
    pub fn cancel_handler_grace(mut self, d: Duration) -> Self {
        self.runtime_config.cancel_handler_grace = d;
        self
    }

    /// Poll interval used inside the `CancelTask` handler's grace window to
    /// re-read task state from storage. 
    pub fn cancel_handler_poll_interval(mut self, d: Duration) -> Self {
        self.runtime_config.cancel_handler_poll_interval = d;
        self
    }

    /// How often the in-flight supervisor batch-polls the cancel marker
    /// across in-flight tasks for cross-instance cancel propagation.
    /// 
    pub fn cross_instance_cancel_poll_interval(mut self, d: Duration) -> Self {
        self.runtime_config.cross_instance_cancel_poll_interval = d;
        self
    }

    /// Maximum retry attempts (including first) per push delivery.
    /// 
    pub fn push_max_attempts(mut self, n: usize) -> Self {
        self.runtime_config.push_max_attempts = n;
        self
    }

    /// Base delay before the second push attempt; doubles up to
    /// [`Self::push_backoff_cap`]. 
    pub fn push_backoff_base(mut self, d: Duration) -> Self {
        self.runtime_config.push_backoff_base = d;
        self
    }

    /// Maximum single-wait in the push retry schedule. 
    pub fn push_backoff_cap(mut self, d: Duration) -> Self {
        self.runtime_config.push_backoff_cap = d;
        self
    }

    /// Jitter fraction applied to push retry waits. 
    pub fn push_backoff_jitter(mut self, j: f32) -> Self {
        self.runtime_config.push_backoff_jitter = j;
        self
    }

    /// Total per-request timeout for a push POST. 
    pub fn push_request_timeout(mut self, d: Duration) -> Self {
        self.runtime_config.push_request_timeout = d;
        self
    }

    /// Connect-phase timeout for a push POST. 
    pub fn push_connect_timeout(mut self, d: Duration) -> Self {
        self.runtime_config.push_connect_timeout = d;
        self
    }

    /// Read-phase timeout for a push POST. 
    pub fn push_read_timeout(mut self, d: Duration) -> Self {
        self.runtime_config.push_read_timeout = d;
        self
    }

    /// Claim expiry for the push delivery claim table. Must exceed the
    /// retry horizon implied by `push_max_attempts` + `push_backoff_cap`;
    /// the push-delivery builder validates this on `build()`.
    pub fn push_claim_expiry(mut self, d: Duration) -> Self {
        self.runtime_config.push_claim_expiry = d;
        self
    }

    /// Push-config cache TTL inside the delivery worker. 
    pub fn push_config_cache_ttl(mut self, d: Duration) -> Self {
        self.runtime_config.push_config_cache_ttl = d;
        self
    }

    /// Retention for failed push delivery records (operator inspection).
    /// 
    pub fn push_failed_delivery_retention(mut self, d: Duration) -> Self {
        self.runtime_config.push_failed_delivery_retention = d;
        self
    }

    /// Maximum serialized Task body size for a push POST. 
    pub fn push_max_payload_bytes(mut self, bytes: usize) -> Self {
        self.runtime_config.push_max_payload_bytes = bytes;
        self
    }

    /// Dev-only escape hatch: permit `http://` webhook URLs AND bypass the
    /// private-IP blocklist. Required for localhost wiremock testing.
    /// Default false.
    pub fn allow_insecure_push_urls(mut self, allow: bool) -> Self {
        self.runtime_config.allow_insecure_push_urls = allow;
        self
    }

    /// Interval between reclaim-and-redispatch sweeps. Default 60s.
    pub fn push_reclaim_sweep_interval(mut self, d: Duration) -> Self {
        self.runtime_config.push_reclaim_sweep_interval = d;
        self
    }

    /// Max rows pulled per reclaim sweep tick. Default 64.
    pub fn push_reclaim_sweep_batch(mut self, n: usize) -> Self {
        self.runtime_config.push_reclaim_sweep_batch = n;
        self
    }

    pub fn executor(mut self, executor: impl AgentExecutor + 'static) -> Self {
        self.executor = Some(Arc::new(executor));
        self
    }

    /// Set all storage from a single backend instance.
    ///
    /// This is the **preferred** method — a single struct implementing all storage
    /// traits guarantees the same-backend requirement (ADR-009). Works with any
    /// backend: `InMemoryA2aStorage`, `SqliteA2aStorage`, `PostgresA2aStorage`,
    /// `DynamoDbA2aStorage`, etc.
    pub fn storage<S>(mut self, storage: S) -> Self
    where
        S: A2aTaskStorage
            + A2aPushNotificationStorage
            + crate::storage::A2aEventStore
            + A2aAtomicStore
            + crate::storage::A2aCancellationSupervisor
            + crate::push::A2aPushDeliveryStore
            + Clone
            + 'static,
    {
        self.task_storage = Some(Arc::new(storage.clone()));
        self.push_storage = Some(Arc::new(storage.clone()));
        self.event_store = Some(Arc::new(storage.clone()));
        self.atomic_store = Some(Arc::new(storage.clone()));
        self.cancellation_supervisor = Some(Arc::new(storage.clone()));
        self.push_delivery_store = Some(Arc::new(storage));
        self
    }

    /// Set the cancellation supervisor individually. Prefer `.storage()`
    /// for ADR-009 same-backend compliance. Consumed by the `:cancel`
    /// handler for cross-instance marker reads.
    pub fn cancellation_supervisor(
        mut self,
        supervisor: impl crate::storage::A2aCancellationSupervisor + 'static,
    ) -> Self {
        self.cancellation_supervisor = Some(Arc::new(supervisor));
        self
    }

    /// Set task storage individually. Prefer `.storage()` for ADR-009 compliance.
    pub fn task_storage(mut self, storage: impl A2aTaskStorage + 'static) -> Self {
        self.task_storage = Some(Arc::new(storage));
        self
    }

    /// Set push notification storage individually. Prefer `.storage()` for ADR-009 compliance.
    pub fn push_storage(mut self, storage: impl A2aPushNotificationStorage + 'static) -> Self {
        self.push_storage = Some(Arc::new(storage));
        self
    }

    /// Set event store individually. Prefer `.storage()` for ADR-009 compliance.
    pub fn event_store(mut self, store: impl crate::storage::A2aEventStore + 'static) -> Self {
        self.event_store = Some(Arc::new(store));
        self
    }

    /// Set atomic store individually. Prefer `.storage()` for ADR-009 compliance.
    pub fn atomic_store(mut self, store: impl A2aAtomicStore + 'static) -> Self {
        self.atomic_store = Some(Arc::new(store));
        self
    }

    /// Set the push delivery claim store individually (ADR-011 §10).
    ///
    /// Required when push-notification delivery is enabled in the
    /// deployment. `.storage()` wires this automatically from a unified
    /// backend; use this setter only for mixed-backend tests or when
    /// running against an external push-coordination service. Prefer
    /// `.storage()` for ADR-009 same-backend compliance.
    pub fn push_delivery_store(
        mut self,
        store: impl crate::push::A2aPushDeliveryStore + 'static,
    ) -> Self {
        self.push_delivery_store = Some(Arc::new(store));
        self
    }

    pub fn bind(mut self, addr: impl Into<SocketAddr>) -> Self {
        self.bind_addr = addr.into();
        self
    }

    /// Add auth middleware. Multiple calls stack (AND semantics).
    /// Use `AnyOfMiddleware` for OR semantics.
    pub fn middleware(mut self, mw: Arc<dyn A2aMiddleware>) -> Self {
        self.middleware.push(mw);
        self
    }

    pub fn build(self) -> Result<A2aServer, A2aError> {
        let executor = self
            .executor
            .ok_or(A2aError::Internal("executor is required".into()))?;

        let default_storage = InMemoryA2aStorage::new();
        let task_storage = self
            .task_storage
            .unwrap_or_else(|| Arc::new(default_storage.clone()));
        let push_storage = self
            .push_storage
            .unwrap_or_else(|| Arc::new(default_storage.clone()));
        let event_store: Arc<dyn crate::storage::A2aEventStore> = self
            .event_store
            .unwrap_or_else(|| Arc::new(default_storage.clone()));
        let atomic_store: Arc<dyn A2aAtomicStore> = self
            .atomic_store
            .unwrap_or_else(|| Arc::new(default_storage.clone()));
        let cancellation_supervisor: Arc<dyn crate::storage::A2aCancellationSupervisor> = self
            .cancellation_supervisor
            .unwrap_or_else(|| Arc::new(default_storage.clone()));
        let push_delivery_store: Option<Arc<dyn crate::push::A2aPushDeliveryStore>> =
            self.push_delivery_store;

        // Same-backend enforcement (ADR-009): all storage traits must share the same backend.
        let task_backend = task_storage.backend_name();
        let push_backend = push_storage.backend_name();
        let event_backend = event_store.backend_name();
        let atomic_backend = atomic_store.backend_name();
        let supervisor_backend = cancellation_supervisor.backend_name();
        if task_backend != push_backend
            || task_backend != event_backend
            || task_backend != atomic_backend
            || task_backend != supervisor_backend
        {
            return Err(A2aError::Internal(format!(
                "Storage backend mismatch: task={task_backend}, push={push_backend}, \
                 event={event_backend}, atomic={atomic_backend}, \
                 cancellation_supervisor={supervisor_backend}. \
                 ADR-009 requires all storage traits to share the same backend."
            )));
        }
        let push_dispatcher: Option<Arc<crate::push::PushDispatcher>> =
            if let Some(push_delivery) = push_delivery_store.as_ref() {
                let push_delivery_backend = push_delivery.backend_name();
                if task_backend != push_delivery_backend {
                    return Err(A2aError::Internal(format!(
                        "Storage backend mismatch: task={task_backend}, \
                         push_delivery={push_delivery_backend}. \
                         ADR-009 requires all storage traits to share the same backend."
                    )));
                }

                // ADR-011 §10.3: claim expiry must exceed the worst-case
                // retry horizon so a claim held by a healthy worker is
                // never mis-classified as stale mid-retry. Upper bound
                // for the horizon is `max_attempts * backoff_cap`
                // (every attempt waits the cap), which is conservative
                // and independent of jitter.
                let retry_horizon = self
                    .runtime_config
                    .push_backoff_cap
                    .saturating_mul(self.runtime_config.push_max_attempts as u32);
                if self.runtime_config.push_claim_expiry <= retry_horizon {
                    return Err(A2aError::Internal(format!(
                        "push_claim_expiry ({:?}) must be greater than retry horizon \
                         (push_max_attempts={} * push_backoff_cap={:?} = {:?}). \
                         Raise push_claim_expiry or lower push_max_attempts/push_backoff_cap.",
                        self.runtime_config.push_claim_expiry,
                        self.runtime_config.push_max_attempts,
                        self.runtime_config.push_backoff_cap,
                        retry_horizon
                    )));
                }

                // Build the push-delivery worker + dispatcher now that
                // we've validated the horizon. Runtime config carries
                // the full worker tuning; the dispatcher closes the
                // commit-to-POST loop (ADR-011 §2 + §13.13).
                let mut delivery_cfg = crate::push::delivery::PushDeliveryConfig::default();
                delivery_cfg.max_attempts = self.runtime_config.push_max_attempts as u32;
                delivery_cfg.backoff_base = self.runtime_config.push_backoff_base;
                delivery_cfg.backoff_cap = self.runtime_config.push_backoff_cap;
                delivery_cfg.backoff_jitter = self.runtime_config.push_backoff_jitter;
                delivery_cfg.request_timeout = self.runtime_config.push_request_timeout;
                delivery_cfg.connect_timeout = self.runtime_config.push_connect_timeout;
                delivery_cfg.read_timeout = self.runtime_config.push_read_timeout;
                delivery_cfg.claim_expiry = self.runtime_config.push_claim_expiry;
                delivery_cfg.max_payload_bytes = self.runtime_config.push_max_payload_bytes;
                delivery_cfg.allow_insecure_urls = self.runtime_config.allow_insecure_push_urls;

                let instance_id = format!("a2a-server-{}", uuid::Uuid::now_v7());
                let worker = crate::push::delivery::PushDeliveryWorker::new(
                    push_delivery.clone(),
                    delivery_cfg,
                    None,
                    instance_id,
                )
                .map_err(|e| A2aError::Internal(format!("push worker build failed: {e}")))?;

                Some(Arc::new(crate::push::PushDispatcher::new(
                    Arc::new(worker),
                    push_storage.clone(),
                    task_storage.clone(),
                )))
            } else {
                None
            };

        // Collect and merge security contributions
        let contributions: Vec<SecurityContribution> = self
            .middleware
            .iter()
            .map(|m| m.security_contribution())
            .collect();
        let merged = merge_stacked_contributions(&contributions)?;

        Ok(A2aServer {
            state: AppState {
                executor,
                task_storage,
                push_storage,
                event_store,
                atomic_store,
                event_broker: TaskEventBroker::new(),
                middleware_stack: Arc::new(MiddlewareStack::new(self.middleware)),
                runtime_config: self.runtime_config,
                in_flight: Arc::new(crate::server::in_flight::InFlightRegistry::new()),
                cancellation_supervisor,
                push_delivery_store,
                push_dispatcher,
            },
            merged_security: merged,
            bind_addr: self.bind_addr,
        })
    }
}

impl Default for A2aServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Merge stacked contributions (AND semantics).
///
/// Schemes: union with collision detection (identical = dedup, different = error).
/// Requirements: Cartesian product (AND).
fn merge_stacked_contributions(
    contributions: &[SecurityContribution],
) -> Result<SecurityContribution, A2aError> {
    let mut merged = SecurityContribution::new();

    if contributions.is_empty() {
        return Ok(merged);
    }

    // 1. Collect schemes with collision detection
    let mut seen_schemes: std::collections::HashMap<String, turul_a2a_proto::SecurityScheme> =
        std::collections::HashMap::new();

    for contrib in contributions {
        for (name, scheme) in &contrib.schemes {
            if let Some(existing) = seen_schemes.get(name) {
                // Check semantic equality
                if !schemes_equivalent(existing, scheme) {
                    return Err(A2aError::Internal(format!(
                        "Security scheme collision: '{}' has conflicting definitions",
                        name
                    )));
                }
                // Identical — skip (dedup)
            } else {
                seen_schemes.insert(name.clone(), scheme.clone());
                merged.schemes.push((name.clone(), scheme.clone()));
            }
        }
    }

    // 2. Compute requirements via Cartesian product (AND)
    let requirement_sets: Vec<&[turul_a2a_proto::SecurityRequirement]> = contributions
        .iter()
        .filter(|c| !c.requirements.is_empty())
        .map(|c| c.requirements.as_slice())
        .collect();

    if requirement_sets.is_empty() {
        return Ok(merged);
    }

    let mut combined: Vec<turul_a2a_proto::SecurityRequirement> =
        requirement_sets[0].to_vec();

    for alternatives in &requirement_sets[1..] {
        let mut new_combined = Vec::new();
        for existing in &combined {
            for alt in *alternatives {
                let mut merged_schemes = existing.schemes.clone();
                for (name, scopes) in &alt.schemes {
                    merged_schemes
                        .entry(name.clone())
                        .and_modify(|existing_scopes| {
                            // Union scopes, dedup + sort
                            for s in &scopes.list {
                                if !existing_scopes.list.contains(s) {
                                    existing_scopes.list.push(s.clone());
                                }
                            }
                            existing_scopes.list.sort();
                            existing_scopes.list.dedup();
                        })
                        .or_insert_with(|| scopes.clone());
                }
                new_combined.push(turul_a2a_proto::SecurityRequirement {
                    schemes: merged_schemes,
                });
            }
        }
        combined = new_combined;
    }

    merged.requirements = combined;
    Ok(merged)
}

/// Semantic equality for SecurityScheme.
///
/// Uses structural PartialEq on proto types. This is correct for:
/// - ApiKeySecurityScheme (no maps)
/// - HttpAuthSecurityScheme (no maps)
/// - MutualTlsSecurityScheme (no maps)
///
/// Limitation: For OAuth2SecurityScheme and OpenIdConnectSecurityScheme,
/// scope maps (HashMap) have non-deterministic iteration order. PartialEq
/// on HashMap compares by content (not order), so this is correct for
/// HashMap but would need explicit normalization if proto ever uses
/// BTreeMap or sorted structures. Sufficient for the scheme types
/// this workspace supports (API Key + HTTP Bearer); revisit if new
/// scheme types introduce ordering-sensitive fields.
fn schemes_equivalent(
    a: &turul_a2a_proto::SecurityScheme,
    b: &turul_a2a_proto::SecurityScheme,
) -> bool {
    a == b
}

/// A configured A2A server ready to run.
pub struct A2aServer {
    state: AppState,
    merged_security: SecurityContribution,
    bind_addr: SocketAddr,
}

impl A2aServer {
    pub fn builder() -> A2aServerBuilder {
        A2aServerBuilder::new()
    }

    /// Build the axum router — useful for testing.
    /// Augments the AgentCard with merged security contributions.
    pub fn into_router(self) -> axum::Router {
        let (state, had_security) = self.into_augmented_state();
        if !had_security {
            return build_router(state);
        }
        build_router(state)
    }

    /// Produce the `AppState` that `into_router` would mount, plus a flag
    /// indicating whether security augmentation was applied. Exposed to
    /// tests so they can assert post-augmentation invariants (e.g., that
    /// `push_delivery_store` survives the auth rebuild) without going
    /// through an HTTP round-trip.
    pub(crate) fn into_augmented_state(self) -> (AppState, bool) {
        if self.merged_security.is_empty() {
            return (self.state, false);
        }

        let wrapped = SecurityAugmentedExecutor {
            inner: self.state.executor.clone(),
            security: self.merged_security,
        };

        let augmented = AppState {
            executor: Arc::new(wrapped),
            task_storage: self.state.task_storage,
            push_storage: self.state.push_storage,
            event_store: self.state.event_store,
            atomic_store: self.state.atomic_store,
            event_broker: self.state.event_broker,
            middleware_stack: self.state.middleware_stack,
            runtime_config: self.state.runtime_config,
            in_flight: self.state.in_flight,
            cancellation_supervisor: self.state.cancellation_supervisor,
            // Preserve both the push claim store and the dispatcher
            // through security augmentation. Dropping either would
            // silently disable push delivery for every auth-gated
            // deployment.
            push_delivery_store: self.state.push_delivery_store,
            push_dispatcher: self.state.push_dispatcher,
        };

        (augmented, true)
    }

    /// Run the server.
    pub async fn run(self) -> Result<(), A2aError> {
        let bind_addr = self.bind_addr;
        // Capture substrate refs for the cross-instance cancel poller
        // before `into_router` consumes `self`. The poller uses the same
        // Arcs for `in_flight` and `cancellation_supervisor` as the
        // router's AppState, so a marker write from any instance is
        // observed here within one `cross_instance_cancel_poll_interval`.
        let poller_registry = std::sync::Arc::clone(&self.state.in_flight);
        let poller_supervisor = std::sync::Arc::clone(&self.state.cancellation_supervisor);
        let poller_interval = self.state.runtime_config.cross_instance_cancel_poll_interval;
        let push_delivery_store_for_sweep = self.state.push_delivery_store.clone();
        let self_push_dispatcher_for_sweep = self.state.push_dispatcher.clone();
        let sweep_interval_for_task =
            self.state.runtime_config.push_reclaim_sweep_interval;
        let sweep_batch_for_task =
            self.state.runtime_config.push_reclaim_sweep_batch;
        let push_claim_expiry_for_sweep =
            self.state.runtime_config.push_claim_expiry;

        let app = self.into_router();
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .map_err(|e| A2aError::Internal(format!("Failed to bind: {e}")))?;
        tracing::info!("A2A server listening on {}", bind_addr);

        // Spawn the supervisor poll loop. A shutdown token lets us stop
        // the poller when the server exits; for now axum::serve runs to
        // completion so the token is never tripped (server shutdown is
        // driven by process exit). Later: wire the token to a graceful
        // shutdown signal.
        let shutdown = tokio_util::sync::CancellationToken::new();
        let poller_shutdown = shutdown.clone();
        let poller_handle = tokio::spawn(crate::server::in_flight::run_cross_instance_cancel_poller(
            poller_registry,
            poller_supervisor,
            poller_interval,
            poller_shutdown,
        ));

        // Push-delivery reclaim loop (ADR-011 §10.5). Two
        // enumerations run per tick:
        //
        // 1. `list_stale_pending_dispatches` — events whose initial
        //    fan-out died (e.g. persistent config-store outage)
        //    before any claim rows were created. The dispatcher
        //    reloads the task and re-runs the fan-out; each
        //    per-config delivery then creates or refreshes its
        //    claim row.
        //
        // 2. `list_reclaimable_claims` — claim rows that reached a
        //    non-terminal expired state (worker exhausted its
        //    bounded persist retry). The dispatcher re-invokes
        //    `deliver()` which re-claims and re-POSTs.
        //
        // Staleness threshold for pending dispatches: claim_expiry,
        // the same budget the builder already validates must exceed
        // the retry horizon. In-progress dispatches stay under this
        // threshold; only genuinely stuck markers get picked up.
        let sweep_handle = match (
            push_delivery_store_for_sweep,
            self_push_dispatcher_for_sweep,
        ) {
            (Some(store), Some(dispatcher)) => {
                let shutdown = shutdown.clone();
                let interval = sweep_interval_for_task;
                let batch = sweep_batch_for_task;
                let pending_stale_threshold = push_claim_expiry_for_sweep;
                Some(tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(interval);
                    ticker.set_missed_tick_behavior(
                        tokio::time::MissedTickBehavior::Delay,
                    );
                    loop {
                        tokio::select! {
                            _ = shutdown.cancelled() => break,
                            _ = ticker.tick() => {
                                let cutoff = std::time::SystemTime::now()
                                    .checked_sub(pending_stale_threshold)
                                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                                match store
                                    .list_stale_pending_dispatches(cutoff, batch)
                                    .await
                                {
                                    Ok(rows) if !rows.is_empty() => {
                                        tracing::warn!(
                                            target: "turul_a2a::push_pending_dispatches_stale",
                                            count = rows.len(),
                                            "reclaim sweep found stale pending-dispatch \
                                             markers; re-running fan-out"
                                        );
                                        for row in rows {
                                            dispatcher.redispatch_pending(row).await;
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        tracing::error!(
                                            target: "turul_a2a::push_pending_sweep_error",
                                            error = %e,
                                            "pending-dispatch sweep failed"
                                        );
                                    }
                                }
                                match store.list_reclaimable_claims(batch).await {
                                    Ok(rows) if !rows.is_empty() => {
                                        tracing::warn!(
                                            target: "turul_a2a::push_claims_reclaimed",
                                            count = rows.len(),
                                            "reclaim sweep found expired non-terminal \
                                             push claims; redispatching"
                                        );
                                        for row in rows {
                                            dispatcher.redispatch_one(row).await;
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        tracing::error!(
                                            target: "turul_a2a::push_sweep_error",
                                            error = %e,
                                            "push claim sweep failed"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }))
            }
            _ => None,
        };

        let serve_result = axum::serve(listener, app).await;

        // Gracefully stop background loops — relevant if axum::serve ever returns.
        shutdown.cancel();
        let _ = poller_handle.await;
        if let Some(h) = sweep_handle {
            let _ = h.await;
        }

        serve_result
            .map_err(|e| A2aError::Internal(format!("Server error: {e}")))?;
        Ok(())
    }
}

/// Wraps an executor to augment its agent card with merged security contributions.
struct SecurityAugmentedExecutor {
    inner: Arc<dyn AgentExecutor>,
    security: SecurityContribution,
}

#[async_trait::async_trait]
impl AgentExecutor for SecurityAugmentedExecutor {
    async fn execute(
        &self,
        task: &mut turul_a2a_types::Task,
        msg: &turul_a2a_types::Message,
        ctx: &crate::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
        self.inner.execute(task, msg, ctx).await
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        let mut card = self.inner.agent_card();
        // Merge security contributions into the card
        for (name, scheme) in &self.security.schemes {
            card.security_schemes
                .entry(name.clone())
                .or_insert_with(|| scheme.clone());
        }
        for req in &self.security.requirements {
            card.security_requirements.push(req.clone());
        }
        card
    }

    fn extended_agent_card(
        &self,
        claims: Option<&serde_json::Value>,
    ) -> Option<turul_a2a_proto::AgentCard> {
        self.inner.extended_agent_card(claims).map(|mut card| {
            for (name, scheme) in &self.security.schemes {
                card.security_schemes
                    .entry(name.clone())
                    .or_insert_with(|| scheme.clone());
            }
            for req in &self.security.requirements {
                card.security_requirements.push(req.clone());
            }
            card
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::A2aError;
    use crate::executor::AgentExecutor;
    use turul_a2a_types::{Message, Task};

    struct DummyExecutor;

    #[async_trait::async_trait]
    impl AgentExecutor for DummyExecutor {
        async fn execute(&self, _task: &mut Task, _msg: &Message, _ctx: &crate::executor::ExecutionContext) -> Result<(), A2aError> {
            Ok(())
        }
        fn agent_card(&self) -> turul_a2a_proto::AgentCard {
            turul_a2a_proto::AgentCard::default()
        }
    }

    #[test]
    fn builder_requires_executor() {
        let result = A2aServer::builder().build();
        assert!(result.is_err());
    }

    #[test]
    fn builder_with_executor_defaults_storage() {
        let server = A2aServer::builder()
            .executor(DummyExecutor)
            .build()
            .unwrap();
        let _ = server.into_router();
    }

    #[test]
    fn builder_with_explicit_storage() {
        let storage = InMemoryA2aStorage::new();
        let server = A2aServer::builder()
            .executor(DummyExecutor)
            .storage(storage)
            .bind(([127, 0, 0, 1], 8080))
            .build()
            .unwrap();
        let _ = server.into_router();
    }

    /// Fake event store with a different backend_name for mismatch testing.
    struct FakeEventStore;

    #[async_trait::async_trait]
    impl crate::storage::A2aEventStore for FakeEventStore {
        fn backend_name(&self) -> &'static str { "fake-backend" }
        async fn append_event(&self, _t: &str, _tid: &str, _e: crate::streaming::StreamEvent) -> Result<u64, crate::storage::A2aStorageError> { Ok(0) }
        async fn get_events_after(&self, _t: &str, _tid: &str, _s: u64) -> Result<Vec<(u64, crate::streaming::StreamEvent)>, crate::storage::A2aStorageError> { Ok(vec![]) }
        async fn latest_sequence(&self, _t: &str, _tid: &str) -> Result<u64, crate::storage::A2aStorageError> { Ok(0) }
        async fn cleanup_expired(&self) -> Result<u64, crate::storage::A2aStorageError> { Ok(0) }
    }

    #[test]
    fn mixed_backend_rejected_at_build() {
        // Task + push = in-memory, event = fake-backend → should fail
        let result = A2aServer::builder()
            .executor(DummyExecutor)
            .event_store(FakeEventStore)
            .build();

        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("backend mismatch") || msg.contains("Storage backend mismatch"),
                    "Error should mention backend mismatch: {msg}"
                );
            }
            Ok(_) => panic!("Mixed backends should be rejected"),
        }
    }

    /// Fake atomic store with a different backend_name for mismatch testing.
    struct FakeAtomicStore;

    #[async_trait::async_trait]
    impl crate::storage::A2aAtomicStore for FakeAtomicStore {
        fn backend_name(&self) -> &'static str { "fake-atomic" }
        async fn create_task_with_events(&self, _t: &str, _o: &str, task: turul_a2a_types::Task, _e: Vec<crate::streaming::StreamEvent>) -> Result<(turul_a2a_types::Task, Vec<u64>), crate::storage::A2aStorageError> { Ok((task, vec![])) }
        async fn update_task_status_with_events(&self, _t: &str, _tid: &str, _o: &str, _s: turul_a2a_types::TaskStatus, _e: Vec<crate::streaming::StreamEvent>) -> Result<(turul_a2a_types::Task, Vec<u64>), crate::storage::A2aStorageError> { unimplemented!() }
        async fn update_task_with_events(&self, _t: &str, _o: &str, _task: turul_a2a_types::Task, _e: Vec<crate::streaming::StreamEvent>) -> Result<Vec<u64>, crate::storage::A2aStorageError> { Ok(vec![]) }
    }

    #[test]
    fn mixed_atomic_backend_rejected_at_build() {
        // Task + push + event = in-memory, atomic = fake-atomic → should fail
        let result = A2aServer::builder()
            .executor(DummyExecutor)
            .atomic_store(FakeAtomicStore)
            .build();

        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("backend mismatch") || msg.contains("Storage backend mismatch"),
                    "Error should mention backend mismatch: {msg}"
                );
            }
            Ok(_) => panic!("Mixed atomic backend should be rejected"),
        }
    }

    #[test]
    fn same_backend_accepted() {
        // All four from the same InMemoryA2aStorage — should succeed
        let storage = InMemoryA2aStorage::new();
        let result = A2aServer::builder()
            .executor(DummyExecutor)
            .task_storage(storage.clone())
            .push_storage(storage.clone())
            .event_store(storage.clone())
            .atomic_store(storage)
            .build();

        assert!(result.is_ok(), "Same backend should be accepted");
    }

    #[test]
    fn unified_storage_accepted() {
        // Single .storage() call — the preferred path
        let result = A2aServer::builder()
            .executor(DummyExecutor)
            .storage(InMemoryA2aStorage::new())
            .build();

        assert!(result.is_ok(), "Unified .storage() should be accepted");
    }

    #[test]
    fn runtime_config_setters_survive_build() {
        // Invariant: builder setters for runtime-config knobs must propagate
        // into the built server's AppState so that handlers can read
        // them via `state.runtime_config`. This test exists to prevent the
        // regression where setters silently become no-ops.
        let server = A2aServer::builder()
            .executor(DummyExecutor)
            // One representative setter per consumer group.
            .blocking_task_timeout(Duration::from_secs(42))          
            .timeout_abort_grace(Duration::from_secs(7))              
            .cancel_handler_grace(Duration::from_secs(3))             
            .cancel_handler_poll_interval(Duration::from_millis(50))  
            .cross_instance_cancel_poll_interval(Duration::from_secs(2))
            .push_max_attempts(17)                                    
            .push_backoff_base(Duration::from_millis(500))            
            .push_backoff_cap(Duration::from_secs(90))                
            .push_backoff_jitter(0.5)                                 
            .push_request_timeout(Duration::from_secs(20))            
            .push_connect_timeout(Duration::from_secs(3))             
            .push_read_timeout(Duration::from_secs(15))               
            .push_claim_expiry(Duration::from_secs(20 * 60))          
            .push_config_cache_ttl(Duration::from_secs(10))           
            .push_failed_delivery_retention(Duration::from_secs(48 * 60 * 60))
            .push_max_payload_bytes(2 * 1024 * 1024)                  
            .allow_insecure_push_urls(true)                           
            .build()
            .expect("build must succeed");

        let cfg = &server.state.runtime_config;
        assert_eq!(cfg.blocking_task_timeout, Duration::from_secs(42));
        assert_eq!(cfg.timeout_abort_grace, Duration::from_secs(7));
        assert_eq!(cfg.cancel_handler_grace, Duration::from_secs(3));
        assert_eq!(cfg.cancel_handler_poll_interval, Duration::from_millis(50));
        assert_eq!(cfg.cross_instance_cancel_poll_interval, Duration::from_secs(2));
        assert_eq!(cfg.push_max_attempts, 17);
        assert_eq!(cfg.push_backoff_base, Duration::from_millis(500));
        assert_eq!(cfg.push_backoff_cap, Duration::from_secs(90));
        assert!((cfg.push_backoff_jitter - 0.5).abs() < f32::EPSILON);
        assert_eq!(cfg.push_request_timeout, Duration::from_secs(20));
        assert_eq!(cfg.push_connect_timeout, Duration::from_secs(3));
        assert_eq!(cfg.push_read_timeout, Duration::from_secs(15));
        assert_eq!(cfg.push_claim_expiry, Duration::from_secs(20 * 60));
        assert_eq!(cfg.push_config_cache_ttl, Duration::from_secs(10));
        assert_eq!(cfg.push_failed_delivery_retention, Duration::from_secs(48 * 60 * 60));
        assert_eq!(cfg.push_max_payload_bytes, 2 * 1024 * 1024);
        assert!(cfg.allow_insecure_push_urls);
    }

    #[test]
    fn runtime_config_defaults_reach_built_server() {
        // Second half of the contract: when no setters are called, the
        // documented defaults land in AppState.
        let server = A2aServer::builder()
            .executor(DummyExecutor)
            .build()
            .expect("build must succeed");

        let cfg = &server.state.runtime_config;
        let defaults = RuntimeConfig::default();
        assert_eq!(cfg.blocking_task_timeout, defaults.blocking_task_timeout);
        assert_eq!(cfg.push_max_attempts, defaults.push_max_attempts);
        assert_eq!(cfg.allow_insecure_push_urls, defaults.allow_insecure_push_urls);
    }

    // -----------------------------------------------------------------
    // Push delivery store wiring (ADR-011 §10)
    // -----------------------------------------------------------------

    #[test]
    fn unified_storage_wires_push_delivery_store() {
        // `.storage()` must populate AppState.push_delivery_store so that
        // a PushDeliveryWorker spawned from the built state has
        // somewhere to claim against.
        let server = A2aServer::builder()
            .executor(DummyExecutor)
            .storage(InMemoryA2aStorage::new())
            .build()
            .expect("build must succeed");
        assert!(
            server.state.push_delivery_store.is_some(),
            "storage() must wire push_delivery_store"
        );
    }

    #[test]
    fn default_storage_leaves_push_delivery_store_unset() {
        // When no storage is passed at all, push delivery is opt-in —
        // push-config CRUD must still work but no worker gets spawned.
        let server = A2aServer::builder()
            .executor(DummyExecutor)
            .build()
            .expect("build must succeed");
        assert!(
            server.state.push_delivery_store.is_none(),
            "default build must leave push_delivery_store unset"
        );
    }

    #[test]
    fn explicit_push_delivery_store_setter() {
        // The individual setter exists for mixed-backend tests. It should
        // also satisfy the same-backend check when paired with matching
        // storage.
        let storage = InMemoryA2aStorage::new();
        let server = A2aServer::builder()
            .executor(DummyExecutor)
            .storage(storage.clone())
            .push_delivery_store(storage)
            .build()
            .expect("build must succeed");
        assert!(server.state.push_delivery_store.is_some());
    }

    #[test]
    fn retry_horizon_violation_rejected() {
        // push_claim_expiry <= max_attempts * backoff_cap must fail fast
        // (ADR-011 §10.3). The in-memory delivery store is required to
        // trigger the check.
        let res = A2aServer::builder()
            .executor(DummyExecutor)
            .storage(InMemoryA2aStorage::new())
            .push_max_attempts(10)
            .push_backoff_cap(Duration::from_secs(60))
            // 10 * 60s = 600s — equal to claim expiry, which is <=, so rejected.
            .push_claim_expiry(Duration::from_secs(600))
            .build();
        let err = match res {
            Err(e) => e,
            Ok(_) => panic!("retry horizon violation must be rejected"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("retry horizon") || msg.contains("push_claim_expiry"),
            "error should mention retry horizon: {msg}"
        );
    }

    // Test middleware that contributes a non-empty SecurityContribution
    // so `into_augmented_state` must rebuild the state. Accepts every
    // request — this test is about wiring, not auth behaviour.
    struct ContribMiddleware;

    #[async_trait::async_trait]
    impl A2aMiddleware for ContribMiddleware {
        async fn before_request(
            &self,
            _ctx: &mut crate::middleware::RequestContext,
        ) -> Result<(), crate::middleware::MiddlewareError> {
            Ok(())
        }
        fn security_contribution(&self) -> SecurityContribution {
            SecurityContribution::new().with_scheme(
                "TestApiKey",
                turul_a2a_proto::SecurityScheme {
                    scheme: Some(turul_a2a_proto::security_scheme::Scheme::ApiKeySecurityScheme(
                        turul_a2a_proto::ApiKeySecurityScheme {
                            description: "test".into(),
                            location: "header".into(),
                            name: "X-Test-Key".into(),
                        },
                    )),
                },
                vec![],
            )
        }
    }

    #[test]
    fn push_delivery_store_survives_security_augmentation() {
        // Regression: `into_router`'s rebuilt AppState used to hard-code
        // push_delivery_store: None, silently disabling push delivery on
        // every auth-gated deployment. The augmented state must carry the
        // same claim-store Arc the builder installed.
        let server = A2aServer::builder()
            .executor(DummyExecutor)
            .storage(InMemoryA2aStorage::new())
            .middleware(Arc::new(ContribMiddleware))
            .build()
            .expect("build must succeed");
        // Sanity: push_delivery_store is wired pre-augmentation.
        assert!(server.state.push_delivery_store.is_some());

        let (augmented, had_security) = server.into_augmented_state();
        assert!(
            had_security,
            "ContribMiddleware contributed a scheme — augmentation must run"
        );
        assert!(
            augmented.push_delivery_store.is_some(),
            "push_delivery_store must survive security augmentation"
        );
    }

    #[test]
    fn push_delivery_store_passthrough_without_security() {
        // With no contributing middleware, `into_augmented_state` returns
        // the original state unchanged — the store must still be present.
        let server = A2aServer::builder()
            .executor(DummyExecutor)
            .storage(InMemoryA2aStorage::new())
            .build()
            .expect("build must succeed");
        let (state, had_security) = server.into_augmented_state();
        assert!(!had_security);
        assert!(state.push_delivery_store.is_some());
    }

    #[test]
    fn retry_horizon_satisfied_accepted() {
        // push_claim_expiry > max_attempts * backoff_cap succeeds.
        let server = A2aServer::builder()
            .executor(DummyExecutor)
            .storage(InMemoryA2aStorage::new())
            .push_max_attempts(5)
            .push_backoff_cap(Duration::from_secs(60))
            .push_claim_expiry(Duration::from_secs(5 * 60 + 1))
            .build()
            .expect("horizon-satisfying config must build");
        assert!(server.state.push_delivery_store.is_some());
    }
}
