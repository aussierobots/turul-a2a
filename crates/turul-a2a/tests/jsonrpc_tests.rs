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
use turul_a2a::router::{AppState, build_router};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Message, Task};

struct CompletingExecutor;

#[async_trait::async_trait]
impl AgentExecutor for CompletingExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _message: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
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
    let s = InMemoryA2aStorage::new();
    AppState {
        executor: Arc::new(CompletingExecutor),
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
        push_dispatcher: None,
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

async fn jsonrpc_call(router: axum::Router, body: &str) -> (u16, serde_json::Value) {
    let req = Request::post("/jsonrpc")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
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
    let data = &body["error"]["data"];
    assert!(data.is_object(), "JSON-RPC error data should be an object");
    assert_eq!(data["@type"], "type.googleapis.com/google.rpc.ErrorInfo");
    assert_eq!(data["reason"], "TASK_NOT_FOUND");
    assert_eq!(data["domain"], "a2a-protocol.org");
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
    let data = &body["error"]["data"];
    assert!(data.is_object(), "JSON-RPC error data should be an object");
    assert_eq!(data["reason"], "TASK_NOT_CANCELABLE");
    assert_eq!(data["domain"], "a2a-protocol.org");
}

// =========================================================
// GetExtendedAgentCard via JSON-RPC
// =========================================================

#[tokio::test]
async fn get_extended_agent_card_not_configured() {
    let router = build_router(test_state());
    let params = serde_json::json!({});
    let (_, body) = jsonrpc_call(router, &jsonrpc_request("GetExtendedAgentCard", params, 6)).await;

    assert_eq!(body["error"]["code"], -32007); // EXTENDED_AGENT_CARD_NOT_CONFIGURED
    let data = &body["error"]["data"];
    assert!(data.is_object(), "JSON-RPC error data should be an object");
    assert_eq!(data["reason"], "EXTENDED_AGENT_CARD_NOT_CONFIGURED");
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
        .header("a2a-version", "1.0")
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
        .header("a2a-version", "1.0")
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
    assert_eq!(
        body["error"]["code"], -32602,
        "Array params should be -32602 Invalid params"
    );
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
    assert_eq!(
        body["error"]["code"], -32602,
        "Scalar params should be -32602 Invalid params"
    );
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
    assert!(
        body.get("error").is_none(),
        "null params should be accepted as empty"
    );
}

// =========================================================
// JSON-RPC Streaming Tests
// =========================================================

/// Collect SSE events from a streaming response body with a timeout.
async fn collect_jsonrpc_sse(body: Body, timeout: std::time::Duration) -> Vec<serde_json::Value> {
    let mut body = body;
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, http_body_util::BodyExt::frame(&mut body)).await {
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    buf.push_str(&String::from_utf8_lossy(data));
                }
            }
            _ => break,
        }
    }

    // Parse SSE data lines as JSON
    let mut events = Vec::new();
    for chunk in buf.split("\n\n") {
        for line in chunk.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(data.trim()) {
                    events.push(json);
                }
            }
        }
    }
    events
}

#[tokio::test]
async fn jsonrpc_send_streaming_message_returns_sse_with_envelopes() {
    let state = test_state();
    let router = build_router(state);

    let req_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "SendStreamingMessage",
        "params": {
            "message": {
                "messageId": "jrpc-stream-1",
                "role": "ROLE_USER",
                "parts": [{"text": "stream test"}]
            }
        },
        "id": 99
    })
    .to_string();

    let req = Request::post("/jsonrpc")
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("a2a-version", "1.0")
        .body(Body::from(req_body))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/event-stream"),
        "JSON-RPC streaming should return text/event-stream, got: {content_type}"
    );

    let events = collect_jsonrpc_sse(resp.into_body(), std::time::Duration::from_secs(3)).await;
    assert!(!events.is_empty(), "Should receive streaming events");

    // Every event should be a JSON-RPC envelope with the original request id
    for (i, event) in events.iter().enumerate() {
        assert_eq!(
            event["jsonrpc"], "2.0",
            "Event {i} should have jsonrpc field"
        );
        assert_eq!(
            event["id"], 99,
            "Event {i} should carry original request id"
        );
        assert!(
            event.get("result").is_some(),
            "Event {i} should have result field"
        );
    }

    // Should see terminal event (COMPLETED)
    let has_completed = events.iter().any(|e| {
        e["result"]
            .get("statusUpdate")
            .and_then(|su| su.get("status"))
            .and_then(|s| s.get("state"))
            .and_then(|s| s.as_str())
            .is_some_and(|s| s == "TASK_STATE_COMPLETED")
    });
    assert!(has_completed, "Stream should include COMPLETED event");
}

