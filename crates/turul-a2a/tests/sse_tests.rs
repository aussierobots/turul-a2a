//! SSE transport integration tests.
//!
//! Tests the actual HTTP response shape from /message:stream and /tasks/{id}:subscribe.
//! Reads the streamed body from tower::ServiceExt, not a full network client.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::router::{build_router, AppState};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Message, Task};

/// Executor that completes the task and produces one artifact.
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
        proto.artifacts.push(turul_a2a_proto::Artifact {
            artifact_id: "art-sse".into(),
            name: "SSE Result".into(),
            description: String::new(),
            parts: vec![turul_a2a_proto::Part {
                content: Some(turul_a2a_proto::part::Content::Text("streamed result".into())),
                metadata: None,
                filename: String::new(),
                media_type: String::new(),
            }],
            metadata: None,
            extensions: vec![],
        });
        *task = Task::try_from(proto).unwrap();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            name: "SSE Test Agent".into(),
            description: "Agent for SSE tests".into(),
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
                streaming: Some(true),
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
    let storage = InMemoryA2aStorage::new();
    AppState {
        executor: Arc::new(CompletingExecutor),
        task_storage: Arc::new(storage.clone()),
        push_storage: Arc::new(storage),
        event_broker: turul_a2a::streaming::TaskEventBroker::new(),
    }
}

fn send_message_body(id: &str, text: &str) -> String {
    serde_json::json!({
        "message": {
            "messageId": id,
            "role": "ROLE_USER",
            "parts": [{"text": text}]
        }
    })
    .to_string()
}

/// Collect SSE events from a response body with a timeout.
/// Returns the raw `data:` payloads as parsed JSON values.
async fn collect_sse_events(
    body: Body,
    timeout: Duration,
) -> Vec<serde_json::Value> {
    let collected = tokio::time::timeout(timeout, async {
        let bytes = body.collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&bytes);
        parse_sse_events(&text)
    })
    .await;

    match collected {
        Ok(events) => events,
        Err(_) => vec![], // Timeout — stream didn't close
    }
}

/// Parse SSE text format into JSON data payloads.
/// SSE format: "data: {json}\n\n" per event, with optional "event:" and "id:" lines.
fn parse_sse_events(text: &str) -> Vec<serde_json::Value> {
    let mut events = Vec::new();
    for chunk in text.split("\n\n") {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        for line in chunk.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                    events.push(json);
                }
            }
        }
    }
    events
}

// =========================================================
// POST /message:stream — content type and event shape
// =========================================================

#[tokio::test]
async fn message_stream_returns_text_event_stream() {
    let state = test_state();
    let router = build_router(state);
    let req = Request::post("/message:stream")
        .header("content-type", "application/json")
        .body(Body::from(send_message_body("sse-ct", "hello")))
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
        "Expected text/event-stream, got: {content_type}"
    );
}

#[tokio::test]
async fn message_stream_first_event_is_status_snapshot() {
    let state = test_state();
    let router = build_router(state);
    let req = Request::post("/message:stream")
        .header("content-type", "application/json")
        .body(Body::from(send_message_body("sse-first", "start")))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    let events = collect_sse_events(resp.into_body(), Duration::from_secs(2)).await;

    assert!(!events.is_empty(), "Should receive at least one event");

    // First event should be a statusUpdate
    let first = &events[0];
    assert!(
        first.get("statusUpdate").is_some(),
        "First event should be statusUpdate, got: {first}"
    );
    let su = &first["statusUpdate"];
    assert!(su.get("taskId").is_some());
    assert!(su.get("contextId").is_some());
    assert!(su.get("status").is_some());
}

#[tokio::test]
async fn message_stream_events_are_proto_correct_stream_response() {
    let state = test_state();
    let router = build_router(state);
    let req = Request::post("/message:stream")
        .header("content-type", "application/json")
        .body(Body::from(send_message_body("sse-shape", "check shape")))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    let events = collect_sse_events(resp.into_body(), Duration::from_secs(2)).await;

    // Every event must be a valid StreamResponse variant (statusUpdate or artifactUpdate)
    for event in &events {
        let is_status = event.get("statusUpdate").is_some();
        let is_artifact = event.get("artifactUpdate").is_some();
        assert!(
            is_status || is_artifact,
            "Event must be statusUpdate or artifactUpdate, got: {event}"
        );
    }
}

#[tokio::test]
async fn message_stream_events_in_order() {
    let state = test_state();
    let router = build_router(state);
    let req = Request::post("/message:stream")
        .header("content-type", "application/json")
        .body(Body::from(send_message_body("sse-order", "ordering")))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    let events = collect_sse_events(resp.into_body(), Duration::from_secs(2)).await;

    // Collect status states in order
    let states: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("statusUpdate"))
        .filter_map(|su| su.get("status"))
        .filter_map(|s| s.get("state"))
        .filter_map(|s| s.as_str())
        .collect();

    // Should see progression: SUBMITTED -> WORKING -> COMPLETED (or subset)
    // The first state should be early in the lifecycle
    if states.len() >= 2 {
        // Verify no regression: later states should not be "earlier" than previous
        let state_order = |s: &str| -> i32 {
            match s {
                "TASK_STATE_SUBMITTED" => 0,
                "TASK_STATE_WORKING" => 1,
                "TASK_STATE_COMPLETED" => 2,
                _ => 99,
            }
        };
        for window in states.windows(2) {
            assert!(
                state_order(window[0]) <= state_order(window[1]),
                "Events out of order: {} before {}",
                window[0],
                window[1]
            );
        }
    }
}

