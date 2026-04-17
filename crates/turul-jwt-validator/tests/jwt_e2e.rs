//! JwtValidator E2E tests with real signed tokens and mock JWKS server.
//!
//! Uses wiremock to serve JWKS, jsonwebtoken to sign RS256 tokens,
//! and rsa to generate keypairs.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rsa::pkcs1::EncodeRsaPrivateKey;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use turul_jwt_validator::JwtValidator;

/// Generate an RSA keypair and return (private_key_pem, jwks_json with kid).
fn generate_rsa_keypair(kid: &str) -> (String, serde_json::Value) {
    use rsa::RsaPrivateKey;

    // rsa 0.9 uses rand_core 0.6's CryptoRngCore; the workspace-level rand 0.10
    // is not compatible. Use rsa's re-exported OsRng to avoid version skew.
    let mut rng = rsa::rand_core::OsRng;
    let private_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
    let private_pem = private_key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap();

    let public_key = private_key.to_public_key();

    // We need n and e as base64url for JWKS. Extract from the public key.
    let n = rsa::traits::PublicKeyParts::n(&public_key);
    let e = rsa::traits::PublicKeyParts::e(&public_key);

    let n_bytes = n.to_bytes_be();
    let e_bytes = e.to_bytes_be();

    use base64::Engine;
    let n_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&n_bytes);
    let e_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&e_bytes);

    let jwks = json!({
        "keys": [{
            "kty": "RSA",
            "kid": kid,
            "alg": "RS256",
            "n": n_b64,
            "e": e_b64,
        }]
    });

    (private_pem.to_string(), jwks)
}

/// Sign a JWT with the given claims using the private key.
fn sign_token(private_pem: &str, kid: &str, claims: &serde_json::Value) -> String {
    let encoding_key = EncodingKey::from_rsa_pem(private_pem.as_bytes()).unwrap();
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_string());
    jsonwebtoken::encode(&header, claims, &encoding_key).unwrap()
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

async fn setup_jwks_server(jwks: &serde_json::Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/jwks.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jwks))
        .mount(&server)
        .await;
    server
}

// =========================================================
// Valid token → claims extracted
// =========================================================

#[tokio::test]
async fn valid_token_returns_claims() {
    let (pem, jwks) = generate_rsa_keypair("key-1");
    let server = setup_jwks_server(&jwks).await;

    let validator = JwtValidator::new(
        format!("{}/{}", server.uri(), ".well-known/jwks.json"),
        "test-audience",
    )
    .with_issuer("test-issuer")
    .with_refresh_interval(Duration::from_millis(100));

    let claims = json!({
        "sub": "user-123",
        "iss": "test-issuer",
        "aud": "test-audience",
        "exp": now_epoch() + 3600,
        "iat": now_epoch(),
    });

    let token = sign_token(&pem, "key-1", &claims);
    let result = validator.validate(&token).await.unwrap();

    assert_eq!(result.sub, "user-123");
    assert_eq!(result.iss, "test-issuer");
}

// =========================================================
// Expired token → rejected
// =========================================================

#[tokio::test]
async fn expired_token_rejected() {
    let (pem, jwks) = generate_rsa_keypair("key-2");
    let server = setup_jwks_server(&jwks).await;

    let validator = JwtValidator::new(
        format!("{}/{}", server.uri(), ".well-known/jwks.json"),
        "test-audience",
    )
    .with_refresh_interval(Duration::from_millis(100));

    let claims = json!({
        "sub": "user-expired",
        "aud": "test-audience",
        "exp": now_epoch() - 3600, // expired 1 hour ago
        "iat": now_epoch() - 7200,
    });

    let token = sign_token(&pem, "key-2", &claims);
    let err = validator.validate(&token).await.unwrap_err();
    assert!(
        matches!(err, turul_jwt_validator::JwtValidationError::TokenExpired),
        "Expected TokenExpired, got: {err:?}"
    );
}

// =========================================================
// Wrong audience → rejected
// =========================================================

