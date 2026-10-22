//! SSE transport integration tests.
//!
//! Tests the actual HTTP response shape from /message:stream and /tasks/{id}:subscribe.
//! Reads the streamed body from tower::ServiceExt, not a full network client.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::router::{AppState, build_router};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Message, Task};

/// Executor that completes the task and produces one artifact.
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
        proto.artifacts.push(turul_a2a_proto::Artifact {
            artifact_id: "art-sse".into(),
            name: "SSE Result".into(),
            description: String::new(),
            parts: vec![turul_a2a_proto::Part {
                content: Some(turul_a2a_proto::part::Content::Text(
                    "streamed result".into(),
                )),
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
        durable_executor_queue: None,
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

/// Stream `body` to a closing state. The caller keeps its router (and
/// so the server state) alive, so the end of the stream is the
/// server's decision, not a dropped broker.
async fn closed_stream(router: &axum::Router, body: &str) -> Vec<serde_json::Value> {
    let req = Request::post("/message:stream")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    common::read_sse(resp.into_body(), Duration::from_secs(5))
        .await
        .closed()
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
        .header("a2a-version", "1.0")
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
async fn message_stream_first_event_is_task_snapshot() {
    let router = build_router(test_state());
    let events = closed_stream(&router, &send_message_body("sse-first", "start")).await;

    // A2A §3.1.2: the stream MUST begin with the Task object.
    let first = &events[0];
    assert!(
        first.get("task").is_some(),
        "First event should be the Task snapshot, got: {first}"
    );
    let task = &first["task"];
    assert!(task.get("id").is_some());
    assert!(task.get("contextId").is_some());
    // The turn has started before the snapshot is taken.
    assert_eq!(task["status"]["state"], "TASK_STATE_WORKING");
}

#[tokio::test]
async fn message_stream_events_are_proto_correct_stream_response() {
    let router = build_router(test_state());
    let events = closed_stream(&router, &send_message_body("sse-shape", "check shape")).await;

    // Every event must be a valid StreamResponse variant: the Task
    // snapshot first, then status or artifact updates.
    assert!(events[0].get("task").is_some(), "first: {}", events[0]);
    for event in &events[1..] {
        let is_status = event.get("statusUpdate").is_some();
        let is_artifact = event.get("artifactUpdate").is_some();
        assert!(
            is_status || is_artifact,
            "Event must be statusUpdate or artifactUpdate, got: {event}"
        );
    }
}

#[tokio::test]
async fn message_stream_closes_after_the_terminal_event() {
    let router = build_router(test_state());
    let events = closed_stream(&router, &send_message_body("sse-term", "complete me")).await;

    // SUBMITTED and WORKING are reflected in the snapshot and not
    // replayed; COMPLETED is the only status update, it is the last
    // event, and the stream has closed after it.
    assert_eq!(common::status_states(&events), ["TASK_STATE_COMPLETED"]);
    assert!(
        events.last().unwrap().get("statusUpdate").is_some(),
        "the terminal status update is the last event, got {events:?}"
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
        .header("a2a-version", "1.0")
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
        .header("a2a-version", "1.0")
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
    // Spec §3.1.6: terminal tasks return UnsupportedOperationError
    let state = test_state();

    // Create and complete a task via normal send
    let router = build_router(state.clone());
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_message_body("sse-sub-term", "complete")))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let task_id = json["task"]["id"].as_str().unwrap();

    // Subscribe to the completed task — should error
    let router = build_router(state);
    let req = Request::get(format!("/tasks/{task_id}:subscribe"))
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        400,
        "Subscribe to terminal task should return 400 (UnsupportedOperationError)"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["error"]["details"][0]["reason"],
        "UNSUPPORTED_OPERATION"
    );
    assert_eq!(json["error"]["details"][0]["domain"], "a2a-protocol.org");
}

#[tokio::test]
async fn subscribe_tenant_scoped_missing_returns_404() {
    let state = test_state();

    // Create task under "acme"
    let router = build_router(state.clone());
    let req = Request::post("/acme/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_message_body("sse-sub-ts", "tenant task")))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let task_id = json["task"]["id"].as_str().unwrap();

    // Subscribe under wrong tenant — should 404
    let router = build_router(state);
    let req = Request::get(format!("/other/tasks/{task_id}:subscribe"))
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        404,
        "Subscribe under wrong tenant should 404"
    );
}

// =========================================================
// GET /tasks/{id}:subscribe — positive happy path
// =========================================================

#[tokio::test]
async fn subscribe_non_terminal_task_returns_sse_opening_with_task_snapshot() {
    let state = test_state();

    // Create task with events via atomic store (D3 model)
    let task = turul_a2a_types::Task::new(
        "sub-happy-1",
        turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Submitted),
    )
    .with_context_id("ctx-sub-happy");

    let submitted_event = turul_a2a::streaming::StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: "sub-happy-1".to_string(),
            context_id: "ctx-sub-happy".to_string(),
            status: serde_json::json!({"state": "TASK_STATE_SUBMITTED"}),
        },
    };

    state
        .atomic_store
        .create_task_with_events("", "anonymous", task, vec![submitted_event])
        .await
        .unwrap();

    // Advance to Working with event
    let working_event = turul_a2a::streaming::StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: "sub-happy-1".to_string(),
            context_id: "ctx-sub-happy".to_string(),
            status: serde_json::json!({"state": "TASK_STATE_WORKING"}),
        },
    };

    state
        .atomic_store
        .update_task_status_with_events(
            "",
            "sub-happy-1",
            "anonymous",
            turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Working),
            vec![working_event],
        )
        .await
        .unwrap();

    // Subscribe — should return 200 + text/event-stream opening with
    // the snapshot. The router is kept alive so the server state is.
    let router = build_router(state);
    let req = Request::get("/tasks/sub-happy-1:subscribe")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();

    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        200,
        "Subscribe to non-terminal task should return 200"
    );

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/event-stream"),
        "Subscribe should return text/event-stream, got: {content_type}"
    );

    // Task snapshot only (spec §3.1.6): it already reflects the stored
    // SUBMITTED and WORKING events, so neither is replayed. The task
    // stays WORKING, so the stream stays open.
    let events = common::read_sse(resp.into_body(), Duration::from_secs(1))
        .await
        .still_open();
    assert_eq!(events.len(), 1, "snapshot only, got: {events:?}");

    // First event MUST be a Task object (spec §3.1.6)
    let first = &events[0];
    assert!(
        first.get("task").is_some(),
        "First subscribe event MUST be a Task object (spec §3.1.6), got: {first}"
    );
    assert_eq!(
        first["task"]["id"].as_str().unwrap_or(""),
        "sub-happy-1",
        "Task snapshot should contain the task ID"
    );
}
