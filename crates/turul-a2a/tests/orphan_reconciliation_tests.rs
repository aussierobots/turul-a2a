//! Startup reconciliation of orphaned tasks.
//!
//! A task left SUBMITTED or WORKING by a process that exited has no
//! executor to finish it. `A2aServer::fail_orphaned_tasks` (and the
//! builder opt-in that runs it from `bind`) commits FAILED for those
//! tasks across every tenant and owner, leaves client-held
//! (INPUT_REQUIRED, AUTH_REQUIRED) and terminal tasks untouched, and is
//! idempotent. The end-to-end cases read the result back over TCP.

use std::time::Duration;

use turul_a2a::A2aServer;
use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::storage::{A2aAtomicStore, A2aEventStore, A2aTaskStorage, InMemoryA2aStorage};
use turul_a2a::streaming::{StatusUpdatePayload, StreamEvent};
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};

/// HTTP requests without auth middleware run as this owner on the
/// default (empty) tenant.
const HTTP_TENANT: &str = "";
const HTTP_OWNER: &str = "anonymous";

const RESTART_REASON: &str = "task failed: the server restarted while the task was running; \
                              its executor did not survive the restart";

struct StreamingAgent;

#[async_trait::async_trait]
impl AgentExecutor for StreamingAgent {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Reconciling Agent", "1.0.0")
            .description("Agent for orphan reconciliation tests")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .streaming(true)
            .build()
            .unwrap()
    }
}

fn status_event(task_id: &str, state: TaskState) -> StreamEvent {
    StreamEvent::StatusUpdate {
        status_update: StatusUpdatePayload {
            task_id: task_id.to_string(),
            context_id: format!("ctx-{task_id}"),
            status: serde_json::to_value(TaskStatus::new(state)).unwrap(),
        },
    }
}

/// Seed a task and drive it to `state` through the atomic store, one
/// status event per transition, as a server that later died would have.
async fn seed(storage: &InMemoryA2aStorage, tenant: &str, owner: &str, id: &str, state: TaskState) {
    let task =
        Task::new(id, TaskStatus::new(TaskState::Submitted)).with_context_id(format!("ctx-{id}"));
    storage.create_task(tenant, owner, task).await.unwrap();
    if state == TaskState::Submitted {
        return;
    }
    storage
        .update_task_status_with_events(
            tenant,
            id,
            owner,
            TaskStatus::new(TaskState::Working),
            vec![status_event(id, TaskState::Working)],
        )
        .await
        .unwrap();
    if state != TaskState::Working {
        storage
            .update_task_status_with_events(
                tenant,
                id,
                owner,
                TaskStatus::new(state),
                vec![status_event(id, state)],
            )
            .await
            .unwrap();
    }
}

/// One task per state on the HTTP tenant/owner, plus executor-held
/// tasks on another tenant and owner.
async fn seed_all(storage: &InMemoryA2aStorage) {
    for (id, state) in [
        ("t-submitted", TaskState::Submitted),
        ("t-working", TaskState::Working),
        ("t-input", TaskState::InputRequired),
        ("t-auth", TaskState::AuthRequired),
        ("t-completed", TaskState::Completed),
        ("t-failed", TaskState::Failed),
        ("t-canceled", TaskState::Canceled),
        ("t-rejected", TaskState::Rejected),
    ] {
        seed(storage, HTTP_TENANT, HTTP_OWNER, id, state).await;
    }
    seed(
        storage,
        "tenant-b",
        "owner-b",
        "b-working",
        TaskState::Working,
    )
    .await;
    seed(
        storage,
        "tenant-b",
        "owner-b",
        "b-submitted",
        TaskState::Submitted,
    )
    .await;
}

const EXECUTOR_HELD: [(&str, &str, &str); 4] = [
    (HTTP_TENANT, HTTP_OWNER, "t-submitted"),
    (HTTP_TENANT, HTTP_OWNER, "t-working"),
    ("tenant-b", "owner-b", "b-working"),
    ("tenant-b", "owner-b", "b-submitted"),
];

