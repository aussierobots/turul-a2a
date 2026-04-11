//! Phase 3: Router auth integration tests.
//!
//! Proves auth middleware runs at transport level before handlers/JSON-RPC dispatch.

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
    A2aMiddleware, AuthIdentity, MiddlewareError, MiddlewareStack, RequestContext,
};
use turul_a2a::router::{build_router, AppState};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a::streaming::TaskEventBroker;
use turul_a2a_types::{Message, Task};

// =========================================================
// Test executor
// =========================================================

struct TestExecutor;

#[async_trait]
impl AgentExecutor for TestExecutor {
    async fn execute(&self, task: &mut Task, _msg: &Message) -> Result<(), A2aError> {
        let mut p = task.as_proto().clone();
        p.status = Some(turul_a2a_proto::TaskStatus {
            state: turul_a2a_proto::TaskState::Completed.into(),
            message: None,
            timestamp: None,
        });
        *task = Task::try_from(p).unwrap();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            name: "Auth Test Agent".into(),
            description: "Agent for auth tests".into(),
            supported_interfaces: vec![turul_a2a_proto::AgentInterface {
                url: "http://localhost".into(),
                protocol_binding: "JSONRPC".into(),
                tenant: String::new(),
                protocol_version: "1.0".into(),
            }],
            provider: None,
            version: "1.0.0".into(),
            documentation_url: None,
            capabilities: Some(turul_a2a_proto::AgentCapabilities {
                streaming: Some(false),
                push_notifications: Some(false),
                extensions: vec![],
                extended_agent_card: Some(true),
            }),
            security_schemes: HashMap::new(),
            security_requirements: vec![],
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![],
            signatures: vec![],
            icon_url: None,
        }
    }

    fn extended_agent_card(
        &self,
        _claims: Option<&serde_json::Value>,
    ) -> Option<turul_a2a_proto::AgentCard> {
        Some(turul_a2a_proto::AgentCard {
            name: "Auth Test Agent (Extended)".into(),
            description: "Extended card with more details".into(),
            supported_interfaces: vec![],
            provider: None,
            version: "1.0.0".into(),
            documentation_url: None,
            capabilities: None,
            security_schemes: HashMap::new(),
            security_requirements: vec![],
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![],
            signatures: vec![],
            icon_url: None,
        })
    }
}

// =========================================================
// Test middleware: accepts requests with X-Test-Auth header
// =========================================================

struct TestAuthMiddleware;

#[async_trait]
impl A2aMiddleware for TestAuthMiddleware {
    async fn before_request(&self, ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
        let auth_value = ctx
            .headers
            .get("x-test-auth")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        match auth_value {
            Some(owner) if !owner.is_empty() => {
                ctx.identity = AuthIdentity::Authenticated {
                    owner,
                    claims: Some(serde_json::json!({"test": true})),
                };
                Ok(())
            }
            _ => Err(MiddlewareError::HttpChallenge {
                status: 401,
                www_authenticate: "TestAuth realm=\"test\"".into(),
            }),
        }
    }
}

// =========================================================
// State builders
// =========================================================

fn state_no_auth() -> AppState {
    let s = InMemoryA2aStorage::new();
    AppState {
        executor: Arc::new(TestExecutor),
        task_storage: Arc::new(s.clone()),
        push_storage: Arc::new(s),
        event_broker: TaskEventBroker::new(),
        middleware_stack: Arc::new(MiddlewareStack::new(vec![])),
    }
}

fn state_with_auth() -> AppState {
    let s = InMemoryA2aStorage::new();
    AppState {
        executor: Arc::new(TestExecutor),
        task_storage: Arc::new(s.clone()),
        push_storage: Arc::new(s),
        event_broker: TaskEventBroker::new(),
        middleware_stack: Arc::new(MiddlewareStack::new(vec![
            Arc::new(TestAuthMiddleware),
        ])),
    }
}

fn send_body(id: &str) -> String {
    serde_json::json!({"message":{"messageId":id,"role":"ROLE_USER","parts":[{"text":"hello"}]}})
        .to_string()
}

