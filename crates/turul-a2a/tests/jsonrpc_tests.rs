//! JSON-RPC dispatch integration tests.
//!
//! Reuses the same core handler logic as HTTP routes.
//! Tests verify exact method names, response envelopes, and error mappings.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::router::{build_router, AppState};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Message, Task};

struct CompletingExecutor;

#[async_trait::async_trait]
impl AgentExecutor for CompletingExecutor {
    async fn execute(&self, task: &mut Task, _message: &Message) -> Result<(), A2aError> {
        let mut proto = task.as_proto().clone();
        proto.status = Some(turul_a2a_proto::TaskStatus {
            state: turul_a2a_proto::TaskState::Completed.into(),
            message: None,
            timestamp: None,
        });
        *task = Task::try_from(proto).unwrap();
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
                push_notifications: Some(true),
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
    let storage = InMemoryA2aStorage::new();
    AppState {
        executor: Arc::new(CompletingExecutor),
        task_storage: Arc::new(storage.clone()),
        push_storage: Arc::new(storage),
        event_broker: turul_a2a::streaming::TaskEventBroker::new(),
        middleware_stack: std::sync::Arc::new(turul_a2a::middleware::MiddlewareStack::new(vec![])),
    }
}

fn jsonrpc_request(method: &str, params: serde_json::Value, id: i64) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": id,
    })
    .to_string()
}

async fn jsonrpc_call(
    router: axum::Router,
    body: &str,
) -> (u16, serde_json::Value) {
    let req = Request::post("/jsonrpc")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or_default();
    (status, json)
}

// =========================================================
// JSON-RPC framework errors
// =========================================================

#[tokio::test]
async fn invalid_json_returns_parse_error() {
    let router = build_router(test_state());
    let (_, body) = jsonrpc_call(router, "not valid json{{{").await;
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["error"]["code"], -32700);
    assert!(body["id"].is_null());
}

#[tokio::test]
async fn missing_jsonrpc_field_returns_invalid_request() {
    let router = build_router(test_state());
    let req = serde_json::json!({"method": "GetTask", "id": 1}).to_string();
    let (_, body) = jsonrpc_call(router, &req).await;
    assert_eq!(body["error"]["code"], -32600);
}

#[tokio::test]
async fn wrong_jsonrpc_version_returns_invalid_request() {
    let router = build_router(test_state());
    let req = serde_json::json!({"jsonrpc": "1.0", "method": "GetTask", "id": 1}).to_string();
    let (_, body) = jsonrpc_call(router, &req).await;
    assert_eq!(body["error"]["code"], -32600);
}

#[tokio::test]
async fn missing_method_returns_invalid_request() {
    let router = build_router(test_state());
    let req = serde_json::json!({"jsonrpc": "2.0", "id": 1}).to_string();
    let (_, body) = jsonrpc_call(router, &req).await;
    assert_eq!(body["error"]["code"], -32600);
}

#[tokio::test]
async fn unknown_method_returns_method_not_found() {
    let router = build_router(test_state());
    let req = jsonrpc_request("NonExistentMethod", serde_json::json!({}), 1);
    let (_, body) = jsonrpc_call(router, &req).await;
    assert_eq!(body["error"]["code"], -32601);
    assert_eq!(body["id"], 1);
}

// =========================================================
// SendMessage via JSON-RPC — exact method name, response shape
// =========================================================

#[tokio::test]
async fn send_message_returns_send_message_response() {
    let router = build_router(test_state());
    let params = serde_json::json!({
        "message": {
            "messageId": "jrpc-m1",
            "role": "ROLE_USER",
            "parts": [{"text": "hello via jsonrpc"}]
        }
    });
    let req = jsonrpc_request("SendMessage", params, 10);
    let (status, body) = jsonrpc_call(router, &req).await;

    assert_eq!(status, 200);
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 10);
    assert!(body.get("error").is_none(), "should not have error");

    // Result is SendMessageResponse — must have task or message
    let result = &body["result"];
    assert!(
        result.get("task").is_some() || result.get("message").is_some(),
        "SendMessageResponse must have task or message, got: {result}"
    );
}