#[tokio::test]
async fn wrong_audience_rejected() {
    let (pem, jwks) = generate_rsa_keypair("key-3");
    let server = setup_jwks_server(&jwks).await;

    let validator = JwtValidator::new(
        format!("{}/{}", server.uri(), ".well-known/jwks.json"),
        "expected-audience",
    )
    .with_refresh_interval(Duration::from_millis(100));

    let claims = json!({
        "sub": "user",
        "aud": "wrong-audience",
        "exp": now_epoch() + 3600,
    });

    let token = sign_token(&pem, "key-3", &claims);
    let err = validator.validate(&token).await.unwrap_err();
    assert!(
        matches!(
            err,
            turul_jwt_validator::JwtValidationError::InvalidAudience
        ),
        "Expected InvalidAudience, got: {err:?}"
    );
}

// =========================================================
// Wrong issuer → rejected
// =========================================================

#[tokio::test]
async fn wrong_issuer_rejected() {
    let (pem, jwks) = generate_rsa_keypair("key-4");
    let server = setup_jwks_server(&jwks).await;

    let validator = JwtValidator::new(
        format!("{}/{}", server.uri(), ".well-known/jwks.json"),
        "test-audience",
    )
    .with_issuer("expected-issuer")
    .with_refresh_interval(Duration::from_millis(100));

    let claims = json!({
        "sub": "user",
        "iss": "wrong-issuer",
        "aud": "test-audience",
        "exp": now_epoch() + 3600,
    });

    let token = sign_token(&pem, "key-4", &claims);
    let err = validator.validate(&token).await.unwrap_err();
    assert!(
        matches!(err, turul_jwt_validator::JwtValidationError::InvalidIssuer),
        "Expected InvalidIssuer, got: {err:?}"
    );
}

// =========================================================
// Kid-miss → JWKS refetch → success
// =========================================================

#[tokio::test]
async fn kid_miss_triggers_refetch() {
    // Generate a keypair for "key-new" — the validator won't have this cached initially
    let (pem_new, jwks_new) = generate_rsa_keypair("key-new");

    // Serve JWKS with key-new — the validator starts with empty cache,
    // first request with kid="key-new" triggers initial fetch
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/jwks.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&jwks_new))
        .mount(&server)
        .await;

    let validator = JwtValidator::new(
        format!("{}/{}", server.uri(), ".well-known/jwks.json"),
        "test-audience",
    )
    .with_refresh_interval(Duration::from_millis(10)); // fast refetch for testing

    let claims = json!({
        "sub": "user-refetch",
        "aud": "test-audience",
        "exp": now_epoch() + 3600,
    });

    // Sign with key-new — validator doesn't have it cached yet, triggers refetch
    let token = sign_token(&pem_new, "key-new", &claims);
    let result = validator.validate(&token).await.unwrap();
    assert_eq!(result.sub, "user-refetch");
}

// =========================================================
// Scope claim extracted
// =========================================================

#[tokio::test]
async fn scope_claim_extracted() {
    let (pem, jwks) = generate_rsa_keypair("key-scope");
    let server = setup_jwks_server(&jwks).await;

    let validator = JwtValidator::new(
        format!("{}/{}", server.uri(), ".well-known/jwks.json"),
        "test-audience",
    )
    .with_refresh_interval(Duration::from_millis(100));

    let claims = json!({
        "sub": "user-scoped",
        "aud": "test-audience",
        "exp": now_epoch() + 3600,
        "scope": "a2a:read a2a:write",
    });

    let token = sign_token(&pem, "key-scope", &claims);
    let result = validator.validate(&token).await.unwrap();
    assert_eq!(result.scope.as_deref(), Some("a2a:read a2a:write"));
}

// =========================================================
// Extra claims captured
// =========================================================

#[tokio::test]
async fn extra_claims_captured() {
    let (pem, jwks) = generate_rsa_keypair("key-extra");
    let server = setup_jwks_server(&jwks).await;

    let validator = JwtValidator::new(
        format!("{}/{}", server.uri(), ".well-known/jwks.json"),
        "test-audience",
    )
    .with_refresh_interval(Duration::from_millis(100));

    let claims = json!({
        "sub": "user-extra",
        "aud": "test-audience",
        "exp": now_epoch() + 3600,
        "custom_field": "custom_value",
        "role": "admin",
    });

    let token = sign_token(&pem, "key-extra", &claims);
    let result = validator.validate(&token).await.unwrap();
    assert_eq!(result.extra.get("custom_field").unwrap(), "custom_value");
    assert_eq!(result.extra.get("role").unwrap(), "admin");
}
