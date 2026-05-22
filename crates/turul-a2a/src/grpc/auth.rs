//! Tower layer that runs the shared `MiddlewareStack` against a tonic server.
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
//! transport-level auth failures bypass the A2A error model.

use std::sync::Arc;
use std::task::{Context, Poll};

use http::{Request, Response};
use tonic::body::Body;
use tower::{Layer, Service};

use crate::middleware::context::RequestContext;
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
            // circuit — install an anonymous RequestContext so handlers
            // can read `identity.owner()` without conditional access.
            if stack.is_empty() {
                req.extensions_mut()
                    .insert(RequestContext::from_headers(headers));
                return inner.call(req).await;
            }

            let mut ctx = RequestContext::from_headers(headers);
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
///
/// Wire surface parity with the HTTP path: the `grpc-message` value is
/// the same stable snake_case string the HTTP body uses, sourced from
/// [`MiddlewareError::wire_body_string`]. `Internal(msg)` deliberately
/// collapses to `"internal_error"` — the inner String payload is never
/// exposed on the wire.
///
/// Status-code mapping:
/// - `Unauthenticated` / `HttpChallenge(*)` → `UNAUTHENTICATED`
/// - `HttpChallenge(InsufficientScope)` → `PERMISSION_DENIED`
///   (HTTP returns 403 here per RFC 6750 §3; gRPC has no 403 equivalent
///   under `UNAUTHENTICATED`, so the closest match is `PERMISSION_DENIED`
///   — the same code used for `Forbidden(*)`.)
/// - `Forbidden(*)` → `PERMISSION_DENIED`
/// - `Internal(_)` → `INTERNAL`
fn middleware_error_to_grpc_response(err: &MiddlewareError) -> Response<Body> {
    let message = err.wire_body_string();
    let status = match err {
        MiddlewareError::HttpChallenge(crate::middleware::AuthFailureKind::InsufficientScope) => {
            tonic::Status::permission_denied(message)
        }
        MiddlewareError::Unauthenticated(_) | MiddlewareError::HttpChallenge(_) => {
            tonic::Status::unauthenticated(message)
        }
        MiddlewareError::Forbidden(_) => tonic::Status::permission_denied(message),
        MiddlewareError::Internal(_) => tonic::Status::internal(message),
    };
    status.into_http()
}