#[tokio::test]
async fn send_message_with_tenant_in_params() {
    let router = build_router(test_state());
    let params = serde_json::json!({
        "tenant": "acme",
        "message": {
            "messageId": "jrpc-mt",
            "role": "ROLE_USER",
            "parts": [{"text": "tenant message"}]
        }
    });
    let req = jsonrpc_request("SendMessage", params, 11);
    let (_, body) = jsonrpc_call(router, &req).await;
    assert!(body.get("error").is_none());
    assert!(body["result"]["task"]["id"].is_string());
}

// =========================================================
// GetTask via JSON-RPC
// =========================================================

#[tokio::test]
async fn get_task_via_jsonrpc() {
    let state = test_state();

    // Create task via SendMessage
    let router = build_router(state.clone());
    let params = serde_json::json!({
        "message": {
            "messageId": "jrpc-gt",
            "role": "ROLE_USER",
            "parts": [{"text": "create for get"}]
        }
    });
    let (_, send_body) = jsonrpc_call(router, &jsonrpc_request("SendMessage", params, 1)).await;
    let task_id = send_body["result"]["task"]["id"].as_str().unwrap();

    // GetTask
    let router = build_router(state);
    let params = serde_json::json!({"id": task_id});
    let (_, body) = jsonrpc_call(router, &jsonrpc_request("GetTask", params, 2)).await;
    assert_eq!(body["id"], 2);
    assert!(body.get("error").is_none());
    assert_eq!(body["result"]["id"].as_str().unwrap(), task_id);
}

#[tokio::test]
async fn get_task_not_found_returns_a2a_error_with_error_info() {
    let router = build_router(test_state());
    let params = serde_json::json!({"id": "nonexistent"});
    let (_, body) = jsonrpc_call(router, &jsonrpc_request("GetTask", params, 3)).await;

    assert_eq!(body["error"]["code"], -32001); // JSONRPC_TASK_NOT_FOUND
    // Must have ErrorInfo in data
    let data = body["error"]["data"].as_array().unwrap();
    assert!(!data.is_empty());
    assert_eq!(data[0]["@type"], "type.googleapis.com/google.rpc.ErrorInfo");
    assert_eq!(data[0]["reason"], "TASK_NOT_FOUND");
    assert_eq!(data[0]["domain"], "a2a-protocol.org");
}

// =========================================================
// ListTasks via JSON-RPC
// =========================================================

#[tokio::test]
async fn list_tasks_via_jsonrpc() {
    let router = build_router(test_state());
    let params = serde_json::json!({});
    let (_, body) = jsonrpc_call(router, &jsonrpc_request("ListTasks", params, 4)).await;

    assert!(body.get("error").is_none());
    let result = &body["result"];
    assert!(result.get("tasks").is_some());
    assert!(result.get("nextPageToken").is_some());
    assert!(result.get("pageSize").is_some());
    assert!(result.get("totalSize").is_some());
}

// =========================================================
// CancelTask via JSON-RPC — 409 maps to -32002
// =========================================================

#[tokio::test]
async fn cancel_completed_task_returns_not_cancelable_error() {
    let state = test_state();

    // Create and complete task
    let router = build_router(state.clone());
    let params = serde_json::json!({
        "message": {
            "messageId": "jrpc-ct",
            "role": "ROLE_USER",
            "parts": [{"text": "cancel me"}]
        }
    });
    let (_, send_body) = jsonrpc_call(router, &jsonrpc_request("SendMessage", params, 1)).await;
    let task_id = send_body["result"]["task"]["id"].as_str().unwrap();

    // Cancel completed task
    let router = build_router(state);
    let params = serde_json::json!({"id": task_id});
    let (_, body) = jsonrpc_call(router, &jsonrpc_request("CancelTask", params, 5)).await;

    assert_eq!(body["error"]["code"], -32002); // JSONRPC_TASK_NOT_CANCELABLE
    let data = body["error"]["data"].as_array().unwrap();
    assert_eq!(data[0]["reason"], "TASK_NOT_CANCELABLE");
    assert_eq!(data[0]["domain"], "a2a-protocol.org");
}

