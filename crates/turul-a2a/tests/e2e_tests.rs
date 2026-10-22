//! End-to-end protocol compliance tests.
//!
//! Full lifecycle scenarios exercising HTTP + JSON-RPC + SSE together.
//! Assertions are wire-level and spec-level, not implementation-shaped.

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

// =========================================================
// Test executors with different behaviors
// =========================================================

/// Completes immediately with an artifact.
struct CompletingExecutor;

#[async_trait::async_trait]
impl AgentExecutor for CompletingExecutor {
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
        p.artifacts.push(turul_a2a_proto::Artifact {
            artifact_id: "e2e-art".into(),
            name: "Result".into(),
            description: String::new(),
            parts: vec![turul_a2a_proto::Part {
                content: Some(turul_a2a_proto::part::Content::Text("e2e done".into())),
                metadata: None,
                filename: String::new(),
                media_type: String::new(),
            }],
            metadata: None,
            extensions: vec![],
        });
        *task = Task::try_from(p).unwrap();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        make_agent_card(true)
    }
}

/// Pauses at INPUT_REQUIRED, then completes on second call.
struct MultiTurnExecutor {
    call_count: Arc<std::sync::atomic::AtomicU32>,
}

impl MultiTurnExecutor {
    fn new() -> Self {
        Self {
            call_count: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl AgentExecutor for MultiTurnExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
        let n = self
            .call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let new_state = if n == 0 {
            turul_a2a_proto::TaskState::InputRequired
        } else {
            turul_a2a_proto::TaskState::Completed
        };
        let mut p = task.as_proto().clone();
        p.status = Some(turul_a2a_proto::TaskStatus {
            state: new_state.into(),
            message: None,
            timestamp: None,
        });
        *task = Task::try_from(p).unwrap();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        make_agent_card(true)
    }
}

fn make_agent_card(push: bool) -> turul_a2a_proto::AgentCard {
    turul_a2a_proto::AgentCard {
        name: "E2E Agent".into(),
        description: "E2E test agent".into(),
        supported_interfaces: vec![turul_a2a_proto::AgentInterface {
            url: "http://localhost".into(),
            protocol_binding: "JSONRPC".into(),
            tenant: String::new(),
            protocol_version: "1.0".into(),
        }],
        provider: None,
        version: "1.0.0".into(),
        documentation_url: None,
        capabilities: Some(turul_a2a_proto::AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(push),
            extensions: vec![],
            extended_agent_card: Some(true),
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

fn state_with(executor: impl AgentExecutor + 'static) -> AppState {
    let s = InMemoryA2aStorage::new();
    AppState {
        executor: Arc::new(executor),
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

// =========================================================
// Helpers
// =========================================================

async fn post_json(router: axum::Router, uri: &str, body: &str) -> (u16, serde_json::Value) {
    let req = Request::post(uri)
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let st = resp.status().as_u16();
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&b).unwrap_or_default())
}

async fn get_json(router: axum::Router, uri: &str) -> (u16, serde_json::Value) {
    let req = Request::get(uri)
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let st = resp.status().as_u16();
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&b).unwrap_or_default())
}

fn send_body(id: &str, text: &str) -> String {
    serde_json::json!({"message":{"messageId":id,"role":"ROLE_USER","parts":[{"text":text}]}})
        .to_string()
}

fn send_body_with_task(id: &str, text: &str, task_id: &str) -> String {
    serde_json::json!({"message":{"messageId":id,"taskId":task_id,"role":"ROLE_USER","parts":[{"text":text}]}}).to_string()
}

fn jrpc(method: &str, params: serde_json::Value, id: i64) -> String {
    serde_json::json!({"jsonrpc":"2.0","method":method,"params":params,"id":id}).to_string()
}

async fn jrpc_call(router: axum::Router, body: &str) -> (u16, serde_json::Value) {
    post_json(router, "/jsonrpc", body).await
}

// =========================================================
// E2E-01: Full SendMessage lifecycle
// =========================================================

#[tokio::test]
async fn e2e_send_message_lifecycle() {
    let st = state_with(CompletingExecutor);

    // Send → creates task, executor completes it
    let (status, body) = post_json(
        build_router(st.clone()),
        "/message:send",
        &send_body("e1", "go"),
    )
    .await;
    assert_eq!(status, 200);
    let task = &body["task"];
    let tid = task["id"].as_str().unwrap();
    assert!(!tid.is_empty());
    assert!(task.get("status").is_some());

    // GetTask confirms persisted state
    let (status, body) = get_json(build_router(st.clone()), &format!("/tasks/{tid}")).await;
    assert_eq!(status, 200);
    assert_eq!(body["id"], tid);

    // Artifacts present
    let arts = body["artifacts"].as_array();
    assert!(
        arts.is_some_and(|a| !a.is_empty()),
        "Task should have artifacts"
    );
}

// =========================================================
// E2E-02: Multi-turn INPUT_REQUIRED
// =========================================================

#[tokio::test]
async fn e2e_multi_turn_input_required() {
    let st = state_with(MultiTurnExecutor::new());

    // First send → INPUT_REQUIRED
    let (status, body) = post_json(
        build_router(st.clone()),
        "/message:send",
        &send_body("mt1", "first"),
    )
    .await;
    assert_eq!(status, 200);
    let tid = body["task"]["id"].as_str().unwrap().to_string();
    let state_str = body["task"]["status"]["state"].as_str().unwrap();
    assert_eq!(state_str, "TASK_STATE_INPUT_REQUIRED");

    // Second send with same task_id → resumes the SAME task → COMPLETED
    let (status, body) = post_json(
        build_router(st.clone()),
        "/message:send",
        &send_body_with_task("mt2", "follow-up", &tid),
    )
    .await;
    assert_eq!(status, 200);
    let tid2 = body["task"]["id"].as_str().unwrap();
    assert_eq!(
        tid2, tid,
        "Second send must continue the same task, not create a new one"
    );
    assert_eq!(body["task"]["status"]["state"], "TASK_STATE_COMPLETED");

    // The executor adds no messages, so history is exactly the two user
    // turns, in order.
    let (_, body) = get_json(build_router(st), &format!("/tasks/{tid}")).await;
    let ids: Vec<&str> = body["history"]
        .as_array()
        .expect("history")
        .iter()
        .map(|m| m["messageId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["mt1", "mt2"], "{body}");
}

// =========================================================
// E2E-03: Cancel while non-terminal
// =========================================================

#[tokio::test]
async fn e2e_cancel_non_terminal() {
    let st = state_with(MultiTurnExecutor::new());

    // Create task that stops at INPUT_REQUIRED
    let (_, body) = post_json(
        build_router(st.clone()),
        "/message:send",
        &send_body("cn1", "pause"),
    )
    .await;
    let tid = body["task"]["id"].as_str().unwrap();

    // Cancel it
    let (status, body) = post_json(
        build_router(st.clone()),
        &format!("/tasks/{tid}:cancel"),
        "",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["status"]["state"], "TASK_STATE_CANCELED");

    // Verify persisted
    let (_, body) = get_json(build_router(st), &format!("/tasks/{tid}")).await;
    assert_eq!(body["status"]["state"], "TASK_STATE_CANCELED");
}

// =========================================================
// E2E-05: Streaming event sequence
// =========================================================

#[tokio::test]
async fn e2e_streaming_event_sequence() {
    let st = state_with(CompletingExecutor);
    let router = build_router(st);
    let req = Request::post("/message:stream")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(send_body("se1", "stream me")))
        .unwrap();

    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let events = common::read_sse(resp.into_body(), Duration::from_secs(5))
        .await
        .closed();

    // The Task snapshot (already WORKING) opens the stream; the only
    // status update after it is the terminal COMPLETED.
    assert_eq!(
        events[0]["task"]["status"]["state"], "TASK_STATE_WORKING",
        "first event is the Task snapshot, got {events:?}"
    );
    assert_eq!(common::status_states(&events), ["TASK_STATE_COMPLETED"]);
}

// =========================================================
// E2E-06: Subscribe on live non-terminal task
//
// The subscriber attaches to a WORKING task, then the task completes:
// the stream carries the snapshot, then the live COMPLETED event, then
// closes.
// =========================================================

#[tokio::test]
async fn e2e_subscribe_live_task() {
    let st = state_with(MultiTurnExecutor::new());

    // Create task with events via atomic store (simulating a streaming creation)
    let task = turul_a2a_types::Task::new(
        "sub-e2e-1",
        turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Submitted),
    )
    .with_context_id("ctx-sub-e2e");

    let submitted_event = turul_a2a::streaming::StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: "sub-e2e-1".to_string(),
            context_id: "ctx-sub-e2e".to_string(),
            status: serde_json::json!({"state": "TASK_STATE_SUBMITTED"}),
        },
    };