fn jrpc_body(method: &str, id: i64) -> String {
    serde_json::json!({"jsonrpc":"2.0","method":method,"params":{},"id":id}).to_string()
}

async fn response(router: axum::Router, req: Request<Body>) -> (u16, Vec<u8>) {
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, body)
}

async fn json_response(router: axum::Router, req: Request<Body>) -> (u16, serde_json::Value) {
    let (status, body) = response(router, req).await;
    let json = serde_json::from_slice(&body).unwrap_or_default();
    (status, json)
}

async fn response_with_headers(
    router: axum::Router,
    req: Request<Body>,
) -> (u16, http::HeaderMap, serde_json::Value) {
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&body).unwrap_or_default();
    (status, headers, json)
}

// =========================================================
// No middleware = backward compatible anonymous access
// =========================================================

#[tokio::test]
async fn no_middleware_allows_all_requests() {
    let router = build_router(state_no_auth());

    // /message:send works without auth
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .body(Body::from(send_body("no-auth")))
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 200, "No middleware should allow all requests");
}

#[tokio::test]
async fn no_middleware_jsonrpc_works() {
    let router = build_router(state_no_auth());
    let req = Request::post("/jsonrpc")
        .header("content-type", "application/json")
        .body(Body::from(jrpc_body("ListTasks", 1)))
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);
    assert!(body.get("error").is_none());
}

// =========================================================
// TODO: Tests below require Tower auth layer wired into router.
// They will fail until Phase 3 implementation is done.
// =========================================================

// Placeholder tests that document the contract.
// These test the EXPECTED behavior once the Tower layer exists.
// Mark as ignored until the layer is wired.

#[tokio::test]
#[ignore = "Requires Tower auth layer (Phase 3 implementation)"]
async fn auth_middleware_rejects_unauthenticated_http() {
    // POST /message:send without X-Test-Auth header → HTTP 401
    // Response should have WWW-Authenticate header
    // Response body should be AIP-193 format, NOT JSON-RPC
}

#[tokio::test]
#[ignore = "Requires Tower auth layer (Phase 3 implementation)"]
async fn auth_middleware_rejects_unauthenticated_jsonrpc() {
    // POST /jsonrpc without X-Test-Auth header → HTTP 401 (NOT JSON-RPC error)
    // The JSON-RPC body is never parsed
}

#[tokio::test]
#[ignore = "Requires Tower auth layer (Phase 3 implementation)"]
async fn well_known_agent_card_excluded_from_auth() {
    // GET /.well-known/agent-card.json → 200 even with auth middleware configured
}

#[tokio::test]
#[ignore = "Requires Tower auth layer (Phase 3 implementation)"]
async fn extended_agent_card_requires_authenticated_identity() {
    // GET /extendedAgentCard without auth → 401
    // GET /extendedAgentCard with auth (X-Test-Auth: user) → 200
    // This gates on is_authenticated(), not claims.is_some()
}

#[tokio::test]
#[ignore = "Requires Tower auth layer (Phase 3 implementation)"]
async fn authenticated_owner_flows_to_storage() {
    // Send message with X-Test-Auth: user-123
    // Get the task back
    // Verify it was stored with owner="user-123" (not "anonymous")
    // Another user (X-Test-Auth: user-456) cannot see it
}

#[tokio::test]
#[ignore = "Requires Tower auth layer (Phase 3 implementation)"]
async fn jsonrpc_unauthenticated_is_http_401_not_jsonrpc_error() {
    // POST /jsonrpc without auth → HTTP 401
    // Response body is NOT {"jsonrpc":"2.0","error":{...}}
    // Response body IS {"error":{"code":401,...}} (AIP-193)
}

#[tokio::test]
#[ignore = "Requires Tower auth layer (Phase 3 implementation)"]
async fn tenant_plus_auth_scoped_together() {
    // POST /acme/message:send with X-Test-Auth: user-a → creates task
    // GET /acme/tasks/{id} with X-Test-Auth: user-b → 404 (wrong owner)
    // GET /other/tasks/{id} with X-Test-Auth: user-a → 404 (wrong tenant)
    // GET /acme/tasks/{id} with X-Test-Auth: user-a → 200 (correct both)
}
