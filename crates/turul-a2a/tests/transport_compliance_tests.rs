//! Transport compliance tests — A2A-Version service parameter and Content-Type validation.

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
        durable_executor_queue: None,
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

/// Assert an HTTP+JSON error body carries exactly one ErrorInfo with the
/// given reason, plus the AIP-193 `status` name.
fn assert_http_error(body: &serde_json::Value, status: &str, reason: &str) {
    assert_eq!(body["error"]["status"], status, "body: {body}");
    assert_eq!(
        body["error"]["details"],
        serde_json::json!([{
            "@type": "type.googleapis.com/google.rpc.ErrorInfo",
            "reason": reason,
            "domain": "a2a-protocol.org",
        }]),
        "body: {body}"
    );
}

/// Assert a JSON-RPC error envelope (`id: null`, since the transport layer
/// rejects before reading the request) with the given code and reason.
fn assert_jsonrpc_error(body: &serde_json::Value, code: i64, reason: &str) {
    assert_eq!(body["jsonrpc"], "2.0", "body: {body}");
    assert!(body["id"].is_null(), "body: {body}");
    assert_eq!(body["error"]["code"], code, "body: {body}");
    assert_eq!(
        body["error"]["data"],
        serde_json::json!([{
            "@type": "type.googleapis.com/google.rpc.ErrorInfo",
            "reason": reason,
            "domain": "a2a-protocol.org",
        }]),
        "body: {body}"
    );
}

fn jsonrpc_list_body() -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ListTasks", "params": {}}).to_string()
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
        "Missing A2A-Version means 0.3 (spec Section 3.6.2) → VersionNotSupportedError"
    );
    assert_http_error(&body, "FAILED_PRECONDITION", "VERSION_NOT_SUPPORTED");
}

#[tokio::test]
#[cfg(not(feature = "compat-v03"))]
async fn empty_a2a_version_is_0_3_and_rejected_strict() {
    let router = build_router(test_state());
    let req = Request::get("/tasks")
        .header("a2a-version", "")
        .body(Body::empty())
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 400);
    assert_http_error(&body, "FAILED_PRECONDITION", "VERSION_NOT_SUPPORTED");
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
    assert_http_error(&body, "FAILED_PRECONDITION", "VERSION_NOT_SUPPORTED");
}

#[tokio::test]
async fn patch_version_is_ignored_in_negotiation() {
    // Spec Section 3.6: patch numbers MUST NOT be considered.
    let router = build_router(test_state());
    let req = Request::get("/tasks")
        .header("a2a-version", "1.0.1")
        .body(Body::empty())
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn version_query_parameter_is_honoured() {
    // Spec Section 3.6.1: clients MAY send A2A-Version as a request parameter.
    let router = build_router(test_state());
    let req = Request::get("/tasks?A2A-Version=1.0")
        .body(Body::empty())
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 200);

    let router = build_router(test_state());
    let req = Request::get("/tasks?A2A-Version=2.0")
        .body(Body::empty())
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 400);
    assert_http_error(&body, "FAILED_PRECONDITION", "VERSION_NOT_SUPPORTED");
}

#[tokio::test]
async fn jsonrpc_unsupported_version_is_jsonrpc_error_envelope() {
    let router = build_router(test_state());
    let req = Request::post("/jsonrpc")
        .header("content-type", "application/json")
        .header("a2a-version", "99.0")
        .body(Body::from(jsonrpc_list_body()))
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200, "JSON-RPC errors travel in a 200 response");
    assert_jsonrpc_error(&body, -32009, "VERSION_NOT_SUPPORTED");
}

#[tokio::test]
async fn jsonrpc_wrong_content_type_is_jsonrpc_error_envelope() {
    let router = build_router(test_state());
    let req = Request::post("/jsonrpc")
        .header("content-type", "text/plain")
        .header("a2a-version", "1.0")
        .body(Body::from(jsonrpc_list_body()))
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200, "JSON-RPC errors travel in a 200 response");
    assert_jsonrpc_error(&body, -32005, "CONTENT_TYPE_NOT_SUPPORTED");
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
    let router = build_router(test_state());
    let req = Request::post("/message:send")
        .header("a2a-version", "1.0")
        .header("content-type", "text/plain")
        .body(Body::from(send_body()))
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 400, "ContentTypeNotSupportedError is HTTP 400");
    assert_http_error(&body, "INVALID_ARGUMENT", "CONTENT_TYPE_NOT_SUPPORTED");
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
    assert_eq!(
        status, 404,
        "POST without Content-Type + empty body reaches the handler (TaskNotFound)"
    );
}

#[tokio::test]
async fn post_with_xml_content_type_is_content_type_not_supported() {
    let router = build_router(test_state());
    let req = Request::post("/message:send")
        .header("content-type", "text/xml")
        .header("a2a-version", "1.0")
        .body(Body::from("<xml/>"))
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 400, "ContentTypeNotSupportedError is HTTP 400");
    assert_http_error(&body, "INVALID_ARGUMENT", "CONTENT_TYPE_NOT_SUPPORTED");
}

