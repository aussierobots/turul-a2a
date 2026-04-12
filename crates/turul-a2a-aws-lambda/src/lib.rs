//! AWS Lambda adapter for turul-a2a.
//!
//! Thin wrapper: converts Lambda events to axum requests, delegates to the
//! same Router, converts responses back. Per ADR-008/ADR-009:
//! - Authorizer context mapped via synthetic headers with anti-spoofing
//! - Streaming supported via durable event store (D3)
//! - SSE responses are buffered: task executes, events are collected, returned as one response
//! - `POST /message:stream` executes the task within the Lambda invocation and returns all events
//! - `GET /tasks/{id}:subscribe` replays stored events (works best for terminal tasks)
//!
//! Lambda streaming is request-scoped (not persistent SSE connections). The durable
//! event store ensures events survive across invocations. Clients reconnect with
//! `Last-Event-ID` for continuation.

mod adapter;
mod auth;
mod no_streaming;

pub use adapter::{lambda_to_axum_request, axum_to_lambda_response};
pub use auth::{AuthorizerMapping, LambdaAuthorizerMiddleware};
pub use no_streaming::NoStreamingLayer;

use std::sync::Arc;

use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::middleware::{A2aMiddleware, MiddlewareStack};
use turul_a2a::router::{build_router, AppState};
use turul_a2a::storage::{
    A2aAtomicStore, A2aEventStore, A2aPushNotificationStorage, A2aTaskStorage,
};
use turul_a2a::streaming::TaskEventBroker;

/// Builder for Lambda A2A handler.
///
/// Use `.storage(my_storage)` to supply a single backend implementing all storage traits.
/// This satisfies ADR-009's same-backend requirement and enables streaming via durable
/// event store.
pub struct LambdaA2aServerBuilder {
    executor: Option<Arc<dyn AgentExecutor>>,
    task_storage: Option<Arc<dyn A2aTaskStorage>>,
    push_storage: Option<Arc<dyn A2aPushNotificationStorage>>,
    event_store: Option<Arc<dyn A2aEventStore>>,
    atomic_store: Option<Arc<dyn A2aAtomicStore>>,
    middleware: Vec<Arc<dyn A2aMiddleware>>,
}

impl LambdaA2aServerBuilder {
    pub fn new() -> Self {
        Self {
            executor: None,
            task_storage: None,
            push_storage: None,
            event_store: None,
            atomic_store: None,
            middleware: vec![],
        }
    }

    pub fn executor(mut self, exec: impl AgentExecutor + 'static) -> Self {
        self.executor = Some(Arc::new(exec));
        self
    }

    /// Set all storage from a single backend instance.
    ///
    /// This is the **preferred** method — a single struct implementing all storage
    /// traits guarantees the same-backend requirement (ADR-009).
    pub fn storage<S>(mut self, storage: S) -> Self
    where
        S: A2aTaskStorage
            + A2aPushNotificationStorage
            + A2aEventStore
            + A2aAtomicStore
            + Clone
            + 'static,
    {
        self.task_storage = Some(Arc::new(storage.clone()));
        self.push_storage = Some(Arc::new(storage.clone()));
        self.event_store = Some(Arc::new(storage.clone()));
        self.atomic_store = Some(Arc::new(storage));
        self
    }

    /// Set task storage individually. Must be paired with event_store/atomic_store
    /// for same-backend compliance.
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

        // Same-backend enforcement (ADR-009)
        let task_backend = task_storage.backend_name();
        let push_backend = push_storage.backend_name();
        let event_backend = event_store.backend_name();
        let atomic_backend = atomic_store.backend_name();
        if task_backend != push_backend
            || task_backend != event_backend
            || task_backend != atomic_backend
        {
            return Err(A2aError::Internal(format!(
                "Storage backend mismatch: task={task_backend}, push={push_backend}, \
                 event={event_backend}, atomic={atomic_backend}. \
                 ADR-009 requires all storage traits to share the same backend. \
                 Use .storage() for unified backend."
            )));
        }

        let state = AppState {
            executor,
            task_storage,
            push_storage,
            event_store,
            atomic_store,
            event_broker: TaskEventBroker::new(),
            middleware_stack: Arc::new(MiddlewareStack::new(self.middleware)),
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
