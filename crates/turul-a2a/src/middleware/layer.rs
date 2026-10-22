//! Tower middleware layer for transport-level auth.
//!
//! Intercepts raw HTTP requests BEFORE axum dispatch.
//! Auth failures produce HTTP 401/403 directly — never JSON-RPC errors.

use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use http::{Request, Response};
use tower::{Layer, Service};

use super::context::RequestContext;
use super::error::MiddlewareError;
use super::stack::MiddlewareStack;

/// Tower Layer that wraps a service with auth middleware.
#[derive(Clone)]
pub struct AuthLayer {
    stack: Arc<MiddlewareStack>,
}

impl AuthLayer {
    pub fn new(stack: Arc<MiddlewareStack>) -> Self {
        Self { stack }
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthService {
            inner,
            stack: self.stack.clone(),
        }
    }
}

/// Tower Service that runs auth middleware before the inner service.
#[derive(Clone)]
pub struct AuthService<S> {
    inner: S,
    stack: Arc<MiddlewareStack>,
}

impl<S> Service<Request<Body>> for AuthService<S>
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
            let path = req.uri().path().to_string();
            let headers = req.headers().clone();

            // Public paths or no-middleware short-circuit: install an
            // anonymous context and pass through.
            if super::is_bypass_path(&path) || stack.is_empty() {
                req.extensions_mut()
                    .insert(RequestContext::from_headers(headers));
                return inner.call(req).await;
            }

            // Run middleware stack against a fresh context.
            let mut ctx = RequestContext::from_headers(headers);
            match stack.before_request(&mut ctx).await {
                Ok(()) => {
                    // Auth passed — inject context into request extensions
                    req.extensions_mut().insert(ctx);
                    inner.call(req).await
                }
                Err(err) => {
                    // Auth failed — return the error directly, never reach handler
                    Ok(middleware_error_to_response(&err, &path))
                }
            }
        })
    }
}

/// Convert a MiddlewareError to an HTTP response.
///
/// Wire surface:
/// - Body: on the JSON-RPC endpoint a JSON-RPC error envelope
///   ([`MiddlewareError::jsonrpc_body`]); elsewhere the HTTP+JSON error
///   body ([`MiddlewareError::http_body`]). Both carry a code, a message
///   and the stable kind as an `ErrorInfo` reason.
/// - `WWW-Authenticate`: emitted only on `HttpChallenge` variants whose kind
///   is a Bearer failure ([`super::AuthFailureKind::bearer_challenge`]): a
///   bare `Bearer realm="a2a"` when the credential is missing (RFC 6750
///   Section 3.1), otherwise with the fixed `error=` code.
///   `error_description` is intentionally omitted to prevent leaking
///   validator internals.
/// - `Internal` is not auth-failure and uses the fixed `INTERNAL_ERROR`
///   kind.
fn middleware_error_to_response(err: &MiddlewareError, path: &str) -> Response<Body> {
    let status = axum::http::StatusCode::from_u16(err.http_status())
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);

    let body = if crate::jsonrpc::is_jsonrpc_path(path) {
        err.jsonrpc_body()
    } else {
        err.http_body()
    };

    // Plain JSON: the transport layer relabels it `application/a2a+json`
    // unless the request speaks the legacy 0.3 protocol.
    let mut builder = Response::builder().status(status).header(
        http::header::CONTENT_TYPE,
        turul_a2a_types::wire::protocol::MEDIA_TYPE_JSON,
    );

    if let MiddlewareError::HttpChallenge(kind) = err
        && let Some(challenge) = kind.bearer_challenge()
    {
        builder = builder.header(http::header::WWW_AUTHENTICATE, challenge);
    }

    builder
        .body(Body::from(serde_json::to_string(&body).unwrap_or_default()))
        .unwrap_or_else(|_| Response::builder().status(500).body(Body::empty()).unwrap())
}
