//! Route integration tests — verify proto-defined HTTP routes dispatch correctly.
//!
//! Uses axum's tower::ServiceExt to test the router without binding a socket.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::executor::AgentExecutor;
use turul_a2a::router::{build_router, AppState};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Message, Task};

/// Minimal test executor that returns the agent card and stubs execute.
struct TestExecutor;

#[async_trait::async_trait]
impl AgentExecutor for TestExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        _message: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), turul_a2a::error::A2aError> {
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            name: "Test Agent".into(),
            description: "A test agent".into(),
            supported_interfaces: vec![turul_a2a_proto::AgentInterface {
                url: "http://localhost:3000".into(),
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
                extended_agent_card: Some(false),
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
}

fn test_state() -> AppState {
    let s = InMemoryA2aStorage::new();
    AppState {
        executor: Arc::new(TestExecutor),
        task_storage: Arc::new(s.clone()),
        push_storage: Arc::new(s.clone()),
        event_store: std::sync::Arc::new(s.clone()),
        atomic_store: std::sync::Arc::new(s),
        event_broker: turul_a2a::streaming::TaskEventBroker::new(),
        middleware_stack: std::sync::Arc::new(turul_a2a::middleware::MiddlewareStack::new(vec![])),
        runtime_config: turul_a2a::server::RuntimeConfig::default(),
        in_flight: std::sync::Arc::new(turul_a2a::server::in_flight::InFlightRegistry::new()),
        cancellation_supervisor: std::sync::Arc::new(turul_a2a::storage::InMemoryA2aStorage::new()),
        push_delivery_store: None,
    }
}

async fn response_status(router: axum::Router, req: Request<Body>) -> u16 {
    let resp = router.oneshot(req).await.unwrap();
    resp.status().as_u16()
}

async fn response_body(router: axum::Router, req: Request<Body>) -> serde_json::Value {
    let resp = router.oneshot(req).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap_or_default()
}

/// Assert route dispatches to a handler (not axum's empty-body 404).
/// A handler 404 (TaskNotFound) has a JSON body; axum's 404 is empty.
async fn assert_route_dispatches(router: axum::Router, req: Request<Body>) {
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    if status == 404 {
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(!body.is_empty(), "Route returned 404 with empty body — not registered");
    }
}

// =========================================================
// Agent Card Discovery — proto line 124, RFC 8615
// =========================================================

#[tokio::test]
async fn well_known_agent_card_returns_200() {
    let router = build_router(test_state());
    let req = Request::get("/.well-known/agent-card.json")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let status = response_status(router, req).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn well_known_agent_card_has_required_fields() {
    let router = build_router(test_state());
    let req = Request::get("/.well-known/agent-card.json")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let body = response_body(router, req).await;
    assert!(body.get("name").is_some(), "AgentCard must have name");
    assert!(
        body.get("description").is_some(),
        "AgentCard must have description"
    );
    assert!(
        body.get("version").is_some(),
        "AgentCard must have version"
    );
}

#[tokio::test]
async fn extended_agent_card_returns_400_when_not_configured() {
    let router = build_router(test_state());
    let req = Request::get("/extendedAgentCard")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let status = response_status(router, req).await;
    // ExtendedAgentCardNotConfiguredError -> HTTP 400
    assert_eq!(status, 400);
}

// =========================================================
// Message routes — proto lines 23, 35
// =========================================================

#[tokio::test]
async fn post_message_send_routes() {
    let router = build_router(test_state());
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from("{}"))
        .unwrap();
    let status = response_status(router, req).await;
    // Handler is stub — returns 400 (UnsupportedOperation)
    // What matters: the route matched, not 404
    assert_ne!(status, 404, "/message:send must route, not 404");
}

#[tokio::test]
async fn post_message_stream_routes() {
    let router = build_router(test_state());
    let req = Request::post("/message:stream")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let status = response_status(router, req).await;
    assert_ne!(status, 404, "/message:stream must route, not 404");
}

// =========================================================
// Task routes — proto lines 47, 57, 66, 78
// =========================================================

#[tokio::test]
async fn get_task_routes() {
    let router = build_router(test_state());
    let req = Request::get("/tasks/some-task-id")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    // Handler returns 404 with body (TaskNotFound) — that's correct routing
    assert_route_dispatches(router, req).await;
}

#[tokio::test]
async fn list_tasks_routes() {
    let router = build_router(test_state());
    let req = Request::get("/tasks").header("a2a-version", "1.0")
        .body(Body::empty()).unwrap();
    let status = response_status(router, req).await;
    assert_ne!(status, 404, "GET /tasks must route");
}

#[tokio::test]
async fn cancel_task_routes() {
    let router = build_router(test_state());
    let req = Request::post("/tasks/some-id:cancel")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    assert_route_dispatches(router, req).await;
}

#[tokio::test]
async fn subscribe_task_routes() {
    let router = build_router(test_state());
    let req = Request::get("/tasks/some-id:subscribe")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    // Task doesn't exist, so handler returns 404 with body (not axum's empty 404)
    assert_route_dispatches(router, req).await;
}

// =========================================================
// Push notification config routes — proto lines 92-139
// =========================================================

#[tokio::test]
async fn create_push_config_routes() {
    let router = build_router(test_state());
    let req = Request::post("/tasks/t1/pushNotificationConfigs")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from("{}"))
        .unwrap();
    // 404 is from task ownership check (task doesn't exist), not from routing
    assert_route_dispatches(router, req).await;
}

#[tokio::test]
async fn get_push_config_routes() {
    let router = build_router(test_state());
    let req = Request::get("/tasks/t1/pushNotificationConfigs/c1")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    // Returns 404 with body (config not found) — that's correct routing
    assert_route_dispatches(router, req).await;
}

#[tokio::test]
async fn list_push_configs_routes() {
    let router = build_router(test_state());
    let req = Request::get("/tasks/t1/pushNotificationConfigs")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    assert_route_dispatches(router, req).await;
}

#[tokio::test]
async fn delete_push_config_routes() {
    let router = build_router(test_state());
    let req = Request::builder()
        .method("DELETE")
        .uri("/tasks/t1/pushNotificationConfigs/c1")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    assert_route_dispatches(router, req).await;
}

// =========================================================
// Tenant-prefixed routes — proto additional_bindings
// =========================================================

#[tokio::test]
async fn tenant_message_send_routes() {
    let router = build_router(test_state());
    let req = Request::post("/acme-corp/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from("{}"))
        .unwrap();
    let status = response_status(router, req).await;
    assert_ne!(status, 404, "/{{tenant}}/message:send must route");
}

#[tokio::test]
async fn tenant_get_task_routes() {
    let router = build_router(test_state());
    let req = Request::get("/acme-corp/tasks/t1")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    assert_route_dispatches(router, req).await;
}

#[tokio::test]
async fn tenant_list_tasks_routes() {
    let router = build_router(test_state());
    let req = Request::get("/acme-corp/tasks")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let status = response_status(router, req).await;
    assert_ne!(status, 404, "/{{tenant}}/tasks must route");
}

#[tokio::test]
async fn tenant_cancel_task_routes() {
    let router = build_router(test_state());
    let req = Request::post("/acme-corp/tasks/t1:cancel")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    assert_route_dispatches(router, req).await;
}

#[tokio::test]
async fn tenant_extended_agent_card_routes() {
    let router = build_router(test_state());
    let req = Request::get("/acme-corp/extendedAgentCard")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let status = response_status(router, req).await;
    assert_ne!(status, 404, "/{{tenant}}/extendedAgentCard must route");
}

// =========================================================
// Error response shape — ErrorInfo in HTTP errors
// =========================================================

#[tokio::test]
async fn http_error_includes_error_info() {
    let router = build_router(test_state());
    // Extended agent card is not configured -> should return ErrorInfo
    let req = Request::get("/extendedAgentCard")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let body = response_body(router, req).await;

    let details = body["error"]["details"].as_array();
    assert!(details.is_some(), "HTTP error must have details array");
    let details = details.unwrap();
    assert!(!details.is_empty(), "details must not be empty");
    assert_eq!(
        details[0]["@type"],
        "type.googleapis.com/google.rpc.ErrorInfo"
    );
    assert_eq!(details[0]["domain"], "a2a-protocol.org");
    assert!(details[0]["reason"].is_string());
}

// =========================================================
// Nonexistent route returns 404
// =========================================================

#[tokio::test]
async fn unknown_route_returns_404() {
    let router = build_router(test_state());
    let req = Request::get("/nonexistent/path")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let status = response_status(router, req).await;
    assert_eq!(status, 404);
}
