//! B0: Transport compliance tests — A2A-Version header and Content-Type validation.

use std::sync::Arc;

use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::router::{AppState, build_router};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a::streaming::TaskEventBroker;
use turul_a2a_types::{Message, Task};

struct DummyExecutor;

#[async_trait::async_trait]
impl AgentExecutor for DummyExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
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
        turul_a2a::card_builder::AgentCardBuilder::new("Transport Test", "1.0.0")
            .description("Tests transport compliance")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build()
            .unwrap()
    }
}

fn test_state() -> AppState {
    let s = InMemoryA2aStorage::new();
    AppState {
        executor: Arc::new(DummyExecutor),
        task_storage: Arc::new(s.clone()),
        push_storage: Arc::new(s.clone()),
        event_store: Arc::new(s.clone()),
        atomic_store: Arc::new(s),
        event_broker: TaskEventBroker::new(),
        middleware_stack: Arc::new(turul_a2a::middleware::MiddlewareStack::new(vec![])),
        runtime_config: turul_a2a::server::RuntimeConfig::default(),
        in_flight: std::sync::Arc::new(turul_a2a::server::in_flight::InFlightRegistry::new()),
        cancellation_supervisor: std::sync::Arc::new(turul_a2a::storage::InMemoryA2aStorage::new()),
        push_delivery_store: None,
        push_dispatcher: None,
    }
}

fn send_body() -> String {
    serde_json::json!({"message":{"messageId":"t1","role":"ROLE_USER","parts":[{"text":"hi"}]}})
        .to_string()
}

async fn json_response(router: axum::Router, req: Request<Body>) -> (u16, serde_json::Value) {
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

// =========================================================
// A2A-Version header (spec §3.6)
// =========================================================

#[tokio::test]
async fn request_with_valid_a2a_version_succeeds() {
    let router = build_router(test_state());
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_body()))
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 200);
}

#[tokio::test]
#[cfg(feature = "compat-v03")]
async fn request_without_a2a_version_accepted_with_compat() {
    // With compat-v03: missing A2A-Version header is accepted.
    // a2a-sdk 0.3.x clients do not send this header.
    let router = build_router(test_state());
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .body(Body::from(send_body()))
        .unwrap();
    let (status, _body) = json_response(router, req).await;
    assert_ne!(
        status, 400,
        "Missing A2A-Version must not be rejected (v0.3 compat)"
    );
}

#[tokio::test]
#[cfg(not(feature = "compat-v03"))]
async fn request_without_a2a_version_rejects_strict() {
    // Without compat-v03: missing A2A-Version header → VersionNotSupportedError
    let router = build_router(test_state());
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .body(Body::from(send_body()))
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(
        status, 400,
        "Missing A2A-Version header → VersionNotSupportedError"
    );
    if let Some(details) = body["error"]["details"].as_array() {
        if !details.is_empty() {
            assert_eq!(details[0]["reason"], "VERSION_NOT_SUPPORTED");
        }
    }
}

#[tokio::test]
async fn request_with_unsupported_version_returns_400() {
    let router = build_router(test_state());
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "99.99")
        .body(Body::from(send_body()))
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 400);
    if let Some(details) = body["error"]["details"].as_array() {
        if !details.is_empty() {
            assert_eq!(details[0]["reason"], "VERSION_NOT_SUPPORTED");
        }
    }
}

#[tokio::test]
async fn agent_card_does_not_require_version_header() {
    // Discovery endpoint is public and should work without A2A-Version
    let router = build_router(test_state());
    let req = Request::get("/.well-known/agent-card.json")
        .body(Body::empty())
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 200, "Agent card should not require A2A-Version");
}

// =========================================================
// Content-Type validation
// =========================================================

#[tokio::test]
async fn post_with_wrong_content_type_is_rejected() {
    // POST with a non-JSON Content-Type should return 415
    let router = build_router(test_state());
    let req = Request::post("/message:send")
        .header("a2a-version", "1.0")
        .header("content-type", "text/plain")
        .body(Body::from(send_body()))
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 415, "Non-JSON Content-Type should return 415");
}

#[tokio::test]
async fn post_without_content_type_allowed_for_empty_body_actions() {
    // POST without Content-Type (e.g., cancel with empty body) should be allowed
    let router = build_router(test_state());
    let req = Request::post("/tasks/nonexistent:cancel")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let (status, _) = json_response(router, req).await;
    // Should get 404 (task not found), not 415
    assert_ne!(
        status, 415,
        "POST without Content-Type + empty body should not be 415"
    );
}

#[tokio::test]
async fn post_with_wrong_content_type_returns_415() {
    let router = build_router(test_state());
    let req = Request::post("/message:send")
        .header("content-type", "text/xml")
        .header("a2a-version", "1.0")
        .body(Body::from("<xml/>"))
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 415, "Non-JSON Content-Type should return 415");
}

#[tokio::test]
async fn post_with_json_content_type_succeeds() {
    let router = build_router(test_state());
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_body()))
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn get_requests_do_not_need_content_type() {
    let router = build_router(test_state());
    let req = Request::get("/tasks")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 200, "GET requests should not require Content-Type");
}