#[tokio::test]
async fn jsonrpc_subscribe_to_task_returns_task_snapshot_first() {
    let state = test_state();

    // Create a non-terminal task with events via atomic store
    let task = turul_a2a_types::Task::new(
        "jrpc-sub-1",
        turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Submitted),
    )
    .with_context_id("ctx-jrpc-sub");

    let submitted_event = turul_a2a::streaming::StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: "jrpc-sub-1".to_string(),
            context_id: "ctx-jrpc-sub".to_string(),
            status: serde_json::json!({"state": "TASK_STATE_SUBMITTED"}),
        },
    };

    state
        .atomic_store
        .create_task_with_events("", "anonymous", task, vec![submitted_event])
        .await
        .unwrap();

    let working_event = turul_a2a::streaming::StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: "jrpc-sub-1".to_string(),
            context_id: "ctx-jrpc-sub".to_string(),
            status: serde_json::json!({"state": "TASK_STATE_WORKING"}),
        },
    };

    state
        .atomic_store
        .update_task_status_with_events(
            "",
            "jrpc-sub-1",
            "anonymous",
            turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Working),
            vec![working_event],
        )
        .await
        .unwrap();

    // Subscribe via JSON-RPC
    let router = build_router(state);
    let req_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "SubscribeToTask",
        "params": {"id": "jrpc-sub-1"},
        "id": 77
    })
    .to_string();

    let req = Request::post("/jsonrpc")
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("a2a-version", "1.0")
        .body(Body::from(req_body))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    let events = collect_jsonrpc_sse(resp.into_body(), std::time::Duration::from_secs(3)).await;
    assert!(
        events.len() >= 3,
        "Should get Task snapshot + 2 events, got {}",
        events.len()
    );

    // First event should be Task snapshot in JSON-RPC envelope
    let first = &events[0];
    assert_eq!(first["jsonrpc"], "2.0");
    assert_eq!(first["id"], 77);
    assert!(
        first["result"].get("task").is_some(),
        "First event should be Task snapshot, got: {}",
        first["result"]
    );
}

#[tokio::test]
async fn jsonrpc_subscribe_terminal_task_returns_error() {
    let state = test_state();

    // Create and complete a task
    let task = turul_a2a_types::Task::new(
        "jrpc-term-1",
        turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Submitted),
    )
    .with_context_id("ctx-jrpc-term");

    state
        .atomic_store
        .create_task_with_events("", "anonymous", task, vec![])
        .await
        .unwrap();

    // Move to completed
    let mut task = state
        .task_storage
        .get_task("", "jrpc-term-1", "anonymous", None)
        .await
        .unwrap()
        .unwrap();
    task.set_status(turul_a2a_types::TaskStatus::new(
        turul_a2a_types::TaskState::Working,
    ));
    task.complete();
    state
        .atomic_store
        .update_task_with_events("", "anonymous", task, vec![])
        .await
        .unwrap();

    // Subscribe to terminal task via JSON-RPC
    let router = build_router(state);
    let req_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "SubscribeToTask",
        "params": {"id": "jrpc-term-1"},
        "id": 88
    })
    .to_string();

    let (_, body) = jsonrpc_call(router, &req_body).await;

    // Should get JSON-RPC error (UnsupportedOperation), not an SSE stream
    assert!(
        body.get("error").is_some(),
        "Terminal subscribe should return error: {body}"
    );
}

#[tokio::test]
async fn jsonrpc_subscribe_with_last_event_id_resumes() {
    let state = test_state();

    // Create a non-terminal task with 2 events
    let task = turul_a2a_types::Task::new(
        "jrpc-lei-1",
        turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Submitted),
    )
    .with_context_id("ctx-jrpc-lei");

    state
        .atomic_store
        .create_task_with_events(
            "",
            "anonymous",
            task,
            vec![turul_a2a::streaming::StreamEvent::StatusUpdate {
                status_update: turul_a2a::streaming::StatusUpdatePayload {
                    task_id: "jrpc-lei-1".to_string(),
                    context_id: "ctx-jrpc-lei".to_string(),
                    status: serde_json::json!({"state": "TASK_STATE_SUBMITTED"}),
                },
            }],
        )
        .await
        .unwrap();

    state
        .atomic_store
        .update_task_status_with_events(
            "",
            "jrpc-lei-1",
            "anonymous",
            turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Working),
            vec![turul_a2a::streaming::StreamEvent::StatusUpdate {
                status_update: turul_a2a::streaming::StatusUpdatePayload {
                    task_id: "jrpc-lei-1".to_string(),
                    context_id: "ctx-jrpc-lei".to_string(),
                    status: serde_json::json!({"state": "TASK_STATE_WORKING"}),
                },
            }],
        )
        .await
        .unwrap();

    // Subscribe with Last-Event-ID to skip first event (Turul extension)
    let router = build_router(state);
    let req_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "SubscribeToTask",
        "params": {"id": "jrpc-lei-1"},
        "id": 55
    })
    .to_string();

    let req = Request::post("/jsonrpc")
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("a2a-version", "1.0")
        .header("Last-Event-ID", "jrpc-lei-1:1") // skip past sequence 1
        .body(Body::from(req_body))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    let events = collect_jsonrpc_sse(resp.into_body(), std::time::Duration::from_secs(3)).await;

    // Should get only event at sequence 2 (skipped sequence 1)
    // No Task snapshot on reconnect (Last-Event-ID > 0)
    assert_eq!(
        events.len(),
        1,
        "Reconnect should return only events after sequence 1, got {}",
        events.len()
    );
    assert_eq!(events[0]["id"], 55);
    // Should NOT have a Task snapshot as first event (reconnecting)
    assert!(
        events[0]["result"].get("task").is_none(),
        "Reconnect should not include Task snapshot"
    );
}