#[tokio::test]
async fn message_stream_includes_terminal_event() {
    let state = test_state();
    let router = build_router(state);
    let req = Request::post("/message:stream")
        .header("content-type", "application/json")
        .body(Body::from(send_message_body("sse-term", "complete me")))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    let events = collect_sse_events(resp.into_body(), Duration::from_secs(2)).await;

    // Should have a terminal status event
    let terminal_states: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("statusUpdate"))
        .filter_map(|su| su.get("status"))
        .filter_map(|s| s.get("state"))
        .filter_map(|s| s.as_str())
        .filter(|s| {
            matches!(
                *s,
                "TASK_STATE_COMPLETED"
                    | "TASK_STATE_FAILED"
                    | "TASK_STATE_CANCELED"
                    | "TASK_STATE_REJECTED"
            )
        })
        .collect();

    assert!(
        !terminal_states.is_empty(),
        "Stream should include a terminal status event"
    );
}

// =========================================================
// POST /message:stream — tenant scoping
// =========================================================

#[tokio::test]
async fn message_stream_tenant_prefixed_works() {
    let state = test_state();
    let router = build_router(state);
    let req = Request::post("/acme/message:stream")
        .header("content-type", "application/json")
        .body(Body::from(send_message_body("sse-tenant", "tenant stream")))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(content_type.contains("text/event-stream"));
}

// =========================================================
// GET /tasks/{id}:subscribe — existing task
// =========================================================

#[tokio::test]
async fn subscribe_missing_task_returns_404() {
    let state = test_state();
    let router = build_router(state);
    let req = Request::get("/tasks/nonexistent-task:subscribe")
        .body(Body::empty())
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 404);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let details = json["error"]["details"].as_array().unwrap();
    assert_eq!(details[0]["reason"], "TASK_NOT_FOUND");
}

#[tokio::test]
async fn subscribe_terminal_task_returns_error() {
    let state = test_state();

    // Create and complete a task via normal send
    let router = build_router(state.clone());
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .body(Body::from(send_message_body("sse-sub-term", "complete")))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let task_id = json["task"]["id"].as_str().unwrap();

    // Subscribe to the completed task — should error
    let router = build_router(state);
    let req = Request::get(&format!("/tasks/{task_id}:subscribe"))
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    // Should be an error, not an SSE stream
    assert_ne!(
        resp.status(),
        200,
        "Subscribe to terminal task should not return 200"
    );
}

#[tokio::test]
async fn subscribe_tenant_scoped_missing_returns_404() {
    let state = test_state();

    // Create task under "acme"
    let router = build_router(state.clone());
    let req = Request::post("/acme/message:send")
        .header("content-type", "application/json")
        .body(Body::from(send_message_body("sse-sub-ts", "tenant task")))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let task_id = json["task"]["id"].as_str().unwrap();

    // Subscribe under wrong tenant — should 404
    let router = build_router(state);
    let req = Request::get(&format!("/other/tasks/{task_id}:subscribe"))
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 404, "Subscribe under wrong tenant should 404");
}

// =========================================================
// GET /tasks/{id}:subscribe — positive happy path
// =========================================================

#[tokio::test]
async fn subscribe_non_terminal_task_returns_sse_with_status_snapshot() {
    let state = test_state();

    // Create a task directly in storage in WORKING state (non-terminal)
    let task = turul_a2a_types::Task::new(
        "sub-happy-1",
        turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Working),
    )
    .with_context_id("ctx-sub-happy");

    state
        .task_storage
        .create_task("", "anonymous", task)
        .await
        .unwrap();

    // Advance to Working (it was created as Submitted internally, need to match)
    // Actually Task::new creates with the given status directly, but storage
    // might validate. Let me create as Submitted then advance.
    // Re-create properly:
    let task2 = turul_a2a_types::Task::new(
        "sub-happy-2",
        turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Submitted),
    )
    .with_context_id("ctx-sub-happy-2");

    state
        .task_storage
        .create_task("", "anonymous", task2)
        .await
        .unwrap();

    // Move to Working (non-terminal)
    state
        .task_storage
        .update_task_status(
            "",
            "sub-happy-2",
            "anonymous",
            turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Working),
        )
        .await
        .unwrap();

    // Subscribe — should return 200 + text/event-stream
    let router = build_router(state);
    let req = Request::get("/tasks/sub-happy-2:subscribe")
        .body(Body::empty())
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200, "Subscribe to non-terminal task should return 200");

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/event-stream"),
        "Subscribe should return text/event-stream, got: {content_type}"
    );

    // Read the first event — should be a status snapshot
    let events = collect_sse_events(resp.into_body(), Duration::from_secs(2)).await;
    assert!(
        !events.is_empty(),
        "Subscribe should emit at least one event (current status snapshot)"
    );

    let first = &events[0];
    assert!(
        first.get("statusUpdate").is_some(),
        "First subscribe event should be statusUpdate, got: {first}"
    );
    let su = &first["statusUpdate"];
    assert_eq!(su["taskId"], "sub-happy-2");
    assert_eq!(su["contextId"], "ctx-sub-happy-2");
    assert!(
        su["status"]["state"]
            .as_str()
            .unwrap_or("")
            .contains("WORKING"),
        "Status snapshot should reflect current WORKING state"
    );
}
