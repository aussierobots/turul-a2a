//! D3 durable streaming tests.
//!
//! Verifies the core D3 guarantees under the upstream spec contract:
//! - Subscribe first event is Task object (spec §3.1.6)
//! - Terminal tasks return UnsupportedOperationError (spec §3.1.6)
//! - Non-terminal subscribe replays stored events from durable store
//! - Cross-instance subscriber/producer via shared store
//! - No correctness dependence on same-instance broker delivery

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use http::Request;
use http_body_util::BodyExt as _;
use tower::ServiceExt;

use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::middleware::MiddlewareStack;
use turul_a2a::router::{AppState, build_router};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a::streaming::{self, StreamEvent, TaskEventBroker};
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};

// =========================================================
// Test executor — completes immediately
// =========================================================

struct CompletingExecutor;

#[async_trait::async_trait]
impl AgentExecutor for CompletingExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
        task.push_text_artifact("d3-art", "Result", "d3 result");
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("D3 Test Agent", "1.0.0")
            .description("Tests D3 streaming")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build()
            .unwrap()
    }
}

// =========================================================
// Helpers
// =========================================================

fn single_instance_state() -> AppState {
    let s = InMemoryA2aStorage::new();
    AppState {
        executor: Arc::new(CompletingExecutor),
        task_storage: Arc::new(s.clone()),
        push_storage: Arc::new(s.clone()),
        event_store: Arc::new(s.clone()),
        atomic_store: Arc::new(s),
        event_broker: TaskEventBroker::new(),
        middleware_stack: Arc::new(MiddlewareStack::new(vec![])),
        runtime_config: turul_a2a::server::RuntimeConfig::default(),
        in_flight: std::sync::Arc::new(turul_a2a::server::in_flight::InFlightRegistry::new()),
        cancellation_supervisor: std::sync::Arc::new(turul_a2a::storage::InMemoryA2aStorage::new()),
        push_delivery_store: None,
        push_dispatcher: None,
        durable_executor_queue: None,
    }
}

/// Two instances sharing storage but with SEPARATE brokers.
fn two_instances() -> (AppState, AppState) {
    let s = InMemoryA2aStorage::new();
    let make = |s: &InMemoryA2aStorage| AppState {
        executor: Arc::new(CompletingExecutor),
        task_storage: Arc::new(s.clone()),
        push_storage: Arc::new(s.clone()),
        event_store: Arc::new(s.clone()),
        atomic_store: Arc::new(s.clone()),
        event_broker: TaskEventBroker::new(),
        middleware_stack: Arc::new(MiddlewareStack::new(vec![])),
        runtime_config: turul_a2a::server::RuntimeConfig::default(),
        in_flight: std::sync::Arc::new(turul_a2a::server::in_flight::InFlightRegistry::new()),
        cancellation_supervisor: std::sync::Arc::new(turul_a2a::storage::InMemoryA2aStorage::new()),
        push_delivery_store: None,
        push_dispatcher: None,
        durable_executor_queue: None,
    };
    (make(&s), make(&s))
}

fn send_body(id: &str) -> String {
    serde_json::json!({"message":{"messageId":id,"role":"ROLE_USER","parts":[{"text":"hello"}]}})
        .to_string()
}

/// Parse SSE events with IDs from raw text.
fn parse_sse_events(text: &str) -> Vec<(Option<String>, serde_json::Value)> {
    let mut events = Vec::new();
    for chunk in text.split("\n\n") {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let mut id = None;
        let mut data = None;
        for line in chunk.lines() {
            if let Some(v) = line.strip_prefix("id:") {
                id = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("data:")
                && let Ok(j) = serde_json::from_str::<serde_json::Value>(v.trim())
            {
                data = Some(j);
            }
        }
        if let Some(d) = data {
            events.push((id, d));
        }
    }
    events
}

async fn collect_sse(body: Body, timeout: Duration) -> Vec<(Option<String>, serde_json::Value)> {
    let mut body = body;
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    buf.push_str(&String::from_utf8_lossy(data));
                }
            }
            _ => break, // timeout, error, or stream ended
        }
    }

    parse_sse_events(&buf)
}

/// Create a non-terminal task with stored events via atomic store.
/// Returns (task_id, number_of_events_stored).
async fn create_non_terminal_task(state: &AppState, task_id: &str) -> usize {
    let task = Task::new(task_id, TaskStatus::new(TaskState::Submitted))
        .with_context_id(format!("ctx-{task_id}"));

    let submitted = StreamEvent::StatusUpdate {
        status_update: streaming::StatusUpdatePayload {
            task_id: task_id.to_string(),
            context_id: format!("ctx-{task_id}"),
            status: serde_json::json!({"state": "TASK_STATE_SUBMITTED"}),
        },
    };
    state
        .atomic_store
        .create_task_with_events("", "anonymous", task, vec![submitted])
        .await
        .unwrap();

    let working = StreamEvent::StatusUpdate {
        status_update: streaming::StatusUpdatePayload {
            task_id: task_id.to_string(),
            context_id: format!("ctx-{task_id}"),
            status: serde_json::json!({"state": "TASK_STATE_WORKING"}),
        },
    };
    state
        .atomic_store
        .update_task_status_with_events(
            "",
            task_id,
            "anonymous",
            TaskStatus::new(TaskState::Working),
            vec![working],
        )
        .await
        .unwrap();

    2 // SUBMITTED + WORKING
}

// =========================================================
// D3-01: Subscribe first event is Task object (spec §3.1.6)
// =========================================================

