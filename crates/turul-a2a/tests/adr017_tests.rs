//! Wire-surface tests for `core_send_message` invariants.
//!
//! Covers three behaviours:
//!
//! 1. `SendMessageConfiguration.return_immediately = true` rejected on
//!    runtimes whose `RuntimeConfig::supports_return_immediately` is
//!    `false` (AWS Lambda adapter). HTTP 400 / JSON-RPC -32004, no task
//!    persisted.
//! 2. Inline `SendMessageConfiguration.task_push_notification_config`
//!    registered with the task before executor spawn, on blocking,
//!    non-blocking and streaming sends (push configs operate
//!    independently of execution mode, A2A v1.0.1 §3.2.2). URL
//!    validated up front (no task persisted on parse failure).
//!    Storage-side registration failure compensates by transitioning
//!    the task to `FAILED` with an agent `Message` carrying the reason
//!    (no executor spawn) and returns the storage error unchanged.
//! 3. `SendMessageConfiguration.history_length` honoured on both send
//!    response paths for a continuation whose task already holds
//!    several messages, preserving the proto tri-state
//!    (`SendMessageConfiguration.history_length`, a2a.proto:150-154):
//!    unset = no limit, `0` = empty, `n>0` = last n.

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tower::util::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::router::{AppState, build_router};
use turul_a2a::server::RuntimeConfig;
use turul_a2a::server::in_flight::InFlightRegistry;
use turul_a2a::storage::{A2aPushNotificationStorage, A2aStorageError, InMemoryA2aStorage};
use turul_a2a::streaming::TaskEventBroker;
use turul_a2a_types::{Message, Task, TaskState};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Completes the task synchronously via direct-task-mutation. Matches
/// the trivial executor pattern in `transport_compliance_tests.rs`.
struct NoOpExecutor;

#[async_trait]
impl AgentExecutor for NoOpExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
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
        turul_a2a::card_builder::AgentCardBuilder::new("ADR-017 test", "1.0.0")
            .description("ADR-017 wire surface test harness")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build()
            .unwrap()
    }
}

/// Base AppState with in-memory storage.
fn base_state() -> AppState {
    let s = InMemoryA2aStorage::new();
    AppState {
        executor: Arc::new(NoOpExecutor),
        task_storage: Arc::new(s.clone()),
        push_storage: Arc::new(s.clone()),
        event_store: Arc::new(s.clone()),
        atomic_store: Arc::new(s.clone()),
        event_broker: TaskEventBroker::new(),
        middleware_stack: Arc::new(turul_a2a::middleware::MiddlewareStack::new(vec![])),
        runtime_config: RuntimeConfig::default(),
        in_flight: Arc::new(InFlightRegistry::new()),
        cancellation_supervisor: Arc::new(s),
        push_delivery_store: None,
        push_dispatcher: None,
        durable_executor_queue: None,
    }
}

/// AppState with `supports_return_immediately = false`, matching the
/// Lambda adapter's configuration.
fn lambda_like_state() -> AppState {
    let mut state = base_state();
    state.runtime_config.supports_return_immediately = false;
    state
}

async fn json_response(router: Router, req: Request<Body>) -> (u16, serde_json::Value) {
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
    (status, json)
}

fn message_body(text: &str) -> serde_json::Value {
    serde_json::json!({
        "messageId": uuid::Uuid::now_v7().to_string(),
        "role": "ROLE_USER",
        "parts": [{"text": text, "mediaType": "text/plain"}],
    })
}

