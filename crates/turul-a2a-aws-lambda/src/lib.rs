//! AWS Lambda adapter for turul-a2a.
//!
//! # Choosing a runner
//!
//! A Lambda binary receives raw event JSON; the adapter owns
//! classifying that JSON and dispatching to the right A2A handler
//! method. Adopters pick the runner whose name matches the Lambda's
//! AWS trigger topology and end `main.rs` with one `.await?` call —
//! `lambda_runtime` / `lambda_http` / event parsing never appear in
//! adopter code.
//!
//! | Lambda triggers                   | Runner                                         |
//! |-----------------------------------|------------------------------------------------|
//! | HTTP only (Function URL / APIGW)  | [`LambdaA2aHandler::run_http_only`]            |
//! | SQS only (event source mapping)   | [`LambdaA2aHandler::run_sqs_only`] *(`sqs`)*   |
//! | HTTP **and** SQS on one function  | [`LambdaA2aHandler::run_http_and_sqs`] *(`sqs`)* |
//! | not sure / just run it            | [`LambdaA2aHandler::run`]                      |
//!
//! [`LambdaA2aHandler::run`] is the default "it just works" entry
//! point: without the `sqs` feature it aliases `run_http_only`; with
//! `sqs` it aliases `run_http_and_sqs` (permissive, handles either
//! shape). The explicit runners are **strict** — a non-matching
//! event shape fails loudly, which is what hardened deployments
//! and tests want.
//!
//! The one-call shortcut is [`LambdaA2aServerBuilder::run`] — it
//! build-then-runs in a single fluent call. Use `.build()` + a
//! handler method when the handler is needed for unit tests or
//! custom middleware.
//!
//! ## Composing with non-framework triggers
//!
//! Lambdas wired to HTTP **plus** a third trigger the framework does
//! not know about (EventBridge scheduler, DynamoDB stream, custom
//! invoke) can't use a named runner directly — the Unknown branch
//! is adopter-owned. Reach for the public classifier + the HTTP
//! envelope primitive instead:
//!
//! ```ignore
//! lambda_runtime::run(lambda_runtime::service_fn(
//!     move |event: lambda_runtime::LambdaEvent<serde_json::Value>| {
//!         let handler = handler.clone();
//!         async move {
//!             let (value, _ctx) = event.into_parts();
//!             match turul_a2a_aws_lambda::classify_event(&value) {
//!                 turul_a2a_aws_lambda::LambdaEvent::Http => {
//!                     handler.handle_http_event_value(value).await
//!                 }
//!                 turul_a2a_aws_lambda::LambdaEvent::Sqs => {
//!                     let sqs = serde_json::from_value(value)?;
//!                     Ok(serde_json::to_value(handler.handle_sqs(sqs).await)?)
//!                 }
//!                 turul_a2a_aws_lambda::LambdaEvent::Unknown => {
//!                     // Adopter-owned: run your scheduled sweep,
//!                     // DynamoDB stream consumer, etc.
//!                     run_adopter_specific_path(value).await
//!                 }
//!             }
//!         }
//!     }
//! )).await
//! ```
//!
//! [`LambdaA2aHandler::handle_http_event_value`] is available without
//! the `sqs` feature; `handle_sqs` is gated on `sqs`. `classify_event`
//! + `LambdaEvent` are always available.
//!
//! ## API Gateway path prefix
//!
//! When Lambda sits behind a REST API Gateway with `AWS_PROXY`
//! integration, the event carries the full stage + resource tree in
//! the path (e.g. `/stage/agent/message:send`),
//! which the root-rooted A2A router would 404. Configure the prefix
//! to strip via [`LambdaA2aServerBuilder::strip_path_prefix`]:
//!
//! ```ignore
//! LambdaA2aServerBuilder::new()
//!     .executor(my_executor)
//!     .storage(my_storage)
//!     .strip_path_prefix("/stage/agent")  // API GW prefix
//!     .run()
//!     .await
//! ```
//!
//! The strip applies to `run_http_only` and `run_http_and_sqs`.
//! SQS events are unaffected. Non-matching paths pass through
//! unchanged (router still 404s on genuinely unknown paths —
//! that's the correct failure mode).
//!
//! ```ignore
//! // HTTP-only Lambda (request Lambda in two-Lambda topology):
//! LambdaA2aServerBuilder::new()
//!     .executor(my_executor)
//!     .storage(my_storage)
//!     .with_sqs_return_immediately(queue_url, sqs)  // enqueues durable jobs
//!     .build()?
//!     .run_http_only()
//!     .await
//!
//! // Single-function HTTP+SQS demo:
//! LambdaA2aServerBuilder::new()
//!     .executor(my_executor)
//!     .storage(my_storage)
//!     .with_sqs_return_immediately(queue_url, sqs)
//!     .run()            // builder shortcut; dispatches HTTP + SQS
//!     .await
//!
//! // Pure SQS worker — no with_sqs_return_immediately(...) needed
//! // (it consumes the queue, never enqueues):
//! LambdaA2aServerBuilder::new()
//!     .executor(my_executor)
//!     .storage(my_storage)
//!     .build()?
//!     .run_sqs_only()
//!     .await
//! ```
//!
//! # Adapter internals
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
//! # Push-notification delivery — external triggers are mandatory
//!
//! Push delivery on Lambda (ADR-013) is architecturally different
//! from the binary server. The request Lambda installed via
//! [`LambdaA2aServerBuilder`] still constructs a `PushDispatcher`
//! when `push_delivery_store` is wired, but any `tokio::spawn`
//! continuation it emits post-return is **opportunistic only** — the
//! Lambda execution environment may be frozen indefinitely between
//! invocations, so nothing can depend on that continuation completing
//! (ADR-013 §4.4).
//!
//! Correctness for push delivery on Lambda is carried by:
//!
//! 1. The atomic pending-dispatch marker written inside the request
//!    Lambda's commit transaction (ADR-013 §4.3 — opt in via
//!    `StorageImpl::with_push_dispatch_enabled(true)`).
//! 2. [`LambdaStreamRecoveryHandler`] — DynamoDB Streams trigger on
//!    `a2a_push_pending_dispatches`. DynamoDB backends only.
//! 3. [`LambdaScheduledRecoveryHandler`] — EventBridge Scheduler
//!    backstop. **Required for all backends**; it is the sole
//!    recovery path for SQLite / PostgreSQL / in-memory deployments.
//!
//! Without at least the scheduled worker, push delivery on Lambda is
//! not durable — a marker written on a cold invocation may never be
//! consumed. The example wiring lives in
//! `examples/lambda-stream-worker` and
//! `examples/lambda-scheduled-worker`.
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
mod event;
mod no_streaming;
mod scheduled_recovery;
mod stream_recovery;

