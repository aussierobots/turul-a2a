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
            + Clone
            + 'static,
    {
        self.task_storage = Some(Arc::new(storage.clone()));
        self.push_storage = Some(Arc::new(storage.clone()));
        self.event_store = Some(Arc::new(storage.clone()));
        self.atomic_store = Some(Arc::new(storage.clone()));
        self.cancellation_supervisor = Some(Arc::new(storage));
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
            .unwrap_or_else(|| Arc::new(default_storage));

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
/// BTreeMap or sorted structures. This is sufficient for v0.2 supported
/// scheme types (API Key + HTTP Bearer).
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
        let router = build_router(self.state.clone());

        // Store merged security in AppState for agent card augmentation
        // We use a different approach: wrap the agent card handler
        // Actually, the simplest: store merged security as an Extension on the router
        // But that's complex. Instead, patch the executor's agent card at build time.
        // Since AgentExecutor returns the card by value, we wrap it.

        if self.merged_security.is_empty() {
            return router;
        }

        // The agent card route is already built. We need to use the state's merged_security.
        // The cleanest approach: store merged_security in AppState and use it in the handler.
        // For now, rebuild with a wrapping executor.
        let wrapped = SecurityAugmentedExecutor {
            inner: self.state.executor.clone(),
            security: self.merged_security,
        };

        let augmented_state = AppState {
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
        };

        build_router(augmented_state)
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

        let serve_result = axum::serve(listener, app).await;

        // Gracefully stop the poller — relevant if axum::serve ever returns.
        shutdown.cancel();
        let _ = poller_handle.await;

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
}
