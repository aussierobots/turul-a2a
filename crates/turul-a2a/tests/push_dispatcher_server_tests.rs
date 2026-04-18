//! Server-level dispatcher integration tests (ADR-011 §13.1, §13.13).
//!
//! Unlike `push_delivery_integration.rs`, which exercises
//! `PushDeliveryWorker::deliver` in isolation, these tests stand up a
//! real `A2aServer` router, register push configs through the HTTP
//! CRUD surface, drive a task to a terminal state through normal
//! transport paths, and assert wiremock receives the dispatched POST
//! with the correct payload.
//!
//! This closes the most important gap in 0.1.x push delivery: the
//! wiring that takes durable commit events → per-config fan-out →
//! outbound HTTP POST.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Notify;
use axum::body::Body;
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::server::A2aServer;
use turul_a2a::storage::{A2aAtomicStore, A2aTaskStorage, InMemoryA2aStorage};
use turul_a2a::streaming::StreamEvent;
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Executor that does nothing; required because `A2aServer::builder()`
/// demands one even when the tests never drive a message:send.
struct DummyExecutor;

#[async_trait]
impl AgentExecutor for DummyExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

/// Poll wiremock's recorded requests until `expected` POSTs have
/// arrived, or `deadline` elapses. Returns the captured requests on
/// success. Push deliveries are spawned tokio tasks, so callers need
/// a short grace window after the triggering HTTP call returns.
async fn await_n_requests(
    server: &MockServer,
    expected: usize,
    deadline: Duration,
) -> Vec<wiremock::Request> {
    let start = Instant::now();
    loop {
        let reqs = server
            .received_requests()
            .await
            .expect("wiremock must have recording enabled");
        if reqs.len() >= expected {
            return reqs;
        }
        if start.elapsed() >= deadline {
            panic!(
                "timed out waiting for {expected} wiremock request(s); got {}",
                reqs.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Seed a task into shared storage in WORKING state, so the cancel
/// handler's grace-expiry path has something to force-commit.
async fn seed_working_task(storage: &InMemoryA2aStorage, task_id: &str) {
    let task = Task::new(task_id, TaskStatus::new(TaskState::Submitted))
        .with_context_id("ctx-server-push-dispatch");

    let submitted = StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: task_id.into(),
            context_id: "ctx-server-push-dispatch".into(),
            status: serde_json::to_value(&TaskStatus::new(TaskState::Submitted)).unwrap(),
        },
    };
    let working_task = storage
        .create_task_with_events("", "anonymous", task, vec![submitted])
        .await
        .expect("create_task_with_events")
        .0;

    let working = StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: task_id.into(),
            context_id: "ctx-server-push-dispatch".into(),
            status: serde_json::to_value(&TaskStatus::new(TaskState::Working)).unwrap(),
        },
    };
    storage
        .update_task_status_with_events(
            "",
            task_id,
            "anonymous",
            TaskStatus::new(TaskState::Working),
            vec![working],
        )
        .await
        .expect("update_task_status_with_events working");

    let _ = working_task; // suppress unused-binding lint on the intermediate snapshot
}

#[tokio::test]
async fn framework_cancel_triggers_push_delivery_with_canceled_state() {
    // --- Wiremock: expect exactly one POST on the cancel terminal --
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock)
        .await;

    // --- Server with short cancel grace + insecure-URL bypass -----
    // `cancel_handler_grace(50ms)` keeps the test quick; in real
    // deployments the default 5s grace is plenty. The wiremock URL
    // resolves to 127.0.0.1, which the SSRF guard would reject
    // without `allow_insecure_push_urls(true)`.
    let storage = InMemoryA2aStorage::new();
    let server = A2aServer::builder()
        .executor(DummyExecutor)
        .storage(storage.clone())
        .allow_insecure_push_urls(true)
        .cancel_handler_grace(Duration::from_millis(50))
        .cancel_handler_poll_interval(Duration::from_millis(10))
        .push_max_attempts(1)
        .push_backoff_cap(Duration::from_millis(10))
        .push_claim_expiry(Duration::from_secs(5))
        .build()
        .expect("server build");
    let router = server.into_router();

    // --- Seed a WORKING task directly in storage ------------------
    let task_id = "task-server-push-cancel";
    seed_working_task(&storage, task_id).await;

    // --- Register push config via the HTTP CRUD surface -----------
    let webhook_url = format!("{}/hook", mock.uri());
    let config_body = serde_json::json!({
        "id": "cfg-server-test",
        "taskId": task_id,
        "url": webhook_url,
        "token": "srv-token",
        "authentication": {
            "scheme": "Bearer",
            "credentials": "srv-cred",
        },
    });

    let create_req = Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/tasks/{task_id}/pushNotificationConfigs"
        ))
        .header("content-type", "application/json")
        .header("A2A-Version", "1.0")
        .body(Body::from(config_body.to_string()))
        .unwrap();
    let create_resp = router.clone().oneshot(create_req).await.unwrap();
    let status = create_resp.status();
    let body_bytes = create_resp.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8_lossy(&body_bytes);
    assert!(
        status.is_success(),
        "push config create failed: {status} body={body_str}"
    );

    // --- Drive the task to CANCELED via :cancel (framework-committed)
    // Nothing is in-flight on this instance, so the cancel handler
    // writes the marker, polls grace (50ms), and force-commits
    // CANCELED via the atomic store — exactly the path §13.13 pins.
    let cancel_req = Request::builder()
        .method(Method::POST)
        .uri(format!("/tasks/{task_id}:cancel"))
        .header("A2A-Version", "1.0")
        .body(Body::empty())
        .unwrap();
    let cancel_resp = router.clone().oneshot(cancel_req).await.unwrap();
    assert_eq!(
        cancel_resp.status(),
        StatusCode::OK,
        "cancel must succeed"
    );
    let cancel_body = cancel_resp.into_body().collect().await.unwrap().to_bytes();
    let cancel_json: serde_json::Value = serde_json::from_slice(&cancel_body).unwrap();
    assert_eq!(
        cancel_json["status"]["state"].as_str(),
        Some("TASK_STATE_CANCELED"),
        "cancel response body must carry the terminal state"
    );

    // --- Await the dispatched POST --------------------------------
    let reqs = await_n_requests(&mock, 1, Duration::from_secs(3)).await;
    assert_eq!(reqs.len(), 1);

    // Payload is the full Task JSON with terminal state CANCELED.
    let posted: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(
        posted["id"].as_str(),
        Some(task_id),
        "POST body must identify the task"
    );
    assert_eq!(
        posted["status"]["state"].as_str(),
        Some("TASK_STATE_CANCELED"),
        "POST body must carry the final terminal state"
    );

    // Headers: Authorization + X-Turul-Push-Token populated from config.
    let auth_header = reqs[0]
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        auth_header.starts_with("Bearer srv-cred"),
        "Authorization header must carry scheme + credentials, got {auth_header:?}"
    );
    let token_header = reqs[0]
        .headers
        .get("x-turul-push-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        token_header, "srv-token",
        "X-Turul-Push-Token header must carry the config token"
    );

    // Drop order matters: MockServer::drop() verifies `.expect(1)`.
    drop(mock);
}

