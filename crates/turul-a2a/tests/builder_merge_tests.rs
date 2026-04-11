//! Builder security contribution merge tests.
//!
//! Exercises security_schemes + security_requirements merge through the real
//! A2aServer::builder(), not just middleware unit tests.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::middleware::{
    A2aMiddleware, AnyOfMiddleware, AuthIdentity, MiddlewareError, RequestContext,
    SecurityContribution,
};
use turul_a2a::A2aServer;
use turul_a2a_types::{Message, Task};

// =========================================================
// Test executor
// =========================================================

struct DummyExecutor;

#[async_trait]
impl AgentExecutor for DummyExecutor {
    async fn execute(&self, _task: &mut Task, _msg: &Message) -> Result<(), A2aError> {
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            name: "Merge Test Agent".into(),
            description: "Tests security merge".into(),
            supported_interfaces: vec![],
            provider: None,
            version: "1.0.0".into(),
            documentation_url: None,
            capabilities: None,
            security_schemes: HashMap::new(), // empty — middleware fills these
            security_requirements: vec![],
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![],
            signatures: vec![],
            icon_url: None,
        }
    }
}

// =========================================================
// Test middleware implementations with known contributions
// =========================================================

struct ApiKeyTestMiddleware;

#[async_trait]
impl A2aMiddleware for ApiKeyTestMiddleware {
    async fn before_request(&self, ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
        ctx.identity = AuthIdentity::Authenticated {
            owner: "test".into(),
            claims: None,
        };
        Ok(())
    }

    fn security_contribution(&self) -> SecurityContribution {
        SecurityContribution::new().with_scheme(
            "apiKey",
            turul_a2a_proto::SecurityScheme {
                scheme: Some(
                    turul_a2a_proto::security_scheme::Scheme::ApiKeySecurityScheme(
                        turul_a2a_proto::ApiKeySecurityScheme {
                            description: String::new(),
                            location: "header".into(),
                            name: "X-API-Key".into(),
                        },
                    ),
                ),
            },
            vec![],
        )
    }
}

struct BearerTestMiddleware;

#[async_trait]
impl A2aMiddleware for BearerTestMiddleware {
    async fn before_request(&self, ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
        ctx.identity = AuthIdentity::Authenticated {
            owner: "test".into(),
            claims: Some(serde_json::json!({"sub": "test"})),
        };
        Ok(())
    }

    fn security_contribution(&self) -> SecurityContribution {
        SecurityContribution::new().with_scheme(
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
            vec!["a2a:read".into()],
        )
    }
}

/// Same scheme name "bearer" but DIFFERENT definition (Basic instead of Bearer)
struct ConflictingMiddleware;

#[async_trait]
impl A2aMiddleware for ConflictingMiddleware {
    async fn before_request(&self, ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
        ctx.identity = AuthIdentity::Authenticated {
            owner: "test".into(),
            claims: None,
        };
        Ok(())
    }

    fn security_contribution(&self) -> SecurityContribution {
        SecurityContribution::new().with_scheme(
            "bearer", // SAME name as BearerTestMiddleware
            turul_a2a_proto::SecurityScheme {
                scheme: Some(
                    turul_a2a_proto::security_scheme::Scheme::HttpAuthSecurityScheme(
                        turul_a2a_proto::HttpAuthSecurityScheme {
                            description: String::new(),
                            scheme: "Basic".into(), // DIFFERENT definition
                            bearer_format: String::new(),
                        },
                    ),
                ),
            },
            vec![],
        )
    }
}

/// Identical to BearerTestMiddleware contribution (for dedup testing)
struct DuplicateBearerMiddleware;

#[async_trait]
impl A2aMiddleware for DuplicateBearerMiddleware {
    async fn before_request(&self, ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
        ctx.identity = AuthIdentity::Authenticated {
            owner: "test".into(),
            claims: None,
        };
        Ok(())
    }

    fn security_contribution(&self) -> SecurityContribution {
        SecurityContribution::new().with_scheme(
            "bearer", // same name
            turul_a2a_proto::SecurityScheme {
                scheme: Some(
                    turul_a2a_proto::security_scheme::Scheme::HttpAuthSecurityScheme(
                        turul_a2a_proto::HttpAuthSecurityScheme {
                            description: String::new(),
                            scheme: "Bearer".into(), // same definition
                            bearer_format: "JWT".into(),
                        },
                    ),
                ),
            },
            vec!["a2a:read".into()],
        )
    }
}

