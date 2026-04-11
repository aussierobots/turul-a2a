//! AWS Lambda adapter for turul-a2a.
//!
//! Thin wrapper: converts Lambda events to axum requests, delegates to the
//! same Router, converts responses back. Per ADR-008:
//! - Request/response only (D2 scope)
//! - Streaming endpoints return UnsupportedOperationError
//! - Authorizer context mapped via synthetic headers with anti-spoofing

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
use turul_a2a::storage::{A2aPushNotificationStorage, A2aTaskStorage};
use turul_a2a::streaming::TaskEventBroker;

/// Builder for Lambda A2A handler.
pub struct LambdaA2aServerBuilder {
    executor: Option<Arc<dyn AgentExecutor>>,
    task_storage: Option<Arc<dyn A2aTaskStorage>>,
    push_storage: Option<Arc<dyn A2aPushNotificationStorage>>,
    middleware: Vec<Arc<dyn A2aMiddleware>>,
}

impl LambdaA2aServerBuilder {
    pub fn new() -> Self {
        Self {
            executor: None,
            task_storage: None,
            push_storage: None,
            middleware: vec![],
        }
    }

    pub fn executor(mut self, exec: impl AgentExecutor + 'static) -> Self {
        self.executor = Some(Arc::new(exec));
        self
    }

    pub fn task_storage(mut self, s: impl A2aTaskStorage + 'static) -> Self {
        self.task_storage = Some(Arc::new(s));
        self
    }

    pub fn push_storage(mut self, s: impl A2aPushNotificationStorage + 'static) -> Self {
        self.push_storage = Some(Arc::new(s));
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

        let state = AppState {
            executor,
            task_storage,
            push_storage,
            event_broker: TaskEventBroker::new(), // inert on Lambda
            middleware_stack: Arc::new(MiddlewareStack::new(self.middleware)),
        };

        // Build router, then layer NoStreamingLayer on top
        let router = build_router(state);
        let router = router.layer(NoStreamingLayer);

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
