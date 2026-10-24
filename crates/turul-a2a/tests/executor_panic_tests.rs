//! An executor that panics must leave its task FAILED, never stranded
//! in a non-terminal state, without ever overriding a terminal the
//! executor already committed.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::middleware::MiddlewareStack;
use turul_a2a::router::{AppState, core_send_message};
use turul_a2a::server::RuntimeConfig;
use turul_a2a::server::in_flight::InFlightRegistry;
use turul_a2a::storage::{A2aEventStore, InMemoryA2aStorage};
use turul_a2a::streaming::TaskEventBroker;
use turul_a2a_types::{Artifact, Message, Part, Task, TaskState};

#[derive(Clone, Copy)]
enum Mode {
    WorkingThenPanic,
    PanicOnly,
    CompleteThenPanic,
    InputRequiredThenPanic,
}

struct Panics(Mode);

#[async_trait]
impl AgentExecutor for Panics {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        match self.0 {
            Mode::WorkingThenPanic => {
                // The task is already WORKING when the executor starts, so
                // this repeat transition is refused; the executor carries on.
                let _ = ctx.events.set_status(TaskState::Working, None).await;
                // The sink is used (an artifact commits) but never closed.
                let emitted = ctx
                    .events
                    .emit_artifact(
                        Artifact::new("a1", vec![Part::text("partial")]),
                        false,
                        true,
                    )
                    .await;
                assert!(emitted.is_ok(), "artifact commit failed: {emitted:?}");
            }
            Mode::PanicOnly => {}
            Mode::CompleteThenPanic => {
                ctx.events.complete(None).await?;
            }
            Mode::InputRequiredThenPanic => {
                let interrupted = ctx.events.require_input(None).await;
                assert!(interrupted.is_ok(), "require_input failed: {interrupted:?}");
            }
        }
        panic!("boom: secret payload");
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

fn state_for(mode: Mode) -> (AppState, InMemoryA2aStorage) {
    let storage = InMemoryA2aStorage::new();
    let mut runtime_config = RuntimeConfig::default();
    runtime_config.supports_return_immediately = true;
    let state = AppState {
        executor: Arc::new(Panics(mode)),
        task_storage: Arc::new(storage.clone()),
        push_storage: Arc::new(storage.clone()),
        event_store: Arc::new(storage.clone()),
        atomic_store: Arc::new(storage.clone()),
        event_broker: TaskEventBroker::new(),
        middleware_stack: Arc::new(MiddlewareStack::new(vec![])),
        runtime_config,
        in_flight: Arc::new(InFlightRegistry::new()),
        cancellation_supervisor: Arc::new(storage.clone()),
        push_delivery_store: None,
        push_dispatcher: None,
        durable_executor_queue: None,
    };
    (state, storage)
}

fn body(return_immediately: bool) -> String {
    serde_json::json!({
        "message": {
            "messageId": uuid::Uuid::now_v7().to_string(),
            "role": "ROLE_USER",
            "parts": [{"text": "go", "mediaType": "text/plain"}],
        },
        "configuration": { "returnImmediately": return_immediately },
    })
    .to_string()
}

async fn send(state: &AppState, return_immediately: bool) -> serde_json::Value {
    tokio::time::timeout(
        Duration::from_secs(10),
        core_send_message(
            state.clone(),
            "",
            "owner",
            None,
            &Default::default(),
            body(return_immediately),
        ),
    )
    .await
    .expect("send hung")
    .expect("send must succeed")
    .0
}

async fn wait_registry_empty(state: &AppState) {
    for _ in 0..500 {
        if state.in_flight.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("in-flight registry entry was never removed");
}

async fn wait_for_state(state: &AppState, task_id: &str, want: &str) -> serde_json::Value {
    for _ in 0..500 {
        let t = state
            .task_storage
            .get_task("", task_id, "owner", None)
            .await
            .unwrap()
            .unwrap();
        let v = serde_json::to_value(&t).unwrap();
        if v["status"]["state"] == want {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("task never reached {want}");
}

/// The FAILED status message is an agent-role message with an id.
fn assert_agent_message(task: &serde_json::Value) {
    assert_eq!(task["status"]["message"]["role"], "ROLE_AGENT");
    assert!(
        task["status"]["message"]["messageId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "messageId must be a non-empty string: {task}"
    );
}

async fn terminal_event_count(storage: &InMemoryA2aStorage, task_id: &str) -> usize {
    storage
        .get_events_after("", task_id, 0)
        .await
        .unwrap()
        .iter()
        .filter(|(_, e)| e.is_terminal())
        .count()
}

#[tokio::test]
async fn non_blocking_send_panic_after_working_fails_task() {
    let (state, storage) = state_for(Mode::WorkingThenPanic);
    let resp = send(&state, true).await;
    let task_id = resp["task"]["id"].as_str().unwrap().to_string();

    let task = wait_for_state(&state, &task_id, "TASK_STATE_FAILED").await;
    assert_eq!(
        task["status"]["message"]["parts"][0]["text"],
        "executor panicked"
    );
    assert!(!task.to_string().contains("secret payload"));
    assert_agent_message(&task);
    assert_eq!(terminal_event_count(&storage, &task_id).await, 1);
    wait_registry_empty(&state).await;
}

#[tokio::test]
async fn blocking_send_panic_returns_failed_task() {
    let (state, storage) = state_for(Mode::PanicOnly);
    let resp = send(&state, false).await;
    assert_eq!(resp["task"]["status"]["state"], "TASK_STATE_FAILED");
    assert_agent_message(&resp["task"]);
    let task_id = resp["task"]["id"].as_str().unwrap().to_string();
    assert_eq!(terminal_event_count(&storage, &task_id).await, 1);
    wait_registry_empty(&state).await;
}

#[tokio::test]
async fn panic_after_complete_keeps_completed() {
    let (state, storage) = state_for(Mode::CompleteThenPanic);
    let resp = send(&state, true).await;
    let task_id = resp["task"]["id"].as_str().unwrap().to_string();

    wait_for_state(&state, &task_id, "TASK_STATE_COMPLETED").await;
    wait_registry_empty(&state).await;
    let task = wait_for_state(&state, &task_id, "TASK_STATE_COMPLETED").await;
    assert!(task["status"]["message"].is_null());
    assert_eq!(terminal_event_count(&storage, &task_id).await, 1);
}

#[tokio::test]
async fn panic_after_require_input_fails_task() {
    let (state, storage) = state_for(Mode::InputRequiredThenPanic);
    let resp = send(&state, true).await;
    let task_id = resp["task"]["id"].as_str().unwrap().to_string();

    let task = wait_for_state(&state, &task_id, "TASK_STATE_FAILED").await;
    assert_eq!(
        task["status"]["message"]["parts"][0]["text"],
        "executor panicked"
    );
    assert_agent_message(&task);
    assert_eq!(terminal_event_count(&storage, &task_id).await, 1);
    wait_registry_empty(&state).await;
}
