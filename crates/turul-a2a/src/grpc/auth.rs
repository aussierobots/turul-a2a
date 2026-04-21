//! Tower layer that runs the shared `MiddlewareStack` against a tonic
//! server (ADR-007 §3, ADR-014 §2.4).
//!
//! This is a transport adapter only — the authentication *business logic*
//! lives in `crate::middleware::stack::MiddlewareStack` and is shared
//! verbatim with the HTTP path. The two transports differ only in:
//!   * request body type (`tonic::body::Body` vs. `axum::body::Body`)
//!   * error encoding (gRPC status codes with `ErrorInfo` vs. HTTP JSON
//!     error body)
//!
//! On auth success the layer injects a [`RequestContext`] into the HTTP
//! request's extensions; tonic surfaces those same extensions to the
//! service impl via `tonic::Request::extensions()`, so the gRPC adapter
//! reads the authenticated owner through `RequestContext::identity` —
//! exactly like the HTTP handlers do.
//!
//! On auth failure the layer produces a gRPC-formatted response carrying
//! the correct `tonic::Status` code. No `A2aError`, no JSON error body —
//! transport-level auth failures bypass the A2A error model per ADR-004.

use std::sync::Arc;
use std::task::{Context, Poll};

use http::{Request, Response};
use tonic::body::Body;
use tower::{Layer, Service};

use crate::middleware::bearer::extract_bearer_token;
use crate::middleware::context::{AuthIdentity, RequestContext};
use crate::middleware::error::MiddlewareError;
use crate::middleware::stack::MiddlewareStack;

/// Tower layer that wraps a tonic service with the shared middleware stack.
#[derive(Clone)]
pub struct GrpcAuthLayer {
    stack: Arc<MiddlewareStack>,
}

impl GrpcAuthLayer {
    pub fn new(stack: Arc<MiddlewareStack>) -> Self {
        Self { stack }
    }
}

impl<S> Layer<S> for GrpcAuthLayer {
    type Service = GrpcAuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcAuthService {
            inner,
            stack: self.stack.clone(),
        }
    }
}

/// Tower service produced by [`GrpcAuthLayer`].
#[derive(Clone)]
pub struct GrpcAuthService<S> {
    inner: S,
    stack: Arc<MiddlewareStack>,
}

impl<S> Service<Request<Body>> for GrpcAuthService<S>
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

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let stack = self.stack.clone();
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let headers = req.headers().clone();

            // Matches the HTTP layer's "no middleware configured" short
            // circuit — still inject a default RequestContext so handlers
            // can read `identity.owner()` without conditional access.
            if stack.is_empty() {
                req.extensions_mut().insert(RequestContext {
                    bearer_token: None,
                    headers,
                    identity: AuthIdentity::Anonymous,
                    extensions: Default::default(),
                });
                return inner.call(req).await;
            }

            // Same header extraction as the HTTP AuthLayer — tonic
            // metadata surfaces as `http::HeaderMap`, so `authorization`
            // + `x-api-key` arrive at the same keys.
            let bearer_token = headers
                .get(http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(extract_bearer_token);

            let mut ctx = RequestContext {
                bearer_token,
                headers: headers.clone(),
                identity: AuthIdentity::Anonymous,
                extensions: Default::default(),
            };

            match stack.before_request(&mut ctx).await {
                Ok(()) => {
                    req.extensions_mut().insert(ctx);
                    inner.call(req).await
                }
                Err(err) => Ok(middleware_error_to_grpc_response(&err)),
            }
        })
    }
}

/// Map a `MiddlewareError` to a gRPC-formatted HTTP response.
///
/// Uses `tonic::Status::into_http()` so the response carries proper
/// gRPC trailers (`grpc-status`, `grpc-message`) — `tonic::Status` is
/// the canonical encoding for transport-level auth failure.
fn middleware_error_to_grpc_response(err: &MiddlewareError) -> Response<Body> {
    let message = format!("{err:?}");
    let status = match err {
        MiddlewareError::Unauthenticated(_) | MiddlewareError::HttpChallenge { .. } => {
            tonic::Status::unauthenticated(message)
        }
        MiddlewareError::Forbidden(_) => tonic::Status::permission_denied(message),
        MiddlewareError::Internal(_) => tonic::Status::internal(message),
    };
    status.into_http()
}
