//! Durable streaming tests.
//!
//! The durable event store, not the in-process broker, is the source of
//! every streamed event:
//! - subscribe opens with the Task snapshot (spec §3.1.6), and events
//!   the snapshot already reflects are not sent again;
//! - a subscriber on one instance receives events committed on another
//!   through the shared store;
//! - delivery does not depend on a same-instance broker wake-up.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use http::Request;
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

/// Create a WORKING task with its SUBMITTED and WORKING events stored
/// through the atomic store.
async fn create_non_terminal_task(state: &AppState, task_id: &str) {
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
}

// =========================================================
// D3-01: Subscribe first event is Task object (spec §3.1.6)
// =========================================================

#[tokio::test]
async fn d3_subscribe_first_event_is_task_object() {
    let state = single_instance_state();
    create_non_terminal_task(&state, "d3-first-1").await;

    // The router (and so the server state) stays alive for the
    // stream's lifetime, as in a running server.
    let router = build_router(state);
    let req = Request::get("/tasks/d3-first-1:subscribe")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    // The snapshot already reflects the stored SUBMITTED and WORKING
    // events, so it is the only item; the task stays WORKING, so the
    // stream stays open.
    let events = common::read_sse(resp.into_body(), Duration::from_secs(1))
        .await
        .still_open();
    assert_eq!(events.len(), 1, "snapshot only, got {events:?}");
    assert_eq!(events[0]["task"]["id"], "d3-first-1");
    assert_eq!(events[0]["task"]["status"]["state"], "TASK_STATE_WORKING");
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

    let events = common::read_sse(resp.into_body(), Duration::from_secs(5))
        .await
        .closed();
    assert_eq!(events.len(), 2, "snapshot + A's event, got {events:?}");
    assert_eq!(events[0]["task"]["id"], "d3-cross-1");
    assert_eq!(
        events[1]["statusUpdate"]["status"]["state"],
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

    let events = common::read_sse(resp.into_body(), Duration::from_secs(5))
        .await
        .closed();
    assert_eq!(events.len(), 2, "snapshot + polled event, got {events:?}");
    assert_eq!(
        events[1]["statusUpdate"]["status"]["state"],
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
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    let events = common::read_sse(resp.into_body(), Duration::from_secs(5))
        .await
        .closed();
    assert!(
        events[0].get("task").is_some(),
        "first item is the Task snapshot, got {events:?}"
    );
    let states: Vec<&str> = events
        .iter()
        .filter_map(|e| e["statusUpdate"]["status"]["state"].as_str())
        .collect();
    assert_eq!(
        states,
        ["TASK_STATE_COMPLETED"],
        "the stream ends with the terminal event"
    );
}