const SEEDED: [(&str, &str); 10] = [
    (HTTP_TENANT, "t-submitted"),
    (HTTP_TENANT, "t-working"),
    (HTTP_TENANT, "t-input"),
    (HTTP_TENANT, "t-auth"),
    (HTTP_TENANT, "t-completed"),
    (HTTP_TENANT, "t-failed"),
    (HTTP_TENANT, "t-canceled"),
    (HTTP_TENANT, "t-rejected"),
    ("tenant-b", "b-working"),
    ("tenant-b", "b-submitted"),
];

const UNTOUCHED: [(&str, TaskState); 6] = [
    ("t-input", TaskState::InputRequired),
    ("t-auth", TaskState::AuthRequired),
    ("t-completed", TaskState::Completed),
    ("t-failed", TaskState::Failed),
    ("t-canceled", TaskState::Canceled),
    ("t-rejected", TaskState::Rejected),
];

fn builder(storage: &InMemoryA2aStorage) -> turul_a2a::server::A2aServerBuilder {
    A2aServer::builder()
        .executor(StreamingAgent)
        .storage(storage.clone())
        .bind(([127, 0, 0, 1], 0))
}

async fn state_of(storage: &InMemoryA2aStorage, tenant: &str, owner: &str, id: &str) -> TaskState {
    storage
        .get_task(tenant, id, owner, Some(0))
        .await
        .unwrap()
        .expect("task exists")
        .status()
        .unwrap()
        .state()
        .unwrap()
}

/// Text of the task's status message, read from the wire JSON.
fn status_text(task: &Task) -> String {
    let json = serde_json::to_value(task).unwrap();
    json["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

async fn event_states(storage: &InMemoryA2aStorage, tenant: &str, id: &str) -> Vec<TaskState> {
    storage
        .get_events_after(tenant, id, 0)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|(_, e)| e.task_state())
        .collect()
}

/// Event states of every seeded task, across all tenants.
async fn all_events(storage: &InMemoryA2aStorage) -> Vec<(String, Vec<TaskState>)> {
    let mut out = Vec::new();
    for (tenant, id) in SEEDED {
        out.push((id.to_string(), event_states(storage, tenant, id).await));
    }
    out
}

#[tokio::test]
async fn fails_executor_held_tasks_on_every_tenant_and_owner() {
    let storage = InMemoryA2aStorage::new();
    seed_all(&storage).await;
    let mut before = Vec::new();
    for (tenant, _, id) in EXECUTOR_HELD {
        before.push(event_states(&storage, tenant, id).await);
    }
    let mut server = builder(&storage).build().unwrap();

    let failed = server.fail_orphaned_tasks().await.expect("reconcile");
    assert_eq!(failed, EXECUTOR_HELD.len());

    for ((tenant, owner, id), events_before) in EXECUTOR_HELD.into_iter().zip(before) {
        let task = storage
            .get_task(tenant, id, owner, Some(0))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            task.status().unwrap().state().unwrap(),
            TaskState::Failed,
            "{id}"
        );
        assert_eq!(status_text(&task), RESTART_REASON, "{id}");
        let json = serde_json::to_value(&task).unwrap();
        assert_eq!(json["status"]["message"]["role"], "ROLE_AGENT", "{id}");
        assert!(
            json["status"]["message"]["messageId"]
                .as_str()
                .is_some_and(|m| !m.is_empty()),
            "{id}: messageId must be a non-empty string"
        );

        let mut expected = events_before;
        expected.push(TaskState::Failed);
        assert_eq!(
            event_states(&storage, tenant, id).await,
            expected,
            "{id}: exactly one FAILED event appended"
        );
    }
}

#[tokio::test]
async fn leaves_client_held_and_terminal_tasks_untouched() {
    let storage = InMemoryA2aStorage::new();
    seed_all(&storage).await;
    let mut before = Vec::new();
    for (id, _) in UNTOUCHED {
        before.push(event_states(&storage, HTTP_TENANT, id).await);
    }

    let mut server = builder(&storage).build().unwrap();
    server.fail_orphaned_tasks().await.expect("reconcile");

    for ((id, state), events_before) in UNTOUCHED.into_iter().zip(before) {
        assert_eq!(
            state_of(&storage, HTTP_TENANT, HTTP_OWNER, id).await,
            state,
            "{id}"
        );
        assert_eq!(
            event_states(&storage, HTTP_TENANT, id).await,
            events_before,
            "{id}: no event written"
        );
    }
}