/// Executor that blocks on a notify, then completes via the sink.
/// This mirrors the real-world "long-running task" shape: the HTTP
/// caller doesn't block, the test registers a push config while the
/// task is WORKING, and only then unblocks the executor.
struct GatedExecutor {
    gate: Arc<Notify>,
}

#[async_trait]
impl AgentExecutor for GatedExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        self.gate.notified().await;
        ctx.events
            .complete(Some(Message::new(
                "m-done",
                turul_a2a_types::Role::Agent,
                vec![turul_a2a_types::Part::text("done")],
            )))
            .await
            .map(|_| ())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

#[tokio::test]
async fn executor_completion_triggers_push_delivery_with_completed_state() {
    // --- Wiremock: expect exactly one POST for the terminal event -
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock)
        .await;

    // --- Server with gated executor -------------------------------
    let storage = InMemoryA2aStorage::new();
    let gate = Arc::new(Notify::new());
    let server = A2aServer::builder()
        .executor(GatedExecutor {
            gate: gate.clone(),
        })
        .storage(storage.clone())
        .allow_insecure_push_urls(true)
        .push_max_attempts(1)
        .push_backoff_cap(Duration::from_millis(10))
        .push_claim_expiry(Duration::from_secs(5))
        .build()
        .expect("server build");
    let router = server.into_router();

    // --- Send a non-blocking message so the executor spawns and parks
    let send_body = serde_json::json!({
        "message": {
            "messageId": "m-1",
            "role": "ROLE_USER",
            "parts": [{"text": "hello"}],
        }
    });
    let router_send = router.clone();
    let send_handle = tokio::spawn(async move {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/message:stream")
            .header("content-type", "application/json")
            .header("A2A-Version", "1.0")
            .header("accept", "text/event-stream")
            .body(Body::from(send_body.to_string()))
            .unwrap();
        let resp = router_send.oneshot(req).await.unwrap();
        // Drain the streaming body so the send completes; content is
        // incidental to this test — we only care that the executor ran.
        let _ = resp.into_body().collect().await.unwrap();
    });

    // --- Wait until exactly one task is registered as WORKING -----
    let task_id = {
        let start = Instant::now();
        loop {
            let filter = turul_a2a::storage::TaskFilter {
                tenant: Some(String::new()),
                owner: Some("anonymous".into()),
                status: Some(turul_a2a_types::TaskState::Working),
                ..Default::default()
            };
            if let Some(tid) = storage
                .list_tasks(filter)
                .await
                .ok()
                .and_then(|page| page.tasks.into_iter().next())
                .map(|t| t.as_proto().id.clone())
            {
                break tid;
            }
            if start.elapsed() > Duration::from_secs(2) {
                panic!("executor did not reach WORKING within 2s");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };

    // --- Register push config BEFORE the executor completes -------
    let webhook_url = format!("{}/hook", mock.uri());
    let config_body = serde_json::json!({
        "id": "cfg-executor-test",
        "taskId": task_id,
        "url": webhook_url,
        "token": "exec-token",
        "authentication": {
            "scheme": "Bearer",
            "credentials": "exec-cred",
        },
    });
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/tasks/{task_id}/pushNotificationConfigs"))
                .header("content-type", "application/json")
                .header("A2A-Version", "1.0")
                .body(Body::from(config_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(create_resp.status().is_success(), "push config create");

    // --- Unblock the executor; it commits COMPLETED through the sink
    gate.notify_one();
    let _ = send_handle.await;

    // --- Await the dispatched POST --------------------------------
    let reqs = await_n_requests(&mock, 1, Duration::from_secs(3)).await;
    assert_eq!(reqs.len(), 1);
    let posted: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(posted["id"].as_str(), Some(task_id.as_str()));
    assert_eq!(
        posted["status"]["state"].as_str(),
        Some("TASK_STATE_COMPLETED"),
        "executor COMPLETED path must dispatch the terminal status"
    );

    drop(mock);
}
