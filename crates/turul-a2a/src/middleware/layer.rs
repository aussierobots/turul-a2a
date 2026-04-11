//! Tower middleware layer for transport-level auth.
//!
//! Intercepts raw HTTP requests BEFORE axum dispatch.
//! Auth failures produce HTTP 401/403 directly — never JSON-RPC errors.

use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::response::IntoResponse;
use http::{Request, Response};
use tower::{Layer, Service};

use super::bearer::extract_bearer_token;
use super::context::{AuthIdentity, RequestContext};
use super::error::MiddlewareError;
use super::stack::MiddlewareStack;

/// Paths excluded from auth (always public).
const PUBLIC_PATHS: &[&str] = &["/.well-known/agent-card.json"];

fn is_public_path(path: &str) -> bool {
    PUBLIC_PATHS.iter().any(|p| path == *p)
}

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

            // Public paths bypass auth entirely
            if is_public_path(&path) {
                req.extensions_mut().insert(RequestContext {
                    bearer_token: None,
                    headers,
                    identity: AuthIdentity::Anonymous,
                    extensions: Default::default(),
                });
                return inner.call(req).await;
            }

            // Skip auth if no middleware configured (backward compat)
            if stack.is_empty() {
                req.extensions_mut().insert(RequestContext {
                    bearer_token: None,
                    headers,
                    identity: AuthIdentity::Anonymous,
                    extensions: Default::default(),
                });
                return inner.call(req).await;
            }

            // Build request context
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

            // Run middleware stack
            match stack.before_request(&mut ctx).await {
                Ok(()) => {
                    // Auth passed — inject context into request extensions
                    req.extensions_mut().insert(ctx);
                    inner.call(req).await
                }
                Err(err) => {
                    // Auth failed — return HTTP error directly, never reach handler
                    Ok(middleware_error_to_response(&err))
                }
            }
        })
    }
}

/// Convert a MiddlewareError to an HTTP response.
/// This is the transport-level error path — no A2aError, no JSON-RPC.
fn middleware_error_to_response(err: &MiddlewareError) -> Response<Body> {
    let status = axum::http::StatusCode::from_u16(err.http_status())
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);

    let body = serde_json::json!({
        "error": {
            "code": err.http_status(),
            "message": format!("{err:?}"),
        }
    });

    let mut builder = Response::builder().status(status).header(
        http::header::CONTENT_TYPE,
        "application/json",
    );

    // Add WWW-Authenticate header for challenges
    if let MiddlewareError::HttpChallenge {
        www_authenticate, ..
    } = err
    {
        builder = builder.header(http::header::WWW_AUTHENTICATE, www_authenticate.as_str());
    }

    builder
        .body(Body::from(serde_json::to_string(&body).unwrap_or_default()))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap()
        })
}