#[tokio::test]
async fn second_run_fails_nothing_and_writes_no_event() {
    let storage = InMemoryA2aStorage::new();
    seed_all(&storage).await;
    let mut server = builder(&storage).build().unwrap();
    server.fail_orphaned_tasks().await.expect("first run");
    let events_after_first = all_events(&storage).await;

    let mut restarted = builder(&storage).build().unwrap();
    assert_eq!(
        restarted.fail_orphaned_tasks().await.expect("second run"),
        0
    );
    assert_eq!(all_events(&storage).await, events_after_first);
}

#[tokio::test]
async fn builder_opt_in_fails_orphans_before_bind_returns_and_http_reads_failed() {
    let storage = InMemoryA2aStorage::new();
    seed_all(&storage).await;

    let bound = builder(&storage)
        .fail_orphaned_tasks_on_start()
        .build()
        .unwrap()
        .bind()
        .await
        .expect("bind");
    for (tenant, owner, id) in EXECUTOR_HELD {
        assert_eq!(
            state_of(&storage, tenant, owner, id).await,
            TaskState::Failed,
            "{id}"
        );
    }

    let addr = bound.local_addr();
    let serving = tokio::spawn(bound.run());
    let http = reqwest::Client::new();

    let task: serde_json::Value = http
        .get(format!("http://{addr}/tasks/t-working"))
        .header("A2A-Version", "1.0")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
    assert_eq!(
        task["status"]["message"]["parts"][0]["text"],
        RESTART_REASON
    );

    let subscribe = http
        .get(format!("http://{addr}/tasks/t-working:subscribe"))
        .header("A2A-Version", "1.0")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(
        subscribe.status(),
        400,
        "subscribe on a terminal task is refused"
    );
    let body: serde_json::Value = subscribe.json().await.unwrap();
    let details = &body["error"]["details"][0];
    assert_eq!(details["reason"], "UNSUPPORTED_OPERATION");
    assert_eq!(details["domain"], "a2a-protocol.org");
    assert_eq!(details["@type"], "type.googleapis.com/google.rpc.ErrorInfo");

    serving.abort();
    let _ = serving.await;
}

#[tokio::test]
#[should_panic(expected = "server.fail_orphaned_tasks()")]
async fn builder_opt_in_with_into_router_requires_the_method_first() {
    let storage = InMemoryA2aStorage::new();
    let _ = builder(&storage)
        .fail_orphaned_tasks_on_start()
        .build()
        .unwrap()
        .into_router();
}

#[cfg(feature = "grpc")]
#[tokio::test]
#[should_panic(expected = "server.fail_orphaned_tasks()")]
async fn builder_opt_in_with_into_routers_requires_the_method_first() {
    let storage = InMemoryA2aStorage::new();
    let _ = builder(&storage)
        .fail_orphaned_tasks_on_start()
        .build()
        .unwrap()
        .into_routers();
}

#[tokio::test]
async fn builder_opt_in_with_into_router_serves_after_the_method() {
    use tower::ServiceExt;

    let storage = InMemoryA2aStorage::new();
    seed_all(&storage).await;
    let mut server = builder(&storage)
        .fail_orphaned_tasks_on_start()
        .build()
        .unwrap();
    server.fail_orphaned_tasks().await.expect("reconcile");

    let response = server
        .into_router()
        .oneshot(
            http::Request::get("/tasks/t-submitted")
                .header("A2A-Version", "1.0")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let task: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
}

#[tokio::test]
async fn without_opt_in_orphans_stay_executor_held() {
    let storage = InMemoryA2aStorage::new();
    seed_all(&storage).await;

    let bound = builder(&storage)
        .build()
        .unwrap()
        .bind()
        .await
        .expect("bind");
    for (tenant, owner, id) in EXECUTOR_HELD {
        let seeded = if id.ends_with("submitted") {
            TaskState::Submitted
        } else {
            TaskState::Working
        };
        assert_eq!(state_of(&storage, tenant, owner, id).await, seeded, "{id}");
    }
    drop(bound);
}
