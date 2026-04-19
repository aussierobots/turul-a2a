//! AWS Lambda adapter for turul-a2a.
//!
//! Thin wrapper: converts Lambda events to axum requests, delegates to the
//! same Router, converts responses back. Per ADR-008/ADR-009:
//! - Authorizer context mapped via synthetic headers with anti-spoofing
//! - Streaming supported via durable event store (D3)
//! - SSE responses are buffered: task executes, events are collected, returned as one response
//! - `POST /message:stream` executes the task within the Lambda invocation and returns all events
//! - `GET /tasks/{id}:subscribe` is for tasks that are **not** in a terminal
//!   state. Within one invocation it emits the initial `Task` snapshot,
//!   replays stored events via `Last-Event-ID`, and closes when the task
//!   reaches a terminal state. Subscribing to an already-terminal task
//!   returns `UnsupportedOperationError` per A2A v1.0 §3.1.6 / ADR-010
//!   §4.3. For retrieving a terminal task's final state use `GetTask`.
//!
//! Lambda streaming is request-scoped (not persistent SSE connections). The durable
//! event store ensures events survive across invocations. Clients reconnect with
//! `Last-Event-ID` for continuation.
//!
//! # Cross-instance cancellation
//!
//! Lambda invocations are stateless and short-lived, so the Lambda
//! adapter does **not** run the persistent cross-instance cancel
//! poller that `A2aServer::run()` spawns. Cancellation behaviour on
//! the Lambda adapter:
//!
//! - **Marker writes** — `CancelTask` on a Lambda invocation writes
//!   the cancel marker to the shared backend (DynamoDB / PostgreSQL).
//!   This works.
//! - **Propagation to a live executor on the SAME Lambda invocation** —
//!   works via the same-instance token-trip path in `core_cancel_task`.
//! - **Propagation to a live executor on a DIFFERENT Lambda invocation
//!   (warm container)** — **not currently supported**. There is no
//!   persistent poller to observe markers written by other
//!   invocations. A subsequent invocation whose handler reads the
//!   marker directly may act on it, but that is not a substitute for
//!   the server runtime's live propagation.
//!
//! The builder still **requires** an `A2aCancellationSupervisor`
//! implementation on the same backend so that marker writes reach
//! the correct backend and a future polling-adapter variant can
//! consume them. If a deployment passes a non-matching supervisor,
//! `build()` rejects the configuration.

mod adapter;
mod auth;
mod no_streaming;
mod stream_recovery;

#[cfg(test)]
mod builder_tests;
#[cfg(test)]
mod stream_recovery_tests;

pub use adapter::{lambda_to_axum_request, axum_to_lambda_response};
pub use auth::{AuthorizerMapping, LambdaAuthorizerMiddleware};
pub use no_streaming::NoStreamingLayer;
pub use stream_recovery::LambdaStreamRecoveryHandler;

use std::sync::Arc;

use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::middleware::{A2aMiddleware, MiddlewareStack};
use turul_a2a::push::A2aPushDeliveryStore;
use turul_a2a::router::{build_router, AppState};
use turul_a2a::server::RuntimeConfig;
use turul_a2a::storage::{
    A2aAtomicStore, A2aCancellationSupervisor, A2aEventStore, A2aPushNotificationStorage,
    A2aTaskStorage,
};
use turul_a2a::streaming::TaskEventBroker;

/// Builder for Lambda A2A handler.
///
/// Use `.storage(my_storage)` to supply a single backend implementing all storage traits.
/// This satisfies ADR-009's same-backend requirement and enables streaming via durable
/// event store.
///
/// For push-notification delivery, wire `.storage()` with a backend that implements
/// [`A2aPushDeliveryStore`] (all four first-party backends do). The atomic store
/// must also opt in via `with_push_dispatch_enabled(true)` (ADR-013 §4.3); the
/// builder rejects configurations where the flag and the delivery store disagree.
pub struct LambdaA2aServerBuilder {
    executor: Option<Arc<dyn AgentExecutor>>,
    task_storage: Option<Arc<dyn A2aTaskStorage>>,
    push_storage: Option<Arc<dyn A2aPushNotificationStorage>>,
    event_store: Option<Arc<dyn A2aEventStore>>,
    atomic_store: Option<Arc<dyn A2aAtomicStore>>,
    cancellation_supervisor: Option<Arc<dyn A2aCancellationSupervisor>>,
    push_delivery_store: Option<Arc<dyn A2aPushDeliveryStore>>,
    middleware: Vec<Arc<dyn A2aMiddleware>>,
    runtime_config: RuntimeConfig,
}

