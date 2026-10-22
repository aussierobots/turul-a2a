//! `BearerMiddleware` on a real JWT path: tokens are signed with a P-256
//! key generated per test run and verified by `JwtValidator` against a
//! JWKS document served on a loopback port. No key material is stored in
//! the repository.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use tower::ServiceExt;

use turul_a2a::middleware::{
    A2aMiddleware, AuthFailureKind, AuthLayer, MiddlewareError, MiddlewareStack, RequestContext,
};
use turul_a2a_auth::BearerMiddleware;
use turul_jwt_validator::JwtValidator;

const KID: &str = "test-key";
const ISSUER: &str = "https://issuer.test";
const AUDIENCE: &str = "a2a-test";

/// A signing key plus the JWKS endpoint that publishes its public half.
struct Issuer {
    signing_key: EncodingKey,
    jwks_uri: String,
    _server: tokio::task::JoinHandle<()>,
}

fn generate_signing_key() -> (EncodingKey, Value) {
    let pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).expect("generate P-256");
    let pkcs8 = pair.to_pkcs8v1().expect("pkcs8");
    // Uncompressed SEC1 point: 0x04 || x (32 bytes) || y (32 bytes).
    let point = pair.public_key().as_ref();
    let jwk = json!({
        "kty": "EC",
        "kid": KID,
        "alg": "ES256",
        "crv": "P-256",
        "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
        "y": URL_SAFE_NO_PAD.encode(&point[33..65]),
    });
    (EncodingKey::from_ec_der(pkcs8.as_ref()), jwk)
}