fn send_request_body(configuration: serde_json::Value) -> String {
    serde_json::json!({
        "message": message_body("hello"),
        "configuration": configuration,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// return_immediately=true refused where the runtime cannot honour it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_message_return_immediately_rejected_http() {
    let router = build_router(lambda_like_state());
    let body = send_request_body(serde_json::json!({"returnImmediately": true}));
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(body))
        .unwrap();

    let (status, body) = json_response(router, req).await;

    assert_eq!(
        status, 400,
        "Lambda-like runtime must refuse returnImmediately=true"
    );
    // google.rpc.ErrorInfo reason is the normative signal — A2A
    // errors carry `ErrorInfo { reason, domain = "a2a-protocol.org" }`
    // on every transport.
    let reason = body
        .pointer("/error/details/0/reason")
        .and_then(|v| v.as_str());
    assert_eq!(
        reason,
        Some("UNSUPPORTED_OPERATION"),
        "error reason must be UNSUPPORTED_OPERATION; got body: {body}"
    );
}

#[tokio::test]
async fn send_message_return_immediately_rejected_jsonrpc() {
    let router = build_router(lambda_like_state());
    let rpc_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "SendMessage",
        "params": {
            "message": message_body("hello"),
            "configuration": {"returnImmediately": true},
        },
    })
    .to_string();
    let req = Request::post("/jsonrpc")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(rpc_body))
        .unwrap();

    let (status, body) = json_response(router, req).await;

    // JSON-RPC always responds HTTP 200; error rides in the payload.
    assert_eq!(status, 200);
    let err_code = body.pointer("/error/code").and_then(|v| v.as_i64());
    assert_eq!(
        err_code,
        Some(-32004),
        "JSON-RPC error code must be -32004 UnsupportedOperation; got body: {body}"
    );
}

#[tokio::test]
async fn send_message_return_immediately_rejected_persists_no_task() {
    // The Lambda reject path must abort BEFORE any storage write — no
    // task row created.
    let state = lambda_like_state();
    let task_storage = state.task_storage.clone();
    let router = build_router(state);

    let body = send_request_body(serde_json::json!({"returnImmediately": true}));
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(body))
        .unwrap();
    let (status, _body) = json_response(router, req).await;
    assert_eq!(status, 400);

    let page = task_storage
        .list_tasks(turul_a2a::storage::TaskFilter {
            tenant: Some(String::new()),
            owner: Some("anonymous".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        page.tasks.is_empty(),
        "rejected returnImmediately send must leave zero tasks; found {}",
        page.tasks.len()
    );
}

// ---------------------------------------------------------------------------
// Inline push config registration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_message_inline_push_config_http() {
    let state = base_state();
    let push_storage = state.push_storage.clone();
    let router = build_router(state);

    let body = send_request_body(serde_json::json!({
        "taskPushNotificationConfig": {
            "url": "https://example.test/webhook",
        }
    }));
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(body))
        .unwrap();
    let (status, resp_body) = json_response(router, req).await;

    assert_eq!(
        status, 200,
        "inline push config send must succeed: {resp_body}"
    );

    let task_id = resp_body
        .pointer("/task/id")
        .and_then(|v| v.as_str())
        .expect("task.id on response")
        .to_string();

    let page = push_storage
        .list_configs("", &task_id, None, None)
        .await
        .unwrap();
    assert_eq!(
        page.configs.len(),
        1,
        "exactly one inline push config must be registered; got {}",
        page.configs.len()
    );
    assert_eq!(page.configs[0].url, "https://example.test/webhook");
    assert_eq!(page.configs[0].task_id, task_id);
}

#[tokio::test]
async fn send_message_inline_push_config_jsonrpc() {
    let state = base_state();
    let push_storage = state.push_storage.clone();
    let router = build_router(state);

    let rpc_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "SendMessage",
        "params": {
            "message": message_body("hello"),
            "configuration": {
                "taskPushNotificationConfig": {"url": "https://example.test/webhook2"},
            },
        },
    })
    .to_string();
    let req = Request::post("/jsonrpc")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(rpc_body))
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);
    let task_id = body
        .pointer("/result/task/id")
        .and_then(|v| v.as_str())
        .expect("JSON-RPC result.task.id")
        .to_string();

    let page = push_storage
        .list_configs("", &task_id, None, None)
        .await
        .unwrap();
    assert_eq!(page.configs.len(), 1);
    assert_eq!(page.configs[0].url, "https://example.test/webhook2");
}

