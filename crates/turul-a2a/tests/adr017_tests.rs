//! ADR-017 wire-surface tests.
//!
//! Covers three bugs closed by ADR-017 against `core_send_message`:
//!
//! 1. `SendMessageConfiguration.return_immediately = true` rejected on
//!    runtimes whose `RuntimeConfig::supports_return_immediately` is
//!    `false` (AWS Lambda adapter). HTTP 400 / JSON-RPC -32004, no task
//!    persisted.
//! 2. Inline `SendMessageConfiguration.task_push_notification_config`
//!    registered atomically with task creation before executor spawn.
//!    URL validated up front (no task persisted on parse failure).
//!    Storage-side registration failure compensates by transitioning
//!    the task to `FAILED` with an agent `Message` carrying the reason
//!    (no executor spawn).
//! 3. `SendMessageConfiguration.history_length` honoured on both send
//!    response paths, preserving the proto tri-state (proto/a2a.proto
//!    §SendMessageConfiguration): unset = no limit, `0` = empty, `n>0`
//!    = last n.

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
use turul_a2a_types::{Message, Part, Role, Task, TaskState};

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
    }
}

/// AppState with `supports_return_immediately = false`, matching the
/// Lambda adapter's configuration (ADR-017 §Decision Bug 1).
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
// Bug 1 — Lambda gate on return_immediately=true
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
    // google.rpc.ErrorInfo reason is the normative signal per ADR-004.
    let reason = body
        .pointer("/error/details/0/reason")
        .and_then(|v| v.as_str());
    assert_eq!(
        reason,
        Some("UNSUPPORTED_OPERATION"),
        "error reason must be UNSUPPORTED_OPERATION (ADR-004); got body: {body}"
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
        "JSON-RPC error code must be -32004 UnsupportedOperation (ADR-004); got body: {body}"
    );
}

#[tokio::test]
async fn send_message_return_immediately_rejected_persists_no_task() {
    // ADR-017 Bug 1 invariant: the Lambda reject path must abort BEFORE
    // any storage write — no task row created.
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
// Bug 2 — inline push config registration
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
    // ADR-017 Bug 2 invariant (P1 anchor): malformed URL returns 400
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
// Bug 2 — storage-failure compensation
// ---------------------------------------------------------------------------

/// Push storage stub that delegates everything to an inner in-memory
/// backend EXCEPT `create_config`, which always fails. Exercises the
/// ADR-017 §Decision Bug 2 step 5 compensation path.
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

#[tokio::test]
async fn send_message_inline_push_config_storage_failure_compensates() {
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
    };
    let task_storage = state.task_storage.clone();
    let router = build_router(state);

    let body = send_request_body(serde_json::json!({
        "taskPushNotificationConfig": {"url": "https://example.test/webhook-fails"},
    }));
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(body))
        .unwrap();
    let (status, resp_body) = json_response(router, req).await;

    // ADR-017 §Decision Bug 2 step 5 — caller sees the registration error.
    assert!(
        status >= 400,
        "storage failure must surface to the caller, got {status}: {resp_body}"
    );

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

// ---------------------------------------------------------------------------
// Bug 3 — history_length tri-state
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

fn send_with_history_length(hl: Option<i32>, return_immediately: bool) -> String {
    let mut configuration = serde_json::Map::new();
    if return_immediately {
        configuration.insert("returnImmediately".into(), serde_json::Value::Bool(true));
    }
    if let Some(n) = hl {
        configuration.insert("historyLength".into(), serde_json::Value::from(n));
    }
    serde_json::json!({
        "message": message_body("hello"),
        "configuration": configuration,
    })
    .to_string()
}

#[tokio::test]
async fn send_message_history_length_zero_empties_response() {
    // Wire-contract anchor: Some(0) must EMPTY the response history,
    // NOT fall through to "no limit" (proto a2a.proto:150-154;
    // storage/postgres.rs:212-222).
    let router = build_router(base_state());
    let resp = send_and_get_task(router, send_with_history_length(Some(0), false)).await;
    let history = resp
        .pointer("/task/history")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        history.is_empty(),
        "historyLength=0 must return empty history, got {} entries",
        history.len()
    );
}

#[tokio::test]
async fn send_message_history_length_unset_is_unbounded() {
    // Wire-contract anchor: omitting historyLength must NOT truncate
    // (proto: "unset value means the client does not impose any limit").
    let router = build_router(base_state());
    let resp = send_and_get_task(router, send_with_history_length(None, false)).await;
    let history = resp
        .pointer("/task/history")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !history.is_empty(),
        "unset historyLength must return the user message; got empty history"
    );
}

#[tokio::test]
async fn send_message_history_length_one_return_immediately() {
    // historyLength=1 on return_immediately path: one message retained.
    let router = build_router(base_state());
    let resp = send_and_get_task(router, send_with_history_length(Some(1), true)).await;
    let history = resp
        .pointer("/task/history")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        history.len(),
        1,
        "historyLength=1 must trim to one message; got {} entries",
        history.len()
    );
}

#[tokio::test]
async fn send_message_history_length_one_blocking() {
    // historyLength=1 on blocking-send path: same truncation contract.
    let router = build_router(base_state());
    let resp = send_and_get_task(router, send_with_history_length(Some(1), false)).await;
    let history = resp
        .pointer("/task/history")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(history.len(), 1);
}

// Unused helpers silence warnings when the test suite is compiled with
// a subset of features — keep them referenced.
#[allow(dead_code)]
fn _use_unused() {
    let _ = (Part::text("x"), Role::User);
}