impl LambdaA2aServerBuilder {
    pub fn new() -> Self {
        Self {
            executor: None,
            task_storage: None,
            push_storage: None,
            event_store: None,
            atomic_store: None,
            cancellation_supervisor: None,
            push_delivery_store: None,
            middleware: vec![],
            runtime_config: RuntimeConfig::default(),
        }
    }

    pub fn executor(mut self, exec: impl AgentExecutor + 'static) -> Self {
        self.executor = Some(Arc::new(exec));
        self
    }

    /// Set all storage from a single backend instance.
    ///
    /// This is the **preferred** method — a single struct implementing all
    /// storage traits guarantees the same-backend requirement (ADR-009)
    /// AND ensures the cancellation supervisor reads the same backend
    /// the cancel marker is written to. A mismatch here (e.g., DynamoDB
    /// task storage + in-memory cancellation supervisor) silently
    /// breaks cross-instance cancellation.
    ///
    /// The bound also requires [`A2aPushDeliveryStore`] so push-delivery
    /// infrastructure is wired by the same call. Backends that implement
    /// push delivery MUST also opt in via `with_push_dispatch_enabled(true)`
    /// on the storage instance; the builder rejects the inconsistent cases
    /// (ADR-013 §4.3).
    pub fn storage<S>(mut self, storage: S) -> Self
    where
        S: A2aTaskStorage
            + A2aPushNotificationStorage
            + A2aEventStore
            + A2aAtomicStore
            + A2aCancellationSupervisor
            + A2aPushDeliveryStore
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

    /// Set task storage individually. Must be paired with event_store/atomic_store
    /// AND `cancellation_supervisor()` for same-backend compliance.
    pub fn task_storage(mut self, s: impl A2aTaskStorage + 'static) -> Self {
        self.task_storage = Some(Arc::new(s));
        self
    }

    /// Set push notification storage individually.
    pub fn push_storage(mut self, s: impl A2aPushNotificationStorage + 'static) -> Self {
        self.push_storage = Some(Arc::new(s));
        self
    }

    /// Set event store individually. Must match task_storage backend.
    pub fn event_store(mut self, s: impl A2aEventStore + 'static) -> Self {
        self.event_store = Some(Arc::new(s));
        self
    }

    /// Set atomic store individually. Must match task_storage backend.
    pub fn atomic_store(mut self, s: impl A2aAtomicStore + 'static) -> Self {
        self.atomic_store = Some(Arc::new(s));
        self
    }

    /// Set the cancellation supervisor individually. Must match
    /// task_storage backend — otherwise `:cancel` writes the marker to
    /// one backend and the supervisor reads from another, silently
    /// breaking cross-instance cancellation. Prefer `.storage()` for
    /// unified wiring. Consumed by `core_cancel_task` (ADR-012).
    pub fn cancellation_supervisor(
        mut self,
        s: impl A2aCancellationSupervisor + 'static,
    ) -> Self {
        self.cancellation_supervisor = Some(Arc::new(s));
        self
    }

    /// Set the push delivery store individually (ADR-011 §10 / ADR-013).
    ///
    /// Prefer `.storage()` for unified wiring. The delivery store MUST
    /// be on the same backend as `.task_storage(...)` — the builder
    /// rejects mismatches. Passing a delivery store also requires the
    /// atomic store to have opted in via `with_push_dispatch_enabled(true)`
    /// (ADR-013 §4.3).
    pub fn push_delivery_store(
        mut self,
        store: impl A2aPushDeliveryStore + 'static,
    ) -> Self {
        self.push_delivery_store = Some(Arc::new(store));
        self
    }