impl Issuer {
    async fn start() -> Self {
        let (signing_key, jwk) = generate_signing_key();
        let jwks = json!({ "keys": [jwk] });
        let app = axum::Router::new().route(
            "/jwks",
            axum::routing::get(move || {
                let jwks = jwks.clone();
                async move { axum::Json(jwks) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind JWKS listener");
        let addr = listener.local_addr().expect("JWKS addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("JWKS server");
        });
        Self {
            signing_key,
            jwks_uri: format!("http://{addr}/jwks"),
            _server: server,
        }
    }

    fn validator(&self) -> Arc<JwtValidator> {
        Arc::new(JwtValidator::new(&self.jwks_uri, AUDIENCE).with_issuer(ISSUER))
    }

    fn sign(&self, claims: &Value) -> String {
        sign_with(&self.signing_key, claims)
    }
}

fn sign_with(key: &EncodingKey, claims: &Value) -> String {
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(KID.into());
    jsonwebtoken::encode(&header, claims, key).expect("sign JWT")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Valid claims for `sub`, merged with `extra` (which may override any).
fn claims(sub: &str, extra: Value) -> Value {
    let mut c = json!({
        "sub": sub,
        "iss": ISSUER,
        "aud": AUDIENCE,
        "iat": now(),
        "exp": now() + 300,
    });
    for (k, v) in extra.as_object().unwrap() {
        c[k] = v.clone();
    }
    c
}

fn ctx_with_token(token: &str) -> RequestContext {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    RequestContext::from_headers(headers)
}

async fn run(mw: &BearerMiddleware, token: &str) -> Result<RequestContext, MiddlewareError> {
    let mut ctx = ctx_with_token(token);
    mw.before_request(&mut ctx).await.map(|()| ctx)
}

fn assert_kind(result: Result<RequestContext, MiddlewareError>, expected: MiddlewareError) {
    let err = result.err().expect("request must be rejected");
    assert_eq!(format!("{err:?}"), format!("{expected:?}"));
}

#[tokio::test]
async fn valid_token_authenticates_sub_with_claims() {
    let issuer = Issuer::start().await;
    let mw = BearerMiddleware::new(issuer.validator());
    let ctx = run(&mw, &issuer.sign(&claims("alice", json!({}))))
        .await
        .expect("valid token");
    assert!(ctx.identity.is_authenticated());
    assert_eq!(ctx.identity.owner(), "alice");
    let claims = ctx
        .identity
        .claims()
        .expect("Bearer identity carries claims");
    assert_eq!(claims["sub"], "alice");
    assert_eq!(claims["iss"], ISSUER);
}

#[tokio::test]
async fn missing_token_is_missing_credential_challenge() {
    let issuer = Issuer::start().await;
    let mw = BearerMiddleware::new(issuer.validator());
    let mut ctx = RequestContext::new();
    let err = mw.before_request(&mut ctx).await.unwrap_err();
    assert!(
        matches!(
            err,
            MiddlewareError::HttpChallenge(AuthFailureKind::MissingCredential)
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn every_validator_failure_is_invalid_token() {
    let issuer = Issuer::start().await;
    let mw = BearerMiddleware::new(issuer.validator());
    let (foreign_key, _) = generate_signing_key();
    let cases = [
        ("malformed", "not-a-jwt".to_string()),
        (
            "expired",
            issuer.sign(&claims(
                "alice",
                json!({"exp": now() - 3600, "iat": now() - 7200}),
            )),
        ),
        (
            "wrong audience",
            issuer.sign(&claims("alice", json!({"aud": "someone-else"}))),
        ),
        (
            "wrong issuer",
            issuer.sign(&claims("alice", json!({"iss": "https://evil.test"}))),
        ),
        (
            "signed by another key",
            sign_with(&foreign_key, &claims("alice", json!({}))),
        ),
    ];
    for (case, token) in cases {
        let err = run(&mw, &token).await.err().expect(case);
        assert!(
            matches!(
                err,
                MiddlewareError::HttpChallenge(AuthFailureKind::InvalidToken)
            ),
            "{case}: {err:?}"
        );
    }
}

#[tokio::test]
async fn empty_or_whitespace_sub_is_empty_principal() {
    let issuer = Issuer::start().await;
    let mw = BearerMiddleware::new(issuer.validator());
    for sub in ["", "   "] {
        assert_kind(
            run(&mw, &issuer.sign(&claims(sub, json!({})))).await,
            MiddlewareError::Unauthenticated(AuthFailureKind::EmptyPrincipal),
        );
    }
}

#[tokio::test]
async fn non_default_principal_claim_is_the_owner() {
    let issuer = Issuer::start().await;
    let mw = BearerMiddleware::new(issuer.validator()).with_principal_claim("email");
    let ctx = run(
        &mw,
        &issuer.sign(&claims("subject-id", json!({"email": "bob@example.com"}))),
    )
    .await
    .expect("valid token");
    assert_eq!(ctx.identity.owner(), "bob@example.com");

    // The configured claim absent: no fallback to `sub`.
    assert_kind(
        run(&mw, &issuer.sign(&claims("subject-id", json!({})))).await,
        MiddlewareError::Unauthenticated(AuthFailureKind::EmptyPrincipal),
    );
}

#[tokio::test]
async fn required_scopes_are_enforced() {
    let issuer = Issuer::start().await;
    let mw =
        BearerMiddleware::new(issuer.validator()).with_required_scopes(vec!["a2a.write".into()]);

    assert_kind(
        run(
            &mw,
            &issuer.sign(&claims("alice", json!({"scope": "a2a.read"}))),
        )
        .await,
        MiddlewareError::HttpChallenge(AuthFailureKind::InsufficientScope),
    );

    let ctx = run(
        &mw,
        &issuer.sign(&claims("alice", json!({"scope": "a2a.read a2a.write"}))),
    )
    .await
    .expect("scope present");
    assert_eq!(ctx.identity.owner(), "alice");
}

/// The wire surface end to end: Bearer middleware behind the auth layer.
async fn call_through_auth_layer(
    mw: BearerMiddleware,
    token: Option<&str>,
) -> (u16, Option<String>, Value) {
    let stack = Arc::new(MiddlewareStack::new(vec![Arc::new(mw)]));
    let router = axum::Router::new()
        .route("/tasks", axum::routing::get(|| async { "ok" }))
        .layer(AuthLayer::new(stack));
    let mut req = http::Request::get("/tasks");
    if let Some(t) = token {
        req = req.header(http::header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let resp = router
        .oneshot(req.body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let challenge = resp
        .headers()
        .get(http::header::WWW_AUTHENTICATE)
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, challenge, body)
}

#[tokio::test]
async fn wire_insufficient_scope_is_403_with_body_and_challenge() {
    let issuer = Issuer::start().await;
    let mw =
        BearerMiddleware::new(issuer.validator()).with_required_scopes(vec!["a2a.write".into()]);
    let token = issuer.sign(&claims("alice", json!({"scope": "a2a.read"})));
    let (status, challenge, body) = call_through_auth_layer(mw, Some(&token)).await;
    assert_eq!(status, 403);
    assert_eq!(body, json!({"error": "insufficient_scope"}));
    assert_eq!(
        challenge.as_deref(),
        Some(r#"Bearer realm="a2a", error="insufficient_scope""#)
    );
}

#[tokio::test]
async fn wire_missing_token_is_401_bare_challenge() {
    let issuer = Issuer::start().await;
    let mw = BearerMiddleware::new(issuer.validator());
    let (status, challenge, body) = call_through_auth_layer(mw, None).await;
    assert_eq!(status, 401);
    assert_eq!(body, json!({"error": "missing_credential"}));
    assert_eq!(challenge.as_deref(), Some(r#"Bearer realm="a2a""#));
}

#[tokio::test]
async fn wire_expired_token_is_401_invalid_token_without_description() {
    let issuer = Issuer::start().await;
    let mw = BearerMiddleware::new(issuer.validator());
    let token = issuer.sign(&claims(
        "alice",
        json!({"exp": now() - 3600, "iat": now() - 7200}),
    ));
    let (status, challenge, body) = call_through_auth_layer(mw, Some(&token)).await;
    assert_eq!(status, 401);
    assert_eq!(body, json!({"error": "invalid_token"}));
    assert_eq!(
        challenge.as_deref(),
        Some(r#"Bearer realm="a2a", error="invalid_token""#)
    );
}

#[tokio::test]
async fn wire_valid_token_reaches_handler() {
    let issuer = Issuer::start().await;
    let mw = BearerMiddleware::new(issuer.validator());
    let token = issuer.sign(&claims("alice", json!({})));
    let (status, challenge, _) = call_through_auth_layer(mw, Some(&token)).await;
    assert_eq!(status, 200);
    assert_eq!(challenge, None);
}