#[tokio::test]
async fn d3_subscribe_first_event_is_task_object() {
    let state = single_instance_state();
    create_non_terminal_task(&state, "d3-first-1").await;

    let router = build_router(state);
    let req = Request::get("/tasks/d3-first-1:subscribe")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    let events = collect_sse(resp.into_body(), Duration::from_secs(3)).await;
    assert!(!events.is_empty(), "Subscribe should emit events");

    // First event MUST be a Task object (spec §3.1.6)
    let (_, first) = &events[0];
    assert!(
        first.get("task").is_some(),
        "First event MUST be Task object, got: {first}"
    );
    assert_eq!(first["task"]["id"], "d3-first-1");

    // Subsequent events are stored StreamEvents from the durable store
    if events.len() > 1 {
        let (_, second) = &events[1];
        assert!(
            second.get("statusUpdate").is_some(),
            "Events after Task snapshot should be stream events, got: {second}"
        );
    }
}

// =========================================================
// D3-02: Terminal tasks return UnsupportedOperationError
// =========================================================

#[tokio::test]
async fn d3_terminal_task_returns_error() {
    let state = single_instance_state();

    // Create and complete a task
    create_non_terminal_task(&state, "d3-term-1").await;
    let mut task = state
        .task_storage
        .get_task("", "d3-term-1", "anonymous", None)
        .await
        .unwrap()
        .unwrap();
    task.complete();
    state
        .atomic_store
        .update_task_with_events(
            "",
            "anonymous",
            task,
            vec![StreamEvent::StatusUpdate {
                status_update: streaming::StatusUpdatePayload {
                    task_id: "d3-term-1".to_string(),
                    context_id: "ctx-d3-term-1".to_string(),
                    status: serde_json::json!({"state": "TASK_STATE_COMPLETED"}),
                },
            }],
        )
        .await
        .unwrap();

    // Subscribe to terminal task → UnsupportedOperationError
    let router = build_router(state);
    let req = Request::get("/tasks/d3-term-1:subscribe")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        400,
        "Terminal subscribe should return 400 (UnsupportedOperationError)"
    );
}

/// After `delay`, commit INPUT_REQUIRED for `task_id` straight to the
/// store without notifying any broker, so only the store poll can
/// deliver it. INPUT_REQUIRED also closes the stream, which ends the
/// collection.
fn commit_input_required_later(state: &AppState, task_id: &str, delay: Duration) {
    let store = state.atomic_store.clone();
    let task_id = task_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let event = StreamEvent::StatusUpdate {
            status_update: streaming::StatusUpdatePayload {
                task_id: task_id.clone(),
                context_id: format!("ctx-{task_id}"),
                status: serde_json::json!({"state": "TASK_STATE_INPUT_REQUIRED"}),
            },
        };
        store
            .update_task_status_with_events(
                "",
                &task_id,
                "anonymous",
                TaskStatus::new(TaskState::InputRequired),
                vec![event],
            )
            .await
            .unwrap();
    });
}

// =========================================================
// D3-05: Cross-instance subscriber/producer (shared store)
// =========================================================

#[tokio::test]
async fn d3_cross_instance_subscriber_producer() {
    let (state_a, state_b) = two_instances();
    create_non_terminal_task(&state_a, "d3-cross-1").await;

    // Subscribe on instance B; instance A commits the next event. B's
    // broker never hears about it, so only the shared-store poll can
    // deliver it.
    // Keep B's state (and its broker) alive for the stream's lifetime,
    // as a running server would.
    let router_b = build_router(state_b.clone());
    let req = Request::get("/tasks/d3-cross-1:subscribe")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = router_b.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    commit_input_required_later(&state_a, "d3-cross-1", Duration::from_millis(300));

    let events = collect_sse(resp.into_body(), Duration::from_secs(5)).await;
    assert_eq!(events.len(), 2, "snapshot + A's event, got {events:?}");
    assert_eq!(events[0].1["task"]["id"], "d3-cross-1");
    assert_eq!(
        events[1].1["statusUpdate"]["status"]["state"],
        "TASK_STATE_INPUT_REQUIRED"
    );
}

// =========================================================
// D3-06: No correctness dependence on broker delivery
// =========================================================

#[tokio::test]
async fn d3_no_broker_correctness_dependency() {
    let state = single_instance_state();
    create_non_terminal_task(&state, "d3-nobroker-1").await;

    let router = build_router(state.clone());
    let req = Request::get("/tasks/d3-nobroker-1:subscribe")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    // Committed with no broker notification: the poll fallback must
    // deliver it.
    commit_input_required_later(&state, "d3-nobroker-1", Duration::from_millis(300));

    let events = collect_sse(resp.into_body(), Duration::from_secs(5)).await;
    assert_eq!(events.len(), 2, "snapshot + polled event, got {events:?}");
    assert_eq!(
        events[1].1["statusUpdate"]["status"]["state"],
        "TASK_STATE_INPUT_REQUIRED"
    );
}

// =========================================================
// D3-07: Streaming send delivers through the durable store
// =========================================================

#[tokio::test]
async fn d3_streaming_send_reaches_terminal_through_the_store() {
    let state = single_instance_state();

    let router = build_router(state);
    let req = Request::post("/message:stream")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_body("d3-stream-1")))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    let events = collect_sse(resp.into_body(), Duration::from_secs(3)).await;
    assert!(!events.is_empty(), "Streaming send should produce events");

    assert!(
        events[0].1.get("task").is_some(),
        "first item is the Task snapshot"
    );

    // Should include terminal event
    let has_terminal = events.iter().any(|(_, data)| {
        data.get("statusUpdate")
            .and_then(|su| su.get("status"))
            .and_then(|s| s.get("state"))
            .and_then(|s| s.as_str())
            .is_some_and(|s| s == "TASK_STATE_COMPLETED")
    });
    assert!(
        has_terminal,
        "Stream should include terminal COMPLETED event"
    );
}