    /// Override the runtime configuration (timeouts, retry budgets,
    /// push tuning). Defaults to [`RuntimeConfig::default()`]. The
    /// builder validates push settings (claim expiry vs retry horizon,
    /// ADR-011 §10.3) when `.push_delivery_store(...)` is wired.
    pub fn runtime_config(mut self, cfg: RuntimeConfig) -> Self {
        self.runtime_config = cfg;
        self
    }

    pub fn middleware(mut self, mw: Arc<dyn A2aMiddleware>) -> Self {
        self.middleware.push(mw);
        self
    }

    pub fn build(self) -> Result<LambdaA2aHandler, A2aError> {
        let executor = self.executor.ok_or(A2aError::Internal(
            "executor is required".into(),
        ))?;
        let task_storage = self.task_storage.ok_or(A2aError::Internal(
            "task_storage is required for Lambda".into(),
        ))?;
        let push_storage = self.push_storage.ok_or(A2aError::Internal(
            "push_storage is required for Lambda".into(),
        ))?;
        let event_store: Arc<dyn A2aEventStore> = self.event_store.ok_or(A2aError::Internal(
            "event_store is required for Lambda (use .storage() for unified backend)".into(),
        ))?;
        let atomic_store: Arc<dyn A2aAtomicStore> = self.atomic_store.ok_or(A2aError::Internal(
            "atomic_store is required for Lambda (use .storage() for unified backend)".into(),
        ))?;
        // ADR-012: the cancellation supervisor MUST be supplied. No
        // InMemoryA2aStorage fallback: that would silently break
        // cross-instance cancellation by reading markers from a
        // different backend than they were written to.
        let cancellation_supervisor: Arc<dyn A2aCancellationSupervisor> =
            self.cancellation_supervisor.ok_or(A2aError::Internal(
                "cancellation_supervisor is required for Lambda. Use .storage() for unified \
                 backend wiring, or .cancellation_supervisor(...) alongside the individual \
                 storage setters. Omitting it would silently break cross-instance \
                 cancellation in ADR-012."
                    .into(),
            ))?;

        // Same-backend enforcement (ADR-009 + ADR-012).
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
                 ADR-009 requires all storage traits to share the same backend. \
                 ADR-012 requires the cancellation supervisor on the same backend \
                 so cross-instance cancel markers are observable. \
                 Use .storage() for unified backend."
            )));
        }

        let push_delivery_store = self.push_delivery_store;

        // Push-dispatch consistency (ADR-013 §4.3): the atomic store's
        // opt-in flag and the presence of `push_delivery_store` MUST
        // agree. Both-true = push fully wired; both-false = non-push
        // deployment. Mixed cases are build errors — this mirrors the
        // main server builder (server/mod.rs).
        match (
            push_delivery_store.is_some(),
            atomic_store.push_dispatch_enabled(),
        ) {
            (true, true) | (false, false) => {}
            (true, false) => {
                return Err(A2aError::Internal(
                    "push_delivery_store wired but atomic_store.push_dispatch_enabled() \
                     is false. Call .with_push_dispatch_enabled(true) on the backend \
                     storage before passing it to .storage()."
                        .into(),
                ));
            }
            (false, true) => {
                return Err(A2aError::Internal(
                    "atomic_store.push_dispatch_enabled() is true but no \
                     push_delivery_store is wired. Pending-dispatch markers would be \
                     written with no consumer, imposing load-bearing infra for no \
                     benefit. If you need to populate markers for an external \
                     consumer, open an issue for a distinctly-named opt-in — for now, \
                     this configuration is rejected."
                        .into(),
                ));
            }
        }

        let runtime_config = self.runtime_config;

        // Push dispatcher wiring (ADR-011 §10 / ADR-013 §7). When
        // `push_delivery_store` is present, validate its backend matches,
        // validate the retry-horizon invariant, and construct the
        // `PushDispatcher`. Under Lambda the dispatcher runs
        // opportunistically post-return (ADR-013 §4.4); correctness
        // comes from the atomic marker + stream/scheduler recovery.
        let push_dispatcher: Option<Arc<turul_a2a::push::PushDispatcher>> =
            if let Some(delivery) = push_delivery_store.as_ref() {
                let push_delivery_backend = delivery.backend_name();
                if task_backend != push_delivery_backend {
                    return Err(A2aError::Internal(format!(
                        "Storage backend mismatch: task={task_backend}, \
                         push_delivery={push_delivery_backend}. \
                         ADR-009 requires all storage traits to share the same backend."
                    )));
                }

                // ADR-011 §10.3: claim expiry must exceed the worst-case
                // retry horizon so a live claim is not mis-classified as
                // stale mid-retry.
                let retry_horizon = runtime_config
                    .push_backoff_cap
                    .saturating_mul(runtime_config.push_max_attempts as u32);
                if runtime_config.push_claim_expiry <= retry_horizon {
                    return Err(A2aError::Internal(format!(
                        "push_claim_expiry ({:?}) must be greater than retry horizon \
                         (push_max_attempts={} * push_backoff_cap={:?} = {:?}). \
                         Raise push_claim_expiry or lower push_max_attempts/push_backoff_cap.",
                        runtime_config.push_claim_expiry,
                        runtime_config.push_max_attempts,
                        runtime_config.push_backoff_cap,
                        retry_horizon
                    )));
                }

                let mut delivery_cfg = turul_a2a::push::delivery::PushDeliveryConfig::default();
                delivery_cfg.max_attempts = runtime_config.push_max_attempts as u32;
                delivery_cfg.backoff_base = runtime_config.push_backoff_base;
                delivery_cfg.backoff_cap = runtime_config.push_backoff_cap;
                delivery_cfg.backoff_jitter = runtime_config.push_backoff_jitter;
                delivery_cfg.request_timeout = runtime_config.push_request_timeout;
                delivery_cfg.connect_timeout = runtime_config.push_connect_timeout;
                delivery_cfg.read_timeout = runtime_config.push_read_timeout;
                delivery_cfg.claim_expiry = runtime_config.push_claim_expiry;
                delivery_cfg.max_payload_bytes = runtime_config.push_max_payload_bytes;
                delivery_cfg.allow_insecure_urls = runtime_config.allow_insecure_push_urls;

                let instance_id = format!("a2a-lambda-{}", uuid::Uuid::now_v7());
                let worker = turul_a2a::push::delivery::PushDeliveryWorker::new(
                    delivery.clone(),
                    delivery_cfg,
                    None,
                    instance_id,
                )
                .map_err(|e| A2aError::Internal(format!("push worker build failed: {e}")))?;

                Some(Arc::new(turul_a2a::push::PushDispatcher::new(
                    Arc::new(worker),
                    push_storage.clone(),
                    task_storage.clone(),
                )))
            } else {
                None
            };

        let state = AppState {
            executor,
            task_storage,
            push_storage,
            event_store,
            atomic_store,
            event_broker: TaskEventBroker::new(),
            middleware_stack: Arc::new(MiddlewareStack::new(self.middleware)),
            runtime_config,
            in_flight: Arc::new(turul_a2a::server::in_flight::InFlightRegistry::new()),
            cancellation_supervisor,
            push_delivery_store,
            push_dispatcher,
        };

        let router = build_router(state);

        Ok(LambdaA2aHandler { router })
    }
}

impl Default for LambdaA2aServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Lambda handler wrapping the axum Router.
#[derive(Clone)]
pub struct LambdaA2aHandler {
    router: axum::Router,
}

impl LambdaA2aHandler {
    pub fn builder() -> LambdaA2aServerBuilder {
        LambdaA2aServerBuilder::new()
    }

    /// Handle a Lambda HTTP request.
    pub async fn handle(
        &self,
        event: lambda_http::Request,
    ) -> Result<lambda_http::Response<lambda_http::Body>, lambda_http::Error> {
        let axum_req = lambda_to_axum_request(event)?;

        let axum_resp = tower::ServiceExt::oneshot(self.router.clone(), axum_req)
            .await
            .map_err(|e| lambda_http::Error::from(format!("Router error: {e}")))?;

        axum_to_lambda_response(axum_resp).await
    }
}
