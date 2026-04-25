//! wire-surface E2E tests.
//!
//! Covers body + WWW-Authenticate contract for Bearer and API-key auth
//! failure paths. Complements `auth_tests.rs` by asserting on the
//! -locked wire shape end-to-end.

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tower::util::ServiceExt;

use turul_a2a::middleware::{
    A2aMiddleware, AuthFailureKind, AuthLayer, MiddlewareError, MiddlewareStack, RequestContext,
};

async fn drain_body(
    resp: http::Response<Body>,
) -> (http::StatusCode, http::HeaderMap, serde_json::Value) {
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::json!({}));
    (status, headers, body)
}

/// Build a router with a single middleware that always fails with the
/// given kind/variant. Auth layer is wired as in real server builds.
fn router_with_failing_middleware(
    err_fn: Arc<dyn Fn() -> MiddlewareError + Send + Sync>,
) -> Router {
    struct FailFixed {
        err_fn: Arc<dyn Fn() -> MiddlewareError + Send + Sync>,
    }
    #[async_trait]
    impl A2aMiddleware for FailFixed {
        async fn before_request(&self, _ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
            Err((self.err_fn)())
        }
    }

    let stack = Arc::new(MiddlewareStack::new(vec![Arc::new(FailFixed { err_fn })]));
    let layer = AuthLayer::new(stack);

    // Inner service returns 200 on any path that auth allows through —
    // under these tests, auth always fails so the inner service is
    // irrelevant. Still must be a Router to satisfy AuthLayer.
    let inner: Router = Router::new()
        .route("/message:send", axum::routing::post(|| async { "ok" }))
        .route("/whatever", axum::routing::get(|| async { "ok" }));

    inner.layer(layer)
}

#[tokio::test]
async fn bearer_invalid_token_emits_kind_body_and_rfc6750_challenge() {
    let router = router_with_failing_middleware(Arc::new(|| {
        MiddlewareError::HttpChallenge(AuthFailureKind::InvalidToken)
    }));

    let req = Request::post("/message:send").body(Body::empty()).unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let (status, headers, body) = drain_body(resp).await;

    assert_eq!(status, 401);
    assert_eq!(body["error"].as_str(), Some("invalid_token"));

    let www = headers
        .get(http::header::WWW_AUTHENTICATE)
        .expect("Bearer challenge must set WWW-Authenticate")
        .to_str()
        .unwrap();
    assert_eq!(www, "Bearer realm=\"a2a\", error=\"invalid_token\"");

    // error_description MUST NOT leak validator internals
    assert!(
        !www.contains("error_description"),
        "error_description must be omitted: {www}"
    );
}

#[tokio::test]
async fn bearer_missing_credential_emits_invalid_request() {
    let router = router_with_failing_middleware(Arc::new(|| {
        MiddlewareError::HttpChallenge(AuthFailureKind::MissingCredential)
    }));

    let req = Request::post("/message:send").body(Body::empty()).unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let (status, headers, body) = drain_body(resp).await;

    assert_eq!(status, 401);
    assert_eq!(body["error"].as_str(), Some("missing_credential"));
    let www = headers
        .get(http::header::WWW_AUTHENTICATE)
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(www, "Bearer realm=\"a2a\", error=\"invalid_request\"");
}

#[tokio::test]
async fn api_key_invalid_emits_body_only_no_challenge() {
    let router = router_with_failing_middleware(Arc::new(|| {
        MiddlewareError::Unauthenticated(AuthFailureKind::InvalidApiKey)
    }));

    let req = Request::post("/message:send").body(Body::empty()).unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let (status, headers, body) = drain_body(resp).await;

    assert_eq!(status, 401);
    assert_eq!(body["error"].as_str(), Some("invalid_api_key"));
    assert!(
        headers.get(http::header::WWW_AUTHENTICATE).is_none(),
        "API-key rejection must not emit a Bearer challenge header"
    );
}

#[tokio::test]
async fn api_key_missing_credential_emits_body_only() {
    let router = router_with_failing_middleware(Arc::new(|| {
        MiddlewareError::Unauthenticated(AuthFailureKind::MissingCredential)
    }));

    let req = Request::post("/message:send").body(Body::empty()).unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let (status, headers, body) = drain_body(resp).await;

    assert_eq!(status, 401);
    assert_eq!(body["error"].as_str(), Some("missing_credential"));
    assert!(
        headers.get(http::header::WWW_AUTHENTICATE).is_none(),
        "Unauthenticated variant never emits challenge headers"
    );
}

#[tokio::test]
async fn empty_principal_via_unauthenticated_body_only() {
    let router = router_with_failing_middleware(Arc::new(|| {
        MiddlewareError::Unauthenticated(AuthFailureKind::EmptyPrincipal)
    }));

    let req = Request::post("/message:send").body(Body::empty()).unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let (status, headers, body) = drain_body(resp).await;

    assert_eq!(status, 401);
    assert_eq!(body["error"].as_str(), Some("empty_principal"));
    assert!(headers.get(http::header::WWW_AUTHENTICATE).is_none());
}

#[tokio::test]
async fn charset_safety_no_quote_or_backslash_in_www_authenticate() {
    // / §2.3: RFC 6750 §3 restricts error_description charset.
    // Since we emit error= from a closed enum and omit error_description,
    // the header value can never contain `"` or `\` from dynamic sources.
    // Exercise every kind that emits a Bearer challenge and assert.
    let kinds = [
        AuthFailureKind::MissingCredential,
        AuthFailureKind::InvalidToken,
        AuthFailureKind::EmptyPrincipal,
        AuthFailureKind::InsufficientScope,
    ];

    for kind in kinds {
        let router =
            router_with_failing_middleware(Arc::new(move || MiddlewareError::HttpChallenge(kind)));
        let req = Request::post("/message:send").body(Body::empty()).unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let (status, headers, _body) = drain_body(resp).await;
        assert_eq!(status, 401);

        let www = headers
            .get(http::header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        // Structural `"` around realm and error values is allowed and required
        // by RFC 6750 syntax. What MUST NOT appear is any quote/backslash
        // sourced from a dynamic (non-enum) field — we never emit
        // error_description, so there's nothing dynamic to contain one.
        // Normative check: the header matches the fixed shape exactly.
        let expected = match kind {
            AuthFailureKind::MissingCredential => "Bearer realm=\"a2a\", error=\"invalid_request\"",
            AuthFailureKind::InvalidToken | AuthFailureKind::EmptyPrincipal => {
                "Bearer realm=\"a2a\", error=\"invalid_token\""
            }
            AuthFailureKind::InsufficientScope => {
                "Bearer realm=\"a2a\", error=\"insufficient_scope\""
            }
            _ => unreachable!(),
        };
        assert_eq!(
            www, expected,
            "header must match the fixed shape for {kind:?}"
        );
    }
}