    st.atomic_store
        .create_task_with_events("", "anonymous", task, vec![submitted_event])
        .await
        .unwrap();

    // Move to Working with event
    let working_event = turul_a2a::streaming::StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: "sub-e2e-1".to_string(),
            context_id: "ctx-sub-e2e".to_string(),
            status: serde_json::json!({"state": "TASK_STATE_WORKING"}),
        },
    };

    st.atomic_store
        .update_task_status_with_events(
            "",
            "sub-e2e-1",
            "anonymous",
            turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Working),
            vec![working_event],
        )
        .await
        .unwrap();

    // Subscribe to the non-terminal task
    let router = build_router(st.clone());
    let req = Request::get("/tasks/sub-e2e-1:subscribe")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.contains("text/event-stream"));

    // The response head means the snapshot is taken; the task now
    // completes, as an executor would complete it.
    let completed_event = turul_a2a::streaming::StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: "sub-e2e-1".to_string(),
            context_id: "ctx-sub-e2e".to_string(),
            status: serde_json::json!({"state": "TASK_STATE_COMPLETED"}),
        },
    };
    st.atomic_store
        .update_task_status_with_events(
            "",
            "sub-e2e-1",
            "anonymous",
            turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Completed),
            vec![completed_event],
        )
        .await
        .unwrap();
    st.event_broker.notify("sub-e2e-1").await;

    let events = common::read_sse(resp.into_body(), Duration::from_secs(5))
        .await
        .closed();

    // First event is the Task object (spec §3.1.6); it already reflects
    // SUBMITTED and WORKING, so neither is replayed. The live event
    // follows, and the stream closes after it.
    assert_eq!(events.len(), 2, "snapshot + live event, got: {events:?}");
    assert_eq!(events[0]["task"]["status"]["state"], "TASK_STATE_WORKING");
    assert_eq!(common::status_states(&events), ["TASK_STATE_COMPLETED"]);
}

