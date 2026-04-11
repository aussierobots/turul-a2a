//! A2A transport compliance middleware.
//!
//! Validates A2A-Version header and Content-Type on POST requests.
//! Runs as the outermost Tower layer, before auth.

use std::task::{Context, Poll};

use axum::body::Body;
use http::{Request, Response};
use tower::{Layer, Service};

use crate::error::A2aError;

/// Supported A2A protocol version.
const SUPPORTED_VERSION: &str = "1.0";

/// Paths excluded from version validation (public discovery).
const VERSION_EXEMPT_PATHS: &[&str] = &["/.well-known/agent-card.json"];

fn is_version_exempt(path: &str) -> bool {
    VERSION_EXEMPT_PATHS.iter().any(|p| path == *p)
}

/// Tower Layer for A2A transport compliance.
#[derive(Clone)]
pub struct TransportComplianceLayer;

impl<S> Layer<S> for TransportComplianceLayer {
    type Service = TransportComplianceService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TransportComplianceService { inner }
    }
}

#[derive(Clone)]
pub struct TransportComplianceService<S> {
    inner: S,
}

impl<S> Service<Request<Body>> for TransportComplianceService<S>
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
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let path = req.uri().path().to_string();
            let method = req.method().clone();

            // 1. A2A-Version validation (skip for discovery)
            if !is_version_exempt(&path) {
                let version = req
                    .headers()
                    .get("a2a-version")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("0.3"); // Per spec: empty = 0.3

                if version != SUPPORTED_VERSION {
                    let err = A2aError::VersionNotSupported {
                        version: version.to_string(),
                    };
                    return Ok(err.into_response_body());
                }
            }

            // 2. Content-Type validation for POST requests with a body
            if method == http::Method::POST {
                let has_content_type = req.headers().contains_key(http::header::CONTENT_TYPE);
                if has_content_type {
                    let content_type = req
                        .headers()
                        .get(http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");

                    if !content_type.contains("application/json") {
                        let err = A2aError::ContentTypeNotSupported {
                            content_type: content_type.to_string(),
                        };
                        return Ok(err.into_response_body());
                    }
                }
                // POST without Content-Type header is allowed (e.g., cancel with empty body)
            }

            inner.call(req).await
        })
    }
}

impl A2aError {
    /// Convert to an HTTP response body for transport-level errors.
    fn into_response_body(&self) -> Response<Body> {
        let status = axum::http::StatusCode::from_u16(self.http_status())
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let body = self.to_http_error_body();
        Response::builder()
            .status(status)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap_or_default()))
            .unwrap_or_else(|_| {
                Response::builder()
                    .status(500)
                    .body(Body::empty())
                    .unwrap()
            })
    }
}