async fn get_agent_card(router: axum::Router) -> serde_json::Value {
    let req = Request::get("/.well-known/agent-card.json")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

// =========================================================
// Tests
// =========================================================

#[tokio::test]
async fn no_middleware_agent_card_has_no_security() {
    let server = A2aServer::builder()
        .executor(DummyExecutor)
        .build()
        .unwrap();
    let card = get_agent_card(server.into_router()).await;
    // No security schemes or requirements
    let schemes = card.get("securitySchemes");
    assert!(
        schemes.is_none() || schemes.unwrap().as_object().map_or(true, |m| m.is_empty()),
        "No middleware should mean no security schemes"
    );
}

#[tokio::test]
async fn single_middleware_populates_agent_card() {
    let server = A2aServer::builder()
        .executor(DummyExecutor)
        .middleware(Arc::new(ApiKeyTestMiddleware))
        .build()
        .unwrap();
    let card = get_agent_card(server.into_router()).await;

    let schemes = card["securitySchemes"].as_object().unwrap();
    assert!(schemes.contains_key("apiKey"), "Should have apiKey scheme");

    let reqs = card["securityRequirements"].as_array().unwrap();
    assert_eq!(reqs.len(), 1);
}

#[tokio::test]
async fn anyof_middleware_produces_or_requirements() {
    let any = AnyOfMiddleware::new(vec![
        Arc::new(ApiKeyTestMiddleware),
        Arc::new(BearerTestMiddleware),
    ]);
    let server = A2aServer::builder()
        .executor(DummyExecutor)
        .middleware(Arc::new(any))
        .build()
        .unwrap();
    let card = get_agent_card(server.into_router()).await;

    let schemes = card["securitySchemes"].as_object().unwrap();
    assert!(schemes.contains_key("apiKey"));
    assert!(schemes.contains_key("bearer"));

    let reqs = card["securityRequirements"].as_array().unwrap();
    assert_eq!(reqs.len(), 2, "AnyOf should produce 2 requirements (OR)");
}

#[tokio::test]
async fn stacked_middleware_produces_and_requirements() {
    let server = A2aServer::builder()
        .executor(DummyExecutor)
        .middleware(Arc::new(ApiKeyTestMiddleware))
        .middleware(Arc::new(BearerTestMiddleware))
        .build()
        .unwrap();
    let card = get_agent_card(server.into_router()).await;

    let schemes = card["securitySchemes"].as_object().unwrap();
    assert!(schemes.contains_key("apiKey"));
    assert!(schemes.contains_key("bearer"));

    let reqs = card["securityRequirements"].as_array().unwrap();
    assert_eq!(reqs.len(), 1, "Stacked should produce 1 requirement (AND)");
    let req = &reqs[0]["schemes"];
    assert!(req.get("apiKey").is_some(), "AND requirement should include apiKey");
    assert!(req.get("bearer").is_some(), "AND requirement should include bearer");
}

#[tokio::test]
async fn same_name_identical_definition_deduplicates() {
    let server = A2aServer::builder()
        .executor(DummyExecutor)
        .middleware(Arc::new(BearerTestMiddleware))
        .middleware(Arc::new(DuplicateBearerMiddleware))
        .build()
        .unwrap();
    let card = get_agent_card(server.into_router()).await;

    let schemes = card["securitySchemes"].as_object().unwrap();
    assert_eq!(schemes.len(), 1, "Identical defs should dedup to 1 scheme");
    assert!(schemes.contains_key("bearer"));
}

#[test]
fn same_name_different_definition_fails_build() {
    let result = A2aServer::builder()
        .executor(DummyExecutor)
        .middleware(Arc::new(BearerTestMiddleware))
        .middleware(Arc::new(ConflictingMiddleware))
        .build();

    assert!(
        result.is_err(),
        "Same scheme name with different definition should fail build()"
    );
}

#[tokio::test]
async fn bearer_scopes_appear_in_requirements() {
    let server = A2aServer::builder()
        .executor(DummyExecutor)
        .middleware(Arc::new(BearerTestMiddleware))
        .build()
        .unwrap();
    let card = get_agent_card(server.into_router()).await;

    let reqs = card["securityRequirements"].as_array().unwrap();
    let bearer_scopes = &reqs[0]["schemes"]["bearer"]["list"];
    let scopes: Vec<&str> = bearer_scopes
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(scopes.contains(&"a2a:read"), "Scopes should be in requirements");
}