// =========================================================
// E2E-09: Pagination
// =========================================================

#[tokio::test]
async fn e2e_pagination() {
    let st = state_with(CompletingExecutor);

    // Create 7 tasks
    for i in 0..7 {
        post_json(
            build_router(st.clone()),
            "/message:send",
            &send_body(&format!("pg{i}"), "t"),
        )
        .await;
    }

    // Page through with pageSize=3: pages of 3, 3, 1.
    let mut all_ids = vec![];
    let mut page_sizes = vec![];
    let mut token: Option<String> = None;
    loop {
        let uri = match &token {
            Some(t) => format!("/tasks?pageSize=3&pageToken={t}"),
            None => "/tasks?pageSize=3".to_string(),
        };
        let (_, body) = get_json(build_router(st.clone()), &uri).await;
        assert_eq!(body["totalSize"], 7);
        let tasks = body["tasks"].as_array().unwrap();
        page_sizes.push(tasks.len());
        assert!(page_sizes.len() <= 3, "pagination must terminate");
        all_ids.extend(
            tasks
                .iter()
                .filter_map(|t| t["id"].as_str().map(String::from)),
        );

        let npt = body["nextPageToken"].as_str().unwrap_or("");
        if npt.is_empty() {
            break;
        }
        token = Some(npt.to_string());
    }
    assert_eq!(page_sizes, vec![3, 3, 1]);
    assert_eq!(all_ids.len(), 7);
    let unique: std::collections::HashSet<_> = all_ids.iter().collect();
    assert_eq!(unique.len(), 7, "No duplicates across pages");
}

// =========================================================
// E2E-10: Agent card discovery
// =========================================================

#[tokio::test]
async fn e2e_agent_card() {
    let st = state_with(CompletingExecutor);
    let (status, body) = get_json(build_router(st), "/.well-known/agent-card.json").await;
    assert_eq!(status, 200);
    assert_eq!(body["name"], "E2E Agent");
    assert!(body.get("defaultInputModes").is_some());
    assert!(body.get("defaultOutputModes").is_some());
    assert!(body.get("capabilities").is_some());
    assert!(body.get("supportedInterfaces").is_some());
}

#[tokio::test]
async fn e2e_extended_card_not_configured() {
    let st = state_with(CompletingExecutor);
    let (status, body) = get_json(build_router(st), "/extendedAgentCard").await;
    assert_eq!(status, 400);
    let details = body["error"]["details"].as_array().unwrap();
    assert_eq!(details[0]["reason"], "EXTENDED_AGENT_CARD_NOT_CONFIGURED");
}

// =========================================================
// E2E-12: Error mapping
// =========================================================

