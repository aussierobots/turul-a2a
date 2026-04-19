//! Integration tests for turul-a2a-auth middleware.
//!
//! Tests AnyOf(ApiKey, Bearer) and security contribution correctness.

use std::collections::HashMap;
use std::sync::Arc;

use turul_a2a::middleware::{A2aMiddleware, AnyOfMiddleware, MiddlewareError, RequestContext};
use turul_a2a_auth::{ApiKeyMiddleware, StaticApiKeyLookup};

fn api_key_middleware() -> ApiKeyMiddleware {
    let mut keys = HashMap::new();
    keys.insert("test-key-1".to_string(), "key-owner".to_string());
    ApiKeyMiddleware::new(Arc::new(StaticApiKeyLookup::new(keys)), "X-API-Key")
}

// =========================================================
// AnyOf(ApiKey, Bearer-stub) — tests with a fake Bearer that always fails
// =========================================================

/// Fake Bearer middleware that always returns HttpChallenge (no real JWT)
struct FakeBearerMiddleware;

#[async_trait::async_trait]
impl A2aMiddleware for FakeBearerMiddleware {
    async fn before_request(&self, _ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
        Err(MiddlewareError::HttpChallenge {
            status: 401,
            www_authenticate: "Bearer realm=\"a2a\"".into(),
        })
    }

    fn security_contribution(&self) -> turul_a2a::middleware::SecurityContribution {
        turul_a2a::middleware::SecurityContribution::new().with_scheme(
            "bearer",
            turul_a2a_proto::SecurityScheme {
                scheme: Some(
                    turul_a2a_proto::security_scheme::Scheme::HttpAuthSecurityScheme(
                        turul_a2a_proto::HttpAuthSecurityScheme {
                            description: String::new(),
                            scheme: "Bearer".into(),
                            bearer_format: "JWT".into(),
                        },
                    ),
                ),
            },
            vec![],
        )
    }
}

#[tokio::test]
async fn anyof_api_key_succeeds_when_bearer_fails() {
    let any = AnyOfMiddleware::new(vec![
        Arc::new(api_key_middleware()),
        Arc::new(FakeBearerMiddleware),
    ]);

    let mut ctx = RequestContext::new();
    ctx.headers
        .insert("x-api-key", "test-key-1".parse().unwrap());

    any.before_request(&mut ctx).await.unwrap();
    assert!(ctx.identity.is_authenticated());
    assert_eq!(ctx.identity.owner(), "key-owner");
}

#[tokio::test]
async fn anyof_both_fail_returns_highest_precedence() {
    let any = AnyOfMiddleware::new(vec![
        Arc::new(api_key_middleware()),
        Arc::new(FakeBearerMiddleware),
    ]);

    // No API key, no Bearer token → both fail
    let mut ctx = RequestContext::new();
    let err = any.before_request(&mut ctx).await.unwrap_err();

    // HttpChallenge (from Bearer) beats Unauthenticated (from ApiKey)
    assert!(
        matches!(err, MiddlewareError::HttpChallenge { .. }),
        "HttpChallenge should win: {err:?}"
    );
}

// =========================================================
// SecurityContribution from AnyOf
// =========================================================

#[test]
fn anyof_security_contribution_has_or_semantics() {
    let any = AnyOfMiddleware::new(vec![
        Arc::new(api_key_middleware()),
        Arc::new(FakeBearerMiddleware),
    ]);

    let contrib = any.security_contribution();

    // Should have both schemes
    assert_eq!(contrib.schemes.len(), 2);
    let names: Vec<&str> = contrib.schemes.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"apiKey"));
    assert!(names.contains(&"bearer"));

    // Should have 2 separate requirements (OR semantics)
    assert_eq!(
        contrib.requirements.len(),
        2,
        "AnyOf should produce 2 requirement entries (OR)"
    );
}

#[test]
fn api_key_security_contribution_matches_proto() {
    let mw = api_key_middleware();
    let contrib = mw.security_contribution();

    assert_eq!(contrib.schemes.len(), 1);
    let (name, scheme) = &contrib.schemes[0];
    assert_eq!(name, "apiKey");

    match &scheme.scheme {
        Some(turul_a2a_proto::security_scheme::Scheme::ApiKeySecurityScheme(s)) => {
            assert_eq!(s.location, "header");
            assert_eq!(s.name, "X-API-Key");
        }
        other => panic!("Expected ApiKeySecurityScheme, got: {other:?}"),
    }

    assert_eq!(contrib.requirements.len(), 1);
    assert!(contrib.requirements[0].schemes.contains_key("apiKey"));
}

#[test]
fn bearer_security_contribution_matches_proto() {
    let mw = FakeBearerMiddleware;
    let contrib = mw.security_contribution();

    assert_eq!(contrib.schemes.len(), 1);
    let (name, scheme) = &contrib.schemes[0];
    assert_eq!(name, "bearer");

    match &scheme.scheme {
        Some(turul_a2a_proto::security_scheme::Scheme::HttpAuthSecurityScheme(s)) => {
            assert_eq!(s.scheme, "Bearer");
            assert_eq!(s.bearer_format, "JWT");
        }
        other => panic!("Expected HttpAuthSecurityScheme, got: {other:?}"),
    }

    assert_eq!(contrib.requirements.len(), 1);
    assert!(contrib.requirements[0].schemes.contains_key("bearer"));
}