// =========================================================
// GetExtendedAgentCard via JSON-RPC
// =========================================================

#[tokio::test]
async fn get_extended_agent_card_not_configured() {
    let router = build_router(test_state());
    let params = serde_json::json!({});
    let (_, body) =
        jsonrpc_call(router, &jsonrpc_request("GetExtendedAgentCard", params, 6)).await;

    assert_eq!(body["error"]["code"], -32007); // EXTENDED_AGENT_CARD_NOT_CONFIGURED
    let data = body["error"]["data"].as_array().unwrap();
    assert_eq!(data[0]["reason"], "EXTENDED_AGENT_CARD_NOT_CONFIGURED");
}

// =========================================================
// JSON-RPC response preserves request id
// =========================================================

#[tokio::test]
async fn response_preserves_request_id() {
    let router = build_router(test_state());
    let req = jsonrpc_request("ListTasks", serde_json::json!({}), 42);
    let (_, body) = jsonrpc_call(router, &req).await;
    assert_eq!(body["id"], 42);
}

#[tokio::test]
async fn string_id_preserved() {
    let router = build_router(test_state());
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "ListTasks",
        "params": {},
        "id": "string-id-123"
    })
    .to_string();
    let (_, body) = jsonrpc_call(router, &req).await;
    assert_eq!(body["id"], "string-id-123");
}

// =========================================================
// [P1] Notifications (no id) must not receive a response
// =========================================================

#[tokio::test]
async fn notification_without_id_returns_empty_body() {
    let router = build_router(test_state());
    // JSON-RPC 2.0: request without "id" is a notification — server must not reply
    let req_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "ListTasks",
        "params": {}
    })
    .to_string();
    let req = Request::post("/jsonrpc")
        .header("content-type", "application/json")
        .body(Body::from(req_body))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();

    // Server should return 204 No Content or 200 with empty body
    assert!(
        status == 204 || bytes.is_empty(),
        "Notification must not produce a response body, got status={status} body={}",
        String::from_utf8_lossy(&bytes)
    );
}

#[tokio::test]
async fn notification_error_still_suppressed() {
    let router = build_router(test_state());
    // Notification that would produce an error — still must not reply
    let req_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "NonExistentMethod",
        "params": {}
    })
    .to_string();
    let req = Request::post("/jsonrpc")
        .header("content-type", "application/json")
        .body(Body::from(req_body))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();

    assert!(
        status == 204 || bytes.is_empty(),
        "Notification error must not produce a response body"
    );
}

// =========================================================
// [P2] Non-object params must be rejected as invalid params
// =========================================================

#[tokio::test]
async fn array_params_returns_invalid_params() {
    let router = build_router(test_state());
    let req_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "ListTasks",
        "params": [1, 2, 3],
        "id": 99
    })
    .to_string();
    let (_, body) = jsonrpc_call(router, &req_body).await;
    assert_eq!(body["error"]["code"], -32602, "Array params should be -32602 Invalid params");
}

#[tokio::test]
async fn scalar_params_returns_invalid_params() {
    let router = build_router(test_state());
    let req_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "ListTasks",
        "params": "not an object",
        "id": 100
    })
    .to_string();
    let (_, body) = jsonrpc_call(router, &req_body).await;
    assert_eq!(body["error"]["code"], -32602, "Scalar params should be -32602 Invalid params");
}

#[tokio::test]
async fn null_params_treated_as_empty_object() {
    let router = build_router(test_state());
    // null params should be treated as {} (no params), not rejected
    let req_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "ListTasks",
        "params": null,
        "id": 101
    })
    .to_string();
    let (_, body) = jsonrpc_call(router, &req_body).await;
    assert!(body.get("error").is_none(), "null params should be accepted as empty");
}