#[tokio::test]
async fn e2e_error_mapping_404() {
    let st = state_with(CompletingExecutor);
    let (status, body) = get_json(build_router(st), "/tasks/nope").await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], 404);
    let d = &body["error"]["details"].as_array().unwrap()[0];
    assert_eq!(d["@type"], "type.googleapis.com/google.rpc.ErrorInfo");
    assert_eq!(d["reason"], "TASK_NOT_FOUND");
    assert_eq!(d["domain"], "a2a-protocol.org");
}

#[tokio::test]
async fn e2e_error_mapping_task_not_cancelable() {
    let st = state_with(CompletingExecutor);
    let (_, send) = post_json(
        build_router(st.clone()),
        "/message:send",
        &send_body("em", "x"),
    )
    .await;
    let tid = send["task"]["id"].as_str().unwrap();

    let (status, body) = post_json(build_router(st), &format!("/tasks/{tid}:cancel"), "").await;
    assert_eq!(status, 400);
    let d = &body["error"]["details"].as_array().unwrap()[0];
    assert_eq!(d["reason"], "TASK_NOT_CANCELABLE");
}

// =========================================================
// E2E-15: Tenant isolation across transports
// =========================================================

#[tokio::test]
async fn e2e_tenant_isolation() {
    let st = state_with(CompletingExecutor);

    // Create under "alpha"
    let (_, body) = post_json(
        build_router(st.clone()),
        "/alpha/message:send",
        &send_body("ti", "x"),
    )
    .await;
    let tid = body["task"]["id"].as_str().unwrap();

    // Visible under "alpha"
    let (status, _) = get_json(build_router(st.clone()), &format!("/alpha/tasks/{tid}")).await;
    assert_eq!(status, 200);

    // Invisible under default
    let (status, _) = get_json(build_router(st.clone()), &format!("/tasks/{tid}")).await;
    assert_eq!(status, 404);

    // Invisible under "beta"
    let (status, _) = get_json(build_router(st.clone()), &format!("/beta/tasks/{tid}")).await;
    assert_eq!(status, 404);

    // List under "alpha" = 1, default = 0
    let (_, body) = get_json(build_router(st.clone()), "/alpha/tasks").await;
    assert_eq!(body["totalSize"], 1);
    let (_, body) = get_json(build_router(st.clone()), "/tasks").await;
    assert_eq!(body["totalSize"], 0);

    // Cancel under wrong tenant fails
    let (status, _) = post_json(
        build_router(st.clone()),
        &format!("/beta/tasks/{tid}:cancel"),
        "",
    )
    .await;
    assert_eq!(status, 404);
}

// =========================================================
// E2E-16: JSON-RPC parity
// =========================================================

#[tokio::test]
async fn e2e_jsonrpc_parity() {
    let st = state_with(CompletingExecutor);

    // SendMessage via JSON-RPC
    let (_, body) = jrpc_call(
        build_router(st.clone()),
        &jrpc("SendMessage", serde_json::json!({"message":{"messageId":"jp","role":"ROLE_USER","parts":[{"text":"hi"}]}}), 1),
    ).await;
    assert!(body.get("error").is_none());
    let tid = body["result"]["task"]["id"].as_str().unwrap().to_string();

    // GetTask via JSON-RPC
    let (_, body) = jrpc_call(
        build_router(st.clone()),
        &jrpc("GetTask", serde_json::json!({"id": tid}), 2),
    )
    .await;
    assert_eq!(body["result"]["id"], tid);

    // ListTasks via JSON-RPC
    let (_, body) = jrpc_call(
        build_router(st.clone()),
        &jrpc("ListTasks", serde_json::json!({}), 3),
    )
    .await;
    assert!(body["result"]["totalSize"].as_i64().unwrap() >= 1);

    // CancelTask (already completed) → -32002
    let (_, body) = jrpc_call(
        build_router(st.clone()),
        &jrpc("CancelTask", serde_json::json!({"id": tid}), 4),
    )
    .await;
    assert_eq!(body["error"]["code"], -32002);
    let d = &body["error"]["data"][0];
    assert_eq!(d["reason"], "TASK_NOT_CANCELABLE");
    assert_eq!(d["domain"], "a2a-protocol.org");

    // GetTask not found → -32001
    let (_, body) = jrpc_call(
        build_router(st),
        &jrpc("GetTask", serde_json::json!({"id": "nope"}), 5),
    )
    .await;
    assert_eq!(body["error"]["code"], -32001);
    assert_eq!(body["error"]["data"][0]["reason"], "TASK_NOT_FOUND");
}