#[cfg(feature = "sqs")]
mod durable;

#[cfg(test)]
mod builder_tests;
#[cfg(test)]
mod scheduled_recovery_tests;
#[cfg(test)]
mod stream_recovery_tests;

pub use adapter::{axum_to_lambda_response, lambda_to_axum_request};
pub use auth::{AuthorizerMapping, LambdaAuthorizerMiddleware};
pub use event::{LambdaEvent, classify_event};
pub use no_streaming::NoStreamingLayer;
pub use scheduled_recovery::{
    LambdaScheduledRecoveryConfig, LambdaScheduledRecoveryHandler, LambdaScheduledRecoveryResponse,
};
pub use stream_recovery::LambdaStreamRecoveryHandler;

#[cfg(feature = "sqs")]
pub use durable::{SqsDurableExecutorQueue, drive_sqs_batch};

use std::sync::Arc;

use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::middleware::{A2aMiddleware, MiddlewareStack};
use turul_a2a::push::A2aPushDeliveryStore;
use turul_a2a::router::{AppState, build_router};
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
    durable_executor_queue: Option<Arc<dyn turul_a2a::durable_executor::DurableExecutorQueue>>,
    path_prefix: Option<String>,
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
            durable_executor_queue: None,
            path_prefix: None,
        }
    }

    /// Configure an HTTP path prefix to strip before the A2A router
    /// sees the request. Needed when Lambda sits behind an API Gateway
    /// whose stage + resource tree is forwarded into the function
    /// (e.g. REST API `AWS_PROXY` integrations surface
    /// `/stage/agent/message:send` where the A2A
    /// router expects `/message:send`).
    ///
    /// The prefix must start with `/` and must not end with `/`
    /// (unless it is exactly `/`, which is a no-op). Segment boundaries
    /// are respected — a prefix of `/dev` will not strip `/devs/...`.
    /// Paths that do not start with the prefix pass through unchanged
    /// (the router will 404 on a genuinely unknown path, which is the
    /// correct failure mode). Applies to both
    /// [`LambdaA2aHandler::run_http_only`] and
    /// [`LambdaA2aHandler::run_http_and_sqs`]. SQS events are
    /// unaffected.
    pub fn strip_path_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.path_prefix = Some(prefix.into());
        self
    }

    /// Wire a durable executor queue (ADR-018). When set, the
    /// `return_immediately = true` path in `core_send_message` enqueues
    /// a [`turul_a2a::durable_executor::QueuedExecutorJob`] on this
    /// queue instead of spawning the executor locally, and the
    /// capability flag `RuntimeConfig::supports_return_immediately` is
    /// turned **back on** as an implementation detail — the capability
    /// cannot be claimed without supplying the queue.
    ///
    /// Adopters should typically reach for the SQS-specific helper
    /// [`Self::with_sqs_return_immediately`] instead; this generic
    /// method exists so adopters can inject in-memory fakes for tests
    /// or provide non-SQS transports (Kinesis, Step Functions task
    /// token, self-invoke, etc.).
    pub fn with_durable_executor(
        mut self,
        queue: Arc<dyn turul_a2a::durable_executor::DurableExecutorQueue>,
    ) -> Self {
        self.durable_executor_queue = Some(queue);
        self.runtime_config.supports_return_immediately = true;
        self
    }

    /// Wire the AWS SQS durable executor queue (ADR-018). Ergonomic
    /// helper over [`Self::with_durable_executor`] that constructs a
    /// [`SqsDurableExecutorQueue`] from a `queue_url` + shared SQS
    /// client. Requires the `sqs` feature.
    #[cfg(feature = "sqs")]
    pub fn with_sqs_return_immediately(
        self,
        queue_url: impl Into<String>,
        sqs_client: Arc<aws_sdk_sqs::Client>,
    ) -> Self {
        let queue = Arc::new(SqsDurableExecutorQueue::new(queue_url, sqs_client));
        self.with_durable_executor(queue)
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
    /// ADR-013 §4.3 errata: `.storage()` wires storage traits only. It does
    /// **not** auto-register the storage as a push-delivery store, even if
    /// the backend happens to implement [`A2aPushDeliveryStore`]. To opt in
    /// to push delivery, call [`Self::push_delivery_store`] explicitly and
    /// call `with_push_dispatch_enabled(true)` on the storage instance
    /// before passing it here. Non-push deployments need neither.
    pub fn storage<S>(mut self, storage: S) -> Self
    where
        S: A2aTaskStorage
            + A2aPushNotificationStorage
            + A2aEventStore
            + A2aAtomicStore
            + A2aCancellationSupervisor
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
    pub fn cancellation_supervisor(mut self, s: impl A2aCancellationSupervisor + 'static) -> Self {
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
    pub fn push_delivery_store(mut self, store: impl A2aPushDeliveryStore + 'static) -> Self {
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
        let executor = self
            .executor
            .ok_or(A2aError::Internal("executor is required".into()))?;
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

        // ADR-017 §Decision Bug 1: the Lambda execution environment
        // freezes after the HTTP response is flushed, so any
        // `tokio::spawn`'d executor continuation is opportunistic only
        // (ADR-013 §4.4). Refuse `SendMessageConfiguration.return_immediately
        // = true` at the `core_send_message` entry point via this
        // capability flag instead of silently orphaning the executor.
        //
        // ADR-018: if a durable executor queue is wired via
        // [`Self::with_durable_executor`] (or the SQS helper), the
        // capability is re-enabled because enqueueing is durable
        // across the Lambda freeze. The explicit re-enable lives in
        // the builder method so the capability cannot be claimed
        // without the mechanism.
        let mut runtime_config = self.runtime_config;
        if self.durable_executor_queue.is_none() {
            runtime_config.supports_return_immediately = false;
        }

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
            durable_executor_queue: self.durable_executor_queue,
        };

        let router = build_router(state.clone());

        let path_prefix = match self.path_prefix {
            None => None,
            Some(p) if p.is_empty() || p == "/" => None,
            Some(p) => {
                if !p.starts_with('/') {
                    return Err(A2aError::InvalidRequest {
                        message: format!(
                            "LambdaA2aServerBuilder::strip_path_prefix: prefix must start with '/'; got {p:?}"
                        ),
                    });
                }
                if p.ends_with('/') {
                    return Err(A2aError::InvalidRequest {
                        message: format!(
                            "LambdaA2aServerBuilder::strip_path_prefix: prefix must not end with '/' (except '/'); got {p:?}"
                        ),
                    });
                }
                Some(Arc::from(p))
            }
        };

        Ok(LambdaA2aHandler {
            router,
            state,
            path_prefix,
        })
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
    /// Held for ADR-018 SQS-event dispatch (`handle_sqs`). Always
    /// populated — the SQS handler method is gated behind the `sqs`
    /// feature but the state is carried unconditionally so callers can
    /// reach it for diagnostics or future non-HTTP / non-SQS handlers.
    #[cfg_attr(not(feature = "sqs"), allow(dead_code))]
    state: AppState,
    /// Leading path segment to strip from each incoming HTTP request
    /// before the A2A router sees it. `None` = no stripping. Set via
    /// [`LambdaA2aServerBuilder::strip_path_prefix`].
    path_prefix: Option<Arc<str>>,
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
        let axum_req = match &self.path_prefix {
            Some(prefix) => adapter::strip_request_path_prefix(axum_req, prefix),
            None => axum_req,
        };

        let axum_resp = tower::ServiceExt::oneshot(self.router.clone(), axum_req)
            .await
            .map_err(|e| lambda_http::Error::from(format!("Router error: {e}")))?;

        axum_to_lambda_response(axum_resp).await
    }

    /// Handle a raw Lambda HTTP event delivered as `serde_json::Value`
    /// and return the API Gateway v2 / Function URL response JSON.
    ///
    /// This is the framework's canonical HTTP envelope conversion
    /// primitive — useful when a single Lambda function routes among
    /// HTTP and one or more non-framework triggers (EventBridge,
    /// DynamoDB streams, custom invoke) and the adopter drives
    /// [`lambda_runtime::run`] with `serde_json::Value` directly.
    /// Handles the `LambdaRequest` deserialization, the text-vs-base64
    /// content-type detection, and the `ApiGatewayV2httpResponse`
    /// rebuild.
    ///
    /// Body encoding: text for `text/*`, `application/json`,
    /// `application/xml`, `application/javascript`, or anything with
    /// `charset=`; base64 otherwise.
    ///
    /// Path-prefix stripping (see
    /// [`LambdaA2aServerBuilder::strip_path_prefix`]) is applied
    /// because dispatch flows through [`Self::handle`]. Adopters who
    /// only serve HTTP should continue to use [`Self::run_http_only`]
    /// or [`LambdaA2aServerBuilder::run`]; this entry point exists for
    /// composition with adopter-owned third triggers.
    pub async fn handle_http_event_value(
        &self,
        value: serde_json::Value,
    ) -> Result<serde_json::Value, lambda_runtime::Error> {
        use base64::Engine;
        use http_body_util::BodyExt;

        let lambda_req: lambda_http::request::LambdaRequest = serde_json::from_value(value)
            .map_err(|e| lambda_runtime::Error::from(format!("invalid HTTP event: {e}")))?;
        let req: lambda_http::Request = lambda_req.into();

        let resp = self
            .handle(req)
            .await
            .map_err(|e| lambda_runtime::Error::from(format!("handler error: {e}")))?;
        let (parts, body) = resp.into_parts();

        let bytes = body
            .collect()
            .await
            .map_err(|e| lambda_runtime::Error::from(format!("body collect: {e}")))?
            .to_bytes();

        let ct = parts
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_ascii_lowercase();
        let is_text = ct.starts_with("text/")
            || ct.starts_with("application/json")
            || ct.starts_with("application/xml")
            || ct.starts_with("application/javascript")
            || ct.contains("charset=");

        let (body_str, is_base64) = if bytes.is_empty() {
            (None, false)
        } else if is_text {
            match std::str::from_utf8(&bytes) {
                Ok(s) => (Some(s.to_string()), false),
                Err(_) => (
                    Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
                    true,
                ),
            }
        } else {
            (
                Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
                true,
            )
        };

        let mut api_resp = aws_lambda_events::apigw::ApiGatewayV2httpResponse::default();
        api_resp.status_code = parts.status.as_u16() as i64;
        api_resp.headers = parts.headers;
        api_resp.body = body_str.map(aws_lambda_events::encodings::Body::Text);
        api_resp.is_base64_encoded = is_base64;

        serde_json::to_value(api_resp)
            .map_err(|e| lambda_runtime::Error::from(format!("serialise HTTP response: {e}")))
    }

    /// Handle an SQS batch from the durable executor queue (ADR-018).
    ///
    /// Returns `SqsBatchResponse` whose `batch_item_failures` list
    /// tells the event source mapping which records to retry.
    /// Requires `FunctionResponseTypes: ["ReportBatchItemFailures"]`
    /// on the mapping.
    ///
    /// Each record: deserialize envelope → load task → terminal-no-op
    /// check → cancel-marker direct-CANCELED commit → run executor.
    /// See `durable::drive_sqs_batch` for the full semantics.
    #[cfg(feature = "sqs")]
    pub async fn handle_sqs(
        &self,
        event: aws_lambda_events::event::sqs::SqsEvent,
    ) -> aws_lambda_events::event::sqs::SqsBatchResponse {
        durable::drive_sqs_batch(&self.state, event).await
    }

    /// Run this handler as a pure HTTP Lambda. Strict: any non-HTTP
    /// event shape (SQS, DynamoDB stream, scheduler, …) causes
    /// `lambda_http::run` to return a deserialization error on
    /// invocation.
    ///
    /// Appropriate for Lambdas whose only trigger is a Function URL,
    /// API Gateway, or ALB. The request Lambda in the two-Lambda
    /// durable-executor topology is the canonical caller.
    pub async fn run_http_only(self) -> Result<(), lambda_runtime::Error> {
        let handler = Arc::new(self);
        lambda_http::run(lambda_http::service_fn(move |event| {
            let h = Arc::clone(&handler);
            async move { h.handle(event).await }
        }))
        .await
    }
}

// Default entrypoint when the `sqs` feature is off — `.run()` aliases
// the HTTP-only runner. With the `sqs` feature enabled, `.run()` lives
// in `durable.rs` and routes HTTP+SQS via the classifier.
#[cfg(not(feature = "sqs"))]
impl LambdaA2aHandler {
    /// Default Lambda runner. Dispatches based on the event shape the
    /// binary receives. Without the `sqs` feature this is HTTP-only
    /// (equivalent to [`LambdaA2aHandler::run_http_only`]).
    pub async fn run(self) -> Result<(), lambda_runtime::Error> {
        self.run_http_only().await
    }
}

impl LambdaA2aServerBuilder {
    /// Build and run in one call. Sugar for
    /// `self.build()?.run().await`. Hides the handler from the adopter —
    /// use `.build()` + `handler.run_*()` instead when the handler is
    /// needed for unit tests, custom middleware, or an explicit
    /// topology runner.
    pub async fn run(self) -> Result<(), lambda_runtime::Error> {
        let handler = self.build().map_err(|e| {
            lambda_runtime::Error::from(format!("LambdaA2aServerBuilder::run: {e}"))
        })?;
        handler.run().await
    }
}