#[tokio::test]
async fn post_with_json_lookalike_content_type_is_rejected() {
    let router = build_router(test_state());
    let req = Request::post("/message:send")
        .header("content-type", "application/jsonx")
        .header("a2a-version", "1.0")
        .body(Body::from(send_body()))
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 400);
    assert_http_error(&body, "INVALID_ARGUMENT", "CONTENT_TYPE_NOT_SUPPORTED");
}

#[tokio::test]
async fn post_with_a2a_json_media_type_and_charset_succeeds() {
    // Spec Section 14.1 registers application/a2a+json; Section 11.1 says
    // it SHOULD be used for HTTP+JSON requests.
    for content_type in [
        "application/a2a+json",
        "application/a2a+json; charset=utf-8",
        "application/json; charset=utf-8",
    ] {
        let router = build_router(test_state());
        let req = Request::post("/message:send")
            .header("content-type", content_type)
            .header("a2a-version", "1.0")
            .body(Body::from(send_body()))
            .unwrap();
        let (status, body) = json_response(router, req).await;
        assert_eq!(status, 200, "{content_type}: {body}");
    }
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

/// Spec Section 11.1: on the HTTP+JSON binding `application/a2a+json`
/// SHOULD be used for responses. JSON responses (success and error) carry
/// it when the client accepts it: `Accept` names it, or `Accept` is absent
/// or a wildcard and the request was itself `application/a2a+json`. Any
/// other client gets `application/json`; event streams keep
/// `text/event-stream`. The JSON-RPC binding is always `application/json`
/// (Section 9.1).
#[tokio::test]
async fn json_response_media_type_follows_accept() {
    let content_type = |resp: &axum::response::Response| {
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let send = |accept: Option<&str>, content_type: &str| {
        let mut req = Request::post("/message:send")
            .header("a2a-version", "1.0")
            .header("content-type", content_type);
        if let Some(a) = accept {
            req = req.header("accept", a);
        }
        req.body(Body::from(send_body())).unwrap()
    };
    for (accept, request_type, expected) in [
        (
            Some("application/a2a+json"),
            "application/json",
            "application/a2a+json",
        ),
        (None, "application/a2a+json", "application/a2a+json"),
        (Some("*/*"), "application/a2a+json", "application/a2a+json"),
        (None, "application/json", "application/json"),
        (
            Some("application/json"),
            "application/a2a+json",
            "application/json",
        ),
    ] {
        let resp = build_router(test_state())
            .oneshot(send(accept, request_type))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            content_type(&resp),
            expected,
            "accept={accept:?} content-type={request_type}"
        );
    }

    let get_missing = |accept: Option<&str>, request_type: Option<&str>| {
        let mut req = Request::get("/tasks/nope").header("a2a-version", "1.0");
        if let Some(a) = accept {
            req = req.header("accept", a);
        }
        if let Some(c) = request_type {
            req = req.header("content-type", c);
        }
        req.body(Body::empty()).unwrap()
    };
    for (accept, request_type, expected) in [
        (Some("application/a2a+json"), None, "application/a2a+json"),
        (None, Some("application/a2a+json"), "application/a2a+json"),
        (None, None, "application/json"),
    ] {
        let resp = build_router(test_state())
            .oneshot(get_missing(accept, request_type))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        assert_eq!(
            content_type(&resp),
            expected,
            "error: accept={accept:?} content-type={request_type:?}"
        );
    }

    // A streaming call refused before the stream opens (this test agent
    // does not stream) answers with a JSON error in a type the client
    // listed: the official Go SDK sends this Accept and rejects any other
    // JSON type.
    let req = Request::post("/tasks/nope:subscribe")
        .header("a2a-version", "1.0")
        .header("accept", "text/event-stream, application/json")
        .body(Body::empty())
        .unwrap();
    let resp = build_router(test_state()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(content_type(&resp), "application/json");

    // JSON-RPC is `application/json` whatever the client accepts.
    for accept in [
        "text/event-stream, application/json",
        "application/a2a+json",
    ] {
        let req = Request::post("/jsonrpc")
            .header("a2a-version", "1.0")
            .header("content-type", "application/a2a+json")
            .header("accept", accept)
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"SubscribeToTask","params":{"id":"nope"}}"#,
            ))
            .unwrap();
        let resp = build_router(test_state()).oneshot(req).await.unwrap();
        assert_eq!(content_type(&resp), "application/json", "accept={accept}");
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["code"], -32004, "{body}");
        assert_eq!(
            body["error"]["data"][0]["reason"], "UNSUPPORTED_OPERATION",
            "{body}"
        );
    }
}

/// The well-known agent card is version-independent: it is
/// `application/json` with or without `A2A-Version`, whatever the features.
#[tokio::test]
async fn agent_card_is_plain_json_for_every_client() {
    for version in [None, Some("1.0")] {
        let mut req = Request::get("/.well-known/agent-card.json");
        if let Some(v) = version {
            req = req.header("a2a-version", v);
        }
        let resp = build_router(test_state())
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{version:?}");
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json"),
            "{version:?}"
        );
    }
}

/// With `compat-v03`, a v0.3 request (no `A2A-Version`) keeps the
/// `application/json` responses those clients were written against.
#[cfg(feature = "compat-v03")]
#[tokio::test]
async fn legacy_v03_responses_keep_application_json() {
    let router = build_router(test_state());
    let req = Request::get("/tasks/nope").body(Body::empty()).unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
}
