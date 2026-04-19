//! NoStreamingLayer — rejects streaming paths with UnsupportedOperationError.
//!
//! Per ADR-008: streaming on Lambda uses the existing A2A error contract,
//! not a Lambda-specific 501.

use std::task::{Context, Poll};

use axum::body::Body;
use http::{Request, Response};
use tower::{Layer, Service};

use turul_a2a::error::A2aError;

/// Tower Layer that rejects streaming paths on Lambda.
#[derive(Clone)]
pub struct NoStreamingLayer;

impl<S> Layer<S> for NoStreamingLayer {
    type Service = NoStreamingService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        NoStreamingService { inner }
    }
}

#[derive(Clone)]
pub struct NoStreamingService<S> {
    inner: S,
}

fn is_streaming_path(path: &str) -> bool {
    path.ends_with(":stream") || path.ends_with(":subscribe")
}

impl<S> Service<Request<Body>> for NoStreamingService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let path = req.uri().path().to_string();

        if is_streaming_path(&path) {
            let err = A2aError::UnsupportedOperation {
                message:
                    "Streaming is not supported on Lambda. Use request/response endpoints instead."
                        .into(),
            };
            let body = err.to_http_error_body();
            let status = axum::http::StatusCode::from_u16(err.http_status())
                .unwrap_or(axum::http::StatusCode::BAD_REQUEST);

            return Box::pin(async move {
                Ok(Response::builder()
                    .status(status)
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap_or_default()))
                    .unwrap())
            });
        }

        let mut inner = self.inner.clone();
        Box::pin(async move { inner.call(req).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_paths_detected() {
        assert!(is_streaming_path("/message:stream"));
        assert!(is_streaming_path("/tenant/message:stream"));
        assert!(is_streaming_path("/tasks/abc:subscribe"));
        assert!(is_streaming_path("/tenant/tasks/abc:subscribe"));

        assert!(!is_streaming_path("/message:send"));
        assert!(!is_streaming_path("/tasks/abc:cancel"));
        assert!(!is_streaming_path("/tasks"));
        assert!(!is_streaming_path("/.well-known/agent-card.json"));
    }
}