#[tokio::test]
async fn send_message_inline_push_config_invalid_url() {
    // Inline push config URL validation: malformed URL returns 400
    // BEFORE any storage write. No task persisted on this path.
    let state = base_state();
    let task_storage = state.task_storage.clone();
    let router = build_router(state);

    let body = send_request_body(serde_json::json!({
        "taskPushNotificationConfig": {"url": "not a url"},
    }));
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(body))
        .unwrap();
    let (status, _body) = json_response(router, req).await;

    assert_eq!(status, 400, "malformed URL must return 400");

    let page = task_storage
        .list_tasks(turul_a2a::storage::TaskFilter {
            tenant: Some(String::new()),
            owner: Some("anonymous".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        page.tasks.is_empty(),
        "URL parse failure must leave zero tasks; found {}",
        page.tasks.len()
    );
}

#[tokio::test]
async fn send_message_inline_push_config_missing_url() {
    // Empty url string must also be rejected up front.
    let state = base_state();
    let task_storage = state.task_storage.clone();
    let router = build_router(state);

    let body = send_request_body(serde_json::json!({
        "taskPushNotificationConfig": {"url": ""},
    }));
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(body))
        .unwrap();
    let (status, _body) = json_response(router, req).await;
    assert_eq!(status, 400);

    let page = task_storage
        .list_tasks(turul_a2a::storage::TaskFilter {
            tenant: Some(String::new()),
            owner: Some("anonymous".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(page.tasks.is_empty());
}

// ---------------------------------------------------------------------------
// Inline push config — storage-failure compensation
// ---------------------------------------------------------------------------

/// Push storage stub that delegates everything to an inner in-memory
/// backend EXCEPT `create_config`, which always fails. Exercises the
/// storage-failure compensation path: the task transitions to FAILED
/// with an agent message naming the reason; the executor never runs.
struct FailingCreatePushStorage {
    inner: Arc<InMemoryA2aStorage>,
}

#[async_trait]
impl A2aPushNotificationStorage for FailingCreatePushStorage {
    fn backend_name(&self) -> &'static str {
        self.inner.backend_name()
    }

    async fn create_config(
        &self,
        _tenant: &str,
        _config: turul_a2a_proto::TaskPushNotificationConfig,
    ) -> Result<turul_a2a_proto::TaskPushNotificationConfig, A2aStorageError> {
        Err(A2aStorageError::DatabaseError(
            "simulated push storage outage".into(),
        ))
    }

    async fn get_config(
        &self,
        tenant: &str,
        task_id: &str,
        config_id: &str,
    ) -> Result<Option<turul_a2a_proto::TaskPushNotificationConfig>, A2aStorageError> {
        self.inner.get_config(tenant, task_id, config_id).await
    }

    async fn list_configs(
        &self,
        tenant: &str,
        task_id: &str,
        page_token: Option<&str>,
        page_size: Option<i32>,
    ) -> Result<turul_a2a::storage::PushConfigListPage, A2aStorageError> {
        self.inner
            .list_configs(tenant, task_id, page_token, page_size)
            .await
    }

    async fn list_configs_eligible_at_event(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        page_token: Option<&str>,
        page_size: Option<i32>,
    ) -> Result<turul_a2a::storage::PushConfigListPage, A2aStorageError> {
        self.inner
            .list_configs_eligible_at_event(tenant, task_id, event_sequence, page_token, page_size)
            .await
    }

    async fn delete_config(
        &self,
        tenant: &str,
        task_id: &str,
        config_id: &str,
    ) -> Result<(), A2aStorageError> {
        self.inner.delete_config(tenant, task_id, config_id).await
    }
}

async fn inline_push_config_storage_failure_compensates(send_path: &str) {
    let inner = Arc::new(InMemoryA2aStorage::new());
    let failing_push = Arc::new(FailingCreatePushStorage {
        inner: inner.clone(),
    });
    let in_flight = Arc::new(InFlightRegistry::new());
    let state = AppState {
        executor: Arc::new(NoOpExecutor),
        task_storage: inner.clone(),
        push_storage: failing_push,
        event_store: inner.clone(),
        atomic_store: inner.clone(),
        event_broker: TaskEventBroker::new(),
        middleware_stack: Arc::new(turul_a2a::middleware::MiddlewareStack::new(vec![])),
        runtime_config: RuntimeConfig::default(),
        in_flight: in_flight.clone(),
        cancellation_supervisor: inner.clone(),
        push_delivery_store: None,
        push_dispatcher: None,
        durable_executor_queue: None,
    };
    let task_storage = state.task_storage.clone();
    let router = build_router(state);

    let body = send_request_body(serde_json::json!({
        "taskPushNotificationConfig": {"url": "https://example.test/webhook-fails"},
    }));
    let req = Request::post(send_path)
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(body))
        .unwrap();
    let (status, resp_body) = json_response(router, req).await;

    // The caller sees the original registration error, unchanged.
    let expected = A2aError::from(A2aStorageError::DatabaseError(
        "simulated push storage outage".into(),
    ));
    assert_eq!(status, expected.http_status(), "{send_path}: {resp_body}");
    assert_eq!(resp_body, expected.to_http_error_body(), "{send_path}");

    // The task exists, in FAILED state, with a reason message.
    let page = task_storage
        .list_tasks(turul_a2a::storage::TaskFilter {
            tenant: Some(String::new()),
            owner: Some("anonymous".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.tasks.len(), 1, "exactly one (FAILED) task must exist");
    let task = &page.tasks[0];
    let state = task
        .status()
        .expect("task has status")
        .state()
        .expect("decodable state");
    assert_eq!(
        state,
        TaskState::Failed,
        "task must be FAILED after compensation"
    );

    let status_msg = task
        .status()
        .expect("task has status")
        .as_proto()
        .message
        .clone()
        .expect("FAILED status must carry a reason Message");
    let text = status_msg
        .parts
        .iter()
        .filter_map(|p| match &p.content {
            Some(turul_a2a_proto::part::Content::Text(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        text.contains("inline push notification config registration failed"),
        "reason message must name the failure; got {text:?}"
    );

    // Executor was never spawned.
    assert_eq!(
        in_flight.len(),
        0,
        "no executor must be registered after compensation"
    );
}

#[tokio::test]
async fn send_message_inline_push_config_storage_failure_compensates() {
    inline_push_config_storage_failure_compensates("/message:send").await;
}

#[tokio::test]
async fn streaming_send_inline_push_config_storage_failure_compensates() {
    inline_push_config_storage_failure_compensates("/message:stream").await;
}

#[tokio::test]
async fn streaming_send_inline_push_config_registers() {
    let state = base_state();
    let push_storage = state.push_storage.clone();
    let task_storage = state.task_storage.clone();
    let router = build_router(state);

    let body = send_request_body(serde_json::json!({
        "taskPushNotificationConfig": {"url": "https://example.test/stream-hook"},
    }));
    let req = Request::post("/message:stream")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(body))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let _ = resp.into_body().collect().await.unwrap();

    let page = task_storage
        .list_tasks(turul_a2a::storage::TaskFilter {
            tenant: Some(String::new()),
            owner: Some("anonymous".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.tasks.len(), 1);
    let task_id = page.tasks[0].id().to_string();
    let configs = push_storage
        .list_configs("", &task_id, None, None)
        .await
        .unwrap();
    assert_eq!(configs.configs.len(), 1);
    assert_eq!(configs.configs[0].url, "https://example.test/stream-hook");
    assert_eq!(configs.configs[0].task_id, task_id);
}

#[tokio::test]
async fn streaming_send_inline_push_config_invalid_url_persists_no_task() {
    for url in ["not a url", ""] {
        let state = base_state();
        let task_storage = state.task_storage.clone();
        let router = build_router(state);

        let body = send_request_body(serde_json::json!({
            "taskPushNotificationConfig": {"url": url},
        }));
        let req = Request::post("/message:stream")
            .header("content-type", "application/json")
            .header("a2a-version", "1.0")
            .body(Body::from(body))
            .unwrap();
        let (status, _body) = json_response(router, req).await;
        assert_eq!(status, 400, "url {url:?}");

        let page = task_storage
            .list_tasks(turul_a2a::storage::TaskFilter {
                tenant: Some(String::new()),
                owner: Some("anonymous".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(page.tasks.is_empty(), "url {url:?}: no task persisted");
    }
}

// ---------------------------------------------------------------------------
// history_length tri-state
// ---------------------------------------------------------------------------

async fn send_and_get_task(router: Router, body: String) -> serde_json::Value {
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(body))
        .unwrap();
    let (status, resp) = json_response(router, req).await;
    assert_eq!(status, 200, "send expected to succeed: {resp}");
    resp
}

/// Asks for more input on every turn except one whose text is
/// "finish", which completes. Lets a test build up a task history
/// across several turns.
struct MultiTurnExecutor;

#[async_trait]
impl AgentExecutor for MultiTurnExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let state = if msg.joined_text() == "finish" {
            turul_a2a_proto::TaskState::Completed
        } else {
            turul_a2a_proto::TaskState::InputRequired
        };
        let mut proto = task.as_proto().clone();
        proto.status = Some(turul_a2a_proto::TaskStatus {
            state: state.into(),
            message: None,
            timestamp: None,
        });
        *task = Task::try_from(proto).unwrap();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        NoOpExecutor.agent_card()
    }
}

fn turn_body(
    task_id: Option<&str>,
    text: &str,
    hl: Option<i32>,
    return_immediately: bool,
) -> String {
    let mut message = message_body(text);
    if let Some(id) = task_id {
        message["taskId"] = serde_json::Value::from(id);
    }
    let mut configuration = serde_json::Map::new();
    if return_immediately {
        configuration.insert("returnImmediately".into(), serde_json::Value::Bool(true));
    }
    if let Some(n) = hl {
        configuration.insert("historyLength".into(), serde_json::Value::from(n));
    }
    serde_json::json!({"message": message, "configuration": configuration}).to_string()
}

/// Run two INPUT_REQUIRED turns, then a third turn with `hl` and
/// `return_immediately`. Returns the third response's history and
/// the task's full stored history afterwards.
async fn continuation_history(
    hl: Option<i32>,
    return_immediately: bool,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let mut state = base_state();
    state.executor = Arc::new(MultiTurnExecutor);
    let task_storage = state.task_storage.clone();
    let in_flight = state.in_flight.clone();
    let router = build_router(state);

    let first = send_and_get_task(router.clone(), turn_body(None, "one", None, false)).await;
    let task_id = first
        .pointer("/task/id")
        .and_then(|v| v.as_str())
        .expect("task id")
        .to_string();
    for text in ["two", "finish"] {
        // A follow-up is accepted once the previous turn's executor
        // has returned.
        let start = std::time::Instant::now();
        while !in_flight.is_empty() {
            assert!(start.elapsed() < std::time::Duration::from_secs(2));
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let (hl, ri) = if text == "finish" {
            (hl, return_immediately)
        } else {
            (None, false)
        };
        let resp = send_and_get_task(router.clone(), turn_body(Some(&task_id), text, hl, ri)).await;
        if text == "finish" {
            let history = resp
                .pointer("/task/history")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let full = task_storage
                .get_task("", &task_id, "anonymous", None)
                .await
                .unwrap()
                .expect("task")
                .history()
                .iter()
                .map(|m| serde_json::to_value(m).unwrap())
                .collect::<Vec<_>>();
            assert!(full.len() >= 3, "three user turns are in the history");
            return (history, full);
        }
    }
    unreachable!()
}

#[tokio::test]
async fn send_message_history_length_zero_empties_response() {
    // Some(0) must EMPTY the response history, not fall through to
    // "no limit" (a2a.proto:150-154).
    let (history, _full) = continuation_history(Some(0), false).await;
    assert!(history.is_empty(), "got {} entries", history.len());
}

#[tokio::test]
async fn send_message_history_length_unset_is_unbounded() {
    // "An unset value means the client does not impose any limit."
    let (history, full) = continuation_history(None, false).await;
    assert_eq!(history, full, "the whole history is returned");
}

#[tokio::test]
async fn send_message_history_length_one_return_immediately() {
    let (history, full) = continuation_history(Some(1), true).await;
    assert_eq!(history.len(), 1);
    let stored_at_return = &history[0];
    assert!(
        full.contains(stored_at_return),
        "the returned message comes from the task history"
    );
    assert_eq!(
        stored_at_return
            .pointer("/parts/0/text")
            .and_then(|v| v.as_str()),
        Some("finish"),
        "the most recent message is kept: {stored_at_return}"
    );
}

#[tokio::test]
async fn send_message_history_length_one_blocking() {
    let (history, full) = continuation_history(Some(1), false).await;
    assert_eq!(history.len(), 1);
    assert_eq!(&history[0], full.last().unwrap(), "the most recent message");
}

// ---------------------------------------------------------------------------
// Inline push config over gRPC — the same shared registration path.
// ---------------------------------------------------------------------------

#[cfg(feature = "grpc")]
mod grpc_inline_push_config {
    use super::*;
    use tokio_stream::StreamExt;
    use turul_a2a_proto::grpc::A2aServiceClient;

    async fn serve() -> (
        A2aServiceClient<tonic::transport::Channel>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().unwrap();
        let server = turul_a2a::server::A2aServer::builder()
            .executor(NoOpExecutor)
            .storage(InMemoryA2aStorage::new())
            .bind(([127, 0, 0, 1], 0))
            .build()
            .expect("build");
        let router = server.into_tonic_router();
        let handle = tokio::spawn(async move {
            let _ = router
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await;
        });
        let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}")).unwrap();
        let start = std::time::Instant::now();
        let channel = loop {
            match endpoint.connect().await {
                Ok(c) => break c,
                Err(e) if start.elapsed() < std::time::Duration::from_secs(5) => {
                    let _ = e;
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(e) => panic!("connect: {e}"),
            }
        };
        (A2aServiceClient::new(channel), handle)
    }

    fn request(url: &str) -> turul_a2a_proto::SendMessageRequest {
        turul_a2a_proto::SendMessageRequest {
            message: Some(turul_a2a_proto::Message {
                message_id: uuid::Uuid::now_v7().to_string(),
                role: turul_a2a_proto::Role::User.into(),
                parts: vec![turul_a2a_proto::Part {
                    content: Some(turul_a2a_proto::part::Content::Text("hello".into())),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            configuration: Some(turul_a2a_proto::SendMessageConfiguration {
                task_push_notification_config: Some(turul_a2a_proto::TaskPushNotificationConfig {
                    url: url.into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    async fn registered_urls(
        client: &mut A2aServiceClient<tonic::transport::Channel>,
        task_id: &str,
    ) -> Vec<String> {
        client
            .list_task_push_notification_configs(tonic::Request::new(
                turul_a2a_proto::ListTaskPushNotificationConfigsRequest {
                    task_id: task_id.into(),
                    ..Default::default()
                },
            ))
            .await
            .expect("list configs")
            .into_inner()
            .configs
            .into_iter()
            .map(|c| c.url)
            .collect()
    }

    #[tokio::test]
    async fn send_message_inline_push_config_grpc() {
        let (mut client, handle) = serve().await;
        let resp = client
            .send_message(tonic::Request::new(request(
                "https://example.test/grpc-hook",
            )))
            .await
            .expect("send")
            .into_inner();
        let task_id = match resp.payload {
            Some(turul_a2a_proto::send_message_response::Payload::Task(t)) => t.id,
            other => panic!("expected a task, got {other:?}"),
        };
        assert_eq!(
            registered_urls(&mut client, &task_id).await,
            vec!["https://example.test/grpc-hook".to_string()]
        );
        handle.abort();
    }

    #[tokio::test]
    async fn send_streaming_message_inline_push_config_grpc() {
        let (mut client, handle) = serve().await;
        let mut stream = client
            .send_streaming_message(tonic::Request::new(request(
                "https://example.test/grpc-stream-hook",
            )))
            .await
            .expect("stream")
            .into_inner();
        let first = stream.next().await.expect("first item").expect("ok item");
        let task_id = match first.payload {
            Some(turul_a2a_proto::stream_response::Payload::Task(t)) => t.id,
            other => panic!("stream opens with the task, got {other:?}"),
        };
        while stream.next().await.is_some() {}
        assert_eq!(
            registered_urls(&mut client, &task_id).await,
            vec!["https://example.test/grpc-stream-hook".to_string()]
        );
        handle.abort();
    }
}
