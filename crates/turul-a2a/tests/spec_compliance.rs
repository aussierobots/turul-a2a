//! A2A v1.0 core-operation compliance walkthrough.
//!
//! This suite exercises every core RPC defined in the A2A v1.0
//! specification (normative proto source at
//! `crates/turul-a2a-proto/proto/a2a.proto`) on both transport
//! bindings the spec requires servers to expose:
//!
//! - **HTTP + JSON** — per-RPC `google.api.http` bindings.
//! - **JSON-RPC 2.0** — `POST /jsonrpc` with PascalCase method names.
//!
//! Per-RPC coverage matrix:
//!
//! | RPC                               | HTTP | JSON-RPC |
//! |-----------------------------------|------|----------|
//! | AgentCard (`/.well-known/...`)    |  ✅  |  n/a     |
//! | GetExtendedAgentCard              |  ✅  |  ✅      |
//! | SendMessage                       |  ✅  |  ✅      |
//! | SendStreamingMessage              |  ✅  |  see SSE suite |
//! | GetTask                           |  ✅  |  ✅      |
//! | ListTasks                         |  ✅  |  ✅      |
//! | CancelTask                        |  ✅  |  ✅      |
//! | SubscribeToTask (terminal-400)    |  ✅  |  see SSE suite |
//! | CreateTaskPushNotificationConfig  |  ✅  |  ✅      |
//! | GetTaskPushNotificationConfig     |  ✅  |  ✅      |
//! | ListTaskPushNotificationConfigs   |  ✅  |  ✅      |
//! | DeleteTaskPushNotificationConfig  |  ✅  |  ✅      |
//!
//! AgentCard discovery is HTTP-only by spec — there is no JSON-RPC
//! equivalent for the `.well-known` URL. Live-subscription sequencing
//! for `SendStreamingMessage` + `SubscribeToTask` is exercised in the
//! dedicated streaming suites (`sse_tests.rs`, `d3_streaming_tests.rs`);
//! this file asserts only the "terminal-task subscribe returns
//! UnsupportedOperationError" contract (§3.1.6).
//!
//! For each operation we assert:
//! - the wire path + verb match the proto's `google.api.http` option,
//! - the response envelope matches the spec (pbjson camelCase,
//!   required fields present, JSON-RPC responses preserve the
//!   request `id`),
//! - error paths carry the spec-mandated HTTP status and JSON-RPC
//!   code, plus a `google.rpc.ErrorInfo` under `error.details`
//!   (HTTP, AIP-193) or `error.data` (JSON-RPC 2.0) — spec §5.4 /
//!   ADR-004.
//!
//! The tests stand up a single `A2aServer` router via `into_router()`
//! so the compliance harness is hermetic — no external ports, no
//! wiremock. Push-notification *delivery* wire mechanics are out of
//! scope here and are covered by `push_dispatcher_server_tests.rs`;
//! this file exercises the push *CRUD* surface.

use std::collections::HashMap;

use async_trait::async_trait;
use axum::body::Body;
use http::{Method, Request};
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::server::A2aServer;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Message, Task};

// ---------------------------------------------------------------------------
// Compliance-agent harness
// ---------------------------------------------------------------------------

/// Minimal executor that completes any received task synchronously —
/// the equivalent of the A2A reference "echo" agent. Deterministic
/// enough to let the compliance suite assert wire shapes.
struct ComplianceAgent;

#[async_trait]
impl AgentExecutor for ComplianceAgent {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let mut p = task.as_proto().clone();
        p.status = Some(turul_a2a_proto::TaskStatus {
            state: turul_a2a_proto::TaskState::Completed.into(),
            message: None,
            timestamp: None,
        });
        *task = Task::try_from(p).unwrap();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            name: "turul-a2a compliance reference".into(),
            description: "A2A v1.0 compliance walkthrough agent".into(),
            supported_interfaces: vec![turul_a2a_proto::AgentInterface {
                url: "http://127.0.0.1:0".into(),
                protocol_binding: "HTTP+JSON".into(),
                tenant: String::new(),
                protocol_version: "1.0".into(),
            }],
            provider: None,
            version: "1.0.0".into(),
            documentation_url: None,
            capabilities: Some(turul_a2a_proto::AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(true),
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

    fn extended_agent_card(
        &self,
        _claims: Option<&serde_json::Value>,
    ) -> Option<turul_a2a_proto::AgentCard> {
        // Extended card surfaces the same shape as the public card
        // but with a separate marker field so tests can distinguish.
        let mut card = self.agent_card();
        card.description = "turul-a2a compliance reference (extended)".into();
        Some(card)
    }
}

fn compliance_router() -> axum::Router {
    A2aServer::builder()
        .executor(ComplianceAgent)
        .storage(InMemoryA2aStorage::new())
        .build()
        .expect("server build")
        .into_router()
}

fn http_send_body(message_id: &str, text: &str) -> String {
    serde_json::json!({
        "message": {
            "messageId": message_id,
            "role": "ROLE_USER",
            "parts": [{"text": text}],
        }
    })
    .to_string()
}

async fn http_call(
    router: axum::Router,
    method: Method,
    uri: &str,
    body: Option<String>,
) -> (u16, http::HeaderMap, serde_json::Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("A2A-Version", "1.0");
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let req = req
        .body(body.map(Body::from).unwrap_or_else(Body::empty))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, headers, json)
}

fn jsonrpc_body(method: &str, params: serde_json::Value, id: i64) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": id,
    })
    .to_string()
}

async fn jsonrpc_call(
    router: axum::Router,
    method: &str,
    params: serde_json::Value,
    id: i64,
) -> serde_json::Value {
    let (_, _, json) = http_call(
        router,
        Method::POST,
        "/jsonrpc",
        Some(jsonrpc_body(method, params, id)),
    )
    .await;
    json
}

/// Spec §5.4 / ADR-004: every A2A error response carries a
/// `google.rpc.ErrorInfo` record with `reason` + `domain`.
///
/// The exact slot differs by transport:
/// - HTTP (AIP-193): `error.details: [{"@type": "...ErrorInfo", ...}]`
/// - JSON-RPC 2.0: `error.data: {"@type": "...ErrorInfo", ...}`
///
/// Both are normative under A2A v1.0 — one per transport — so the
/// compliance check accepts either shape.
fn assert_has_error_info(json: &serde_json::Value, expected_reason: &str) {
    let error = json.get("error").expect("error envelope present");
    let info: &serde_json::Value = if let Some(details) =
        error.get("details").and_then(|d| d.as_array())
    {
        details
            .iter()
            .find(|entry| {
                entry.get("@type").and_then(|t| t.as_str())
                    == Some("type.googleapis.com/google.rpc.ErrorInfo")
            })
            .expect("google.rpc.ErrorInfo in error.details (HTTP)")
    } else if let Some(data) = error.get("data") {
        assert_eq!(
            data.get("@type").and_then(|t| t.as_str()),
            Some("type.googleapis.com/google.rpc.ErrorInfo"),
            "JSON-RPC error.data must be a google.rpc.ErrorInfo"
        );
        data
    } else {
        panic!(
            "error must carry ErrorInfo via either `details` (HTTP) or `data` (JSON-RPC); got {json}"
        )
    };
    assert_eq!(
        info.get("reason").and_then(|v| v.as_str()),
        Some(expected_reason),
        "ErrorInfo.reason mismatch; full error: {json}"
    );
    assert_eq!(
        info.get("domain").and_then(|v| v.as_str()),
        Some("a2a-protocol.org"),
        "ErrorInfo.domain must be `a2a-protocol.org`"
    );
}

// ---------------------------------------------------------------------------
// Discovery: Agent Card (public) + Extended Agent Card (auth-gated)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn public_agent_card_is_served_unauthenticated() {
    let router = compliance_router();
    let (status, _, card) =
        http_call(router, Method::GET, "/.well-known/agent-card.json", None).await;
    assert_eq!(status, 200);
    // Required fields per proto AgentCard.
    assert!(card.get("name").and_then(|v| v.as_str()).is_some());
    assert!(card.get("version").and_then(|v| v.as_str()).is_some());
    // pbjson camelCase: capabilities present even when partial.
    let caps = card
        .get("capabilities")
        .expect("capabilities field present (pbjson camelCase)");
    // `streaming` uses camelCase (field name is already one word).
    assert_eq!(caps.get("streaming"), Some(&serde_json::Value::Bool(true)));
}

#[tokio::test]
async fn extended_agent_card_returns_executor_supplied_card() {
    let router = compliance_router();
    let (status, _, card) = http_call(router, Method::GET, "/extendedAgentCard", None).await;
    assert_eq!(status, 200);
    // Our ComplianceAgent differentiates the extended card via the
    // description field; any shape change is a spec-compliance
    // regression.
    assert_eq!(
        card.get("description").and_then(|v| v.as_str()),
        Some("turul-a2a compliance reference (extended)")
    );
}

// ---------------------------------------------------------------------------
// SendMessage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_message_http_returns_task_with_required_fields() {
    let router = compliance_router();
    let (status, _, resp) = http_call(
        router,
        Method::POST,
        "/message:send",
        Some(http_send_body("m-compliance-1", "hello")),
    )
    .await;
    assert_eq!(status, 200);
    // SendMessageResponse wraps a `task` or `message` per the proto.
    let task = resp
        .get("task")
        .expect("SendMessageResponse must carry `task` for task-producing agents");
    let id = task.get("id").and_then(|v| v.as_str()).expect("task.id");
    assert!(!id.is_empty());
    // status.state uses proto enum wire name.
    let state = task
        .get("status")
        .and_then(|s| s.get("state"))
        .and_then(|v| v.as_str())
        .expect("task.status.state");
    assert!(
        state.starts_with("TASK_STATE_"),
        "proto enum wire form required, got {state}"
    );
}

#[tokio::test]
async fn send_message_jsonrpc_returns_same_shape_as_http() {
    let router = compliance_router();
    let resp = jsonrpc_call(
        router,
        "SendMessage",
        serde_json::json!({
            "message": {
                "messageId": "m-compliance-2",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}],
            }
        }),
        7,
    )
    .await;
    assert_eq!(
        resp.get("jsonrpc").and_then(|v| v.as_str()),
        Some("2.0"),
        "JSON-RPC response must carry jsonrpc=\"2.0\""
    );
    assert_eq!(resp.get("id").and_then(|v| v.as_i64()), Some(7));
    let task = resp
        .get("result")
        .and_then(|r| r.get("task"))
        .expect("result.task present");
    assert!(task.get("id").and_then(|v| v.as_str()).is_some());
}

// ---------------------------------------------------------------------------
// GetTask
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_task_http_and_jsonrpc_both_return_the_same_task() {
    let router = compliance_router();

    // Create a task.
    let (_, _, send_resp) = http_call(
        router.clone(),
        Method::POST,
        "/message:send",
        Some(http_send_body("m-compliance-get", "hi")),
    )
    .await;
    let task_id = send_resp
        .get("task")
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .expect("task.id from send")
        .to_string();

    // HTTP GET.
    let (status, _, http_task) = http_call(
        router.clone(),
        Method::GET,
        &format!("/tasks/{task_id}"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        http_task.get("id").and_then(|v| v.as_str()),
        Some(task_id.as_str())
    );

    // JSON-RPC.
    let jrpc = jsonrpc_call(router, "GetTask", serde_json::json!({"id": task_id}), 42).await;
    let jrpc_task = jrpc.get("result").expect("result present");
    assert_eq!(
        jrpc_task.get("id").and_then(|v| v.as_str()),
        Some(task_id.as_str()),
        "JSON-RPC GetTask must return the same task as HTTP GetTask"
    );
}

#[tokio::test]
async fn get_task_not_found_maps_to_404_and_minus_32001() {
    let router = compliance_router();

    // HTTP.
    let (status, _, http_err) =
        http_call(router.clone(), Method::GET, "/tasks/does-not-exist", None).await;
    assert_eq!(status, 404, "TaskNotFoundError HTTP status per ADR-004");
    assert_has_error_info(&http_err, "TASK_NOT_FOUND");

    // JSON-RPC.
    let jrpc = jsonrpc_call(
        router,
        "GetTask",
        serde_json::json!({"id": "does-not-exist"}),
        99,
    )
    .await;
    let code = jrpc
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|v| v.as_i64())
        .expect("error.code");
    assert_eq!(
        code, -32001,
        "TaskNotFoundError JSON-RPC code per spec §5.4"
    );
    assert_has_error_info(&jrpc, "TASK_NOT_FOUND");
}

// ---------------------------------------------------------------------------
// ListTasks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_tasks_http_carries_required_pagination_fields() {
    let router = compliance_router();
    // Seed a task so the list is non-empty.
    let _ = http_call(
        router.clone(),
        Method::POST,
        "/message:send",
        Some(http_send_body("m-compliance-list", "first")),
    )
    .await;

    let (status, _, page) = http_call(router, Method::GET, "/tasks", None).await;
    assert_eq!(status, 200);
    // ListTasksResponse REQUIRED fields per spec.
    assert!(page.get("tasks").and_then(|v| v.as_array()).is_some());
    assert!(page.get("nextPageToken").is_some());
    assert!(page.get("pageSize").and_then(|v| v.as_i64()).is_some());
    assert!(page.get("totalSize").and_then(|v| v.as_i64()).is_some());
}

// ---------------------------------------------------------------------------
// CancelTask
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_terminal_task_returns_409_and_task_not_cancelable_error() {
    let router = compliance_router();
    // ComplianceAgent drives tasks to COMPLETED synchronously, so
    // any task we create is immediately terminal — perfect for
    // exercising the TaskNotCancelableError path.
    let (_, _, send_resp) = http_call(
        router.clone(),
        Method::POST,
        "/message:send",
        Some(http_send_body("m-compliance-cancel", "x")),
    )
    .await;
    let task_id = send_resp
        .get("task")
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // HTTP.
    let (status, _, http_err) = http_call(
        router.clone(),
        Method::POST,
        &format!("/tasks/{task_id}:cancel"),
        None,
    )
    .await;
    assert_eq!(
        status, 409,
        "TaskNotCancelableError HTTP status per ADR-004"
    );
    assert_has_error_info(&http_err, "TASK_NOT_CANCELABLE");

    // JSON-RPC.
    let jrpc = jsonrpc_call(router, "CancelTask", serde_json::json!({"id": task_id}), 11).await;
    let code = jrpc
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|v| v.as_i64());
    assert_eq!(
        code,
        Some(-32002),
        "TaskNotCancelableError JSON-RPC code per spec §5.4"
    );
    assert_has_error_info(&jrpc, "TASK_NOT_CANCELABLE");
}

#[tokio::test]
async fn cancel_nonexistent_task_returns_404_not_cancel_error() {
    let router = compliance_router();
    let (status, _, err) = http_call(router, Method::POST, "/tasks/ghost:cancel", None).await;
    assert_eq!(status, 404);
    assert_has_error_info(&err, "TASK_NOT_FOUND");
}

// ---------------------------------------------------------------------------
// Push-config CRUD
//
// CreateTaskPushNotificationConfig / GetTaskPushNotificationConfig /
// ListTaskPushNotificationConfigs / DeleteTaskPushNotificationConfig.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn push_config_crud_roundtrip_over_jsonrpc() {
    let router = compliance_router();
    let (_, _, send_resp) = http_call(
        router.clone(),
        Method::POST,
        "/message:send",
        Some(http_send_body("m-compliance-jrpc-push", "x")),
    )
    .await;
    let task_id = send_resp
        .get("task")
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // Create via JSON-RPC.
    let created = jsonrpc_call(
        router.clone(),
        "CreateTaskPushNotificationConfig",
        serde_json::json!({
            "id": "cfg-jrpc",
            "taskId": task_id,
            "url": "https://example.com/hook",
            "token": "tok",
            "authentication": {"scheme": "Bearer", "credentials": "c"},
        }),
        101,
    )
    .await;
    assert_eq!(
        created
            .get("result")
            .and_then(|r| r.get("id"))
            .and_then(|v| v.as_str()),
        Some("cfg-jrpc"),
        "JSON-RPC create must return the registered config"
    );

    // Get via JSON-RPC.
    let got = jsonrpc_call(
        router.clone(),
        "GetTaskPushNotificationConfig",
        serde_json::json!({"taskId": task_id, "id": "cfg-jrpc"}),
        102,
    )
    .await;
    assert_eq!(
        got.get("result")
            .and_then(|r| r.get("url"))
            .and_then(|v| v.as_str()),
        Some("https://example.com/hook")
    );

    // List via JSON-RPC.
    let listed = jsonrpc_call(
        router.clone(),
        "ListTaskPushNotificationConfigs",
        serde_json::json!({"taskId": task_id}),
        103,
    )
    .await;
    let configs = listed
        .get("result")
        .and_then(|r| r.get("configs"))
        .and_then(|v| v.as_array())
        .expect("configs array");
    assert_eq!(configs.len(), 1);

    // Delete via JSON-RPC.
    let deleted = jsonrpc_call(
        router.clone(),
        "DeleteTaskPushNotificationConfig",
        serde_json::json!({"taskId": task_id, "id": "cfg-jrpc"}),
        104,
    )
    .await;
    assert!(
        deleted.get("result").is_some(),
        "JSON-RPC delete must return a result envelope"
    );

    // Confirm via JSON-RPC List that the config is gone.
    let after_delete = jsonrpc_call(
        router,
        "ListTaskPushNotificationConfigs",
        serde_json::json!({"taskId": task_id}),
        105,
    )
    .await;
    let configs = after_delete
        .get("result")
        .and_then(|r| r.get("configs"))
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(configs.is_empty());
}

#[tokio::test]
async fn extended_agent_card_available_over_jsonrpc() {
    let router = compliance_router();
    let resp = jsonrpc_call(router, "GetExtendedAgentCard", serde_json::json!({}), 77).await;
    // The compliance agent returns an extended card with the
    // distinguishing description string.
    assert_eq!(
        resp.get("result")
            .and_then(|r| r.get("description"))
            .and_then(|v| v.as_str()),
        Some("turul-a2a compliance reference (extended)"),
        "JSON-RPC GetExtendedAgentCard must return the executor-supplied extended card"
    );
}

#[tokio::test]
async fn subscribe_to_task_terminal_returns_unsupported_operation() {
    // Per A2A spec §3.1.6 (and ADR on subscribe): subscribing to an
    // already-terminal task MUST return UnsupportedOperationError.
    // `ComplianceAgent` completes synchronously, so the task we
    // create via send is immediately terminal.
    let router = compliance_router();
    let (_, _, send_resp) = http_call(
        router.clone(),
        Method::POST,
        "/message:send",
        Some(http_send_body("m-compliance-sub", "x")),
    )
    .await;
    let task_id = send_resp
        .get("task")
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    let (status, _, err) = http_call(
        router,
        Method::GET,
        &format!("/tasks/{task_id}:subscribe"),
        None,
    )
    .await;
    assert_eq!(
        status, 400,
        "UnsupportedOperationError HTTP status per ADR-004"
    );
    assert_has_error_info(&err, "UNSUPPORTED_OPERATION");
}

#[tokio::test]
async fn push_config_crud_roundtrip_over_http() {
    let router = compliance_router();
    let (_, _, send_resp) = http_call(
        router.clone(),
        Method::POST,
        "/message:send",
        Some(http_send_body("m-compliance-push", "x")),
    )
    .await;
    let task_id = send_resp
        .get("task")
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // Create.
    let body = serde_json::json!({
        "id": "cfg-compliance",
        "taskId": task_id,
        "url": "https://example.com/hook",
        "token": "tok",
        "authentication": {"scheme": "Bearer", "credentials": "c"},
    });
    let (status, _, created) = http_call(
        router.clone(),
        Method::POST,
        &format!("/tasks/{task_id}/pushNotificationConfigs"),
        Some(body.to_string()),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        created.get("id").and_then(|v| v.as_str()),
        Some("cfg-compliance")
    );

    // Get.
    let (status, _, got) = http_call(
        router.clone(),
        Method::GET,
        &format!("/tasks/{task_id}/pushNotificationConfigs/cfg-compliance"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        got.get("url").and_then(|v| v.as_str()),
        Some("https://example.com/hook")
    );

    // List.
    let (status, _, list) = http_call(
        router.clone(),
        Method::GET,
        &format!("/tasks/{task_id}/pushNotificationConfigs"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    let configs = list
        .get("configs")
        .and_then(|v| v.as_array())
        .expect("configs array");
    assert_eq!(configs.len(), 1);

    // Delete (idempotent; re-delete returns success).
    let (status, _, _) = http_call(
        router.clone(),
        Method::DELETE,
        &format!("/tasks/{task_id}/pushNotificationConfigs/cfg-compliance"),
        None,
    )
    .await;
    assert_eq!(status, 200);

    // Confirm deletion via List.
    let (_, _, list) = http_call(
        router,
        Method::GET,
        &format!("/tasks/{task_id}/pushNotificationConfigs"),
        None,
    )
    .await;
    let configs = list.get("configs").and_then(|v| v.as_array()).unwrap();
    assert!(configs.is_empty(), "Delete must remove the config");
}

// ---------------------------------------------------------------------------
// A2A-Version header contract (ADR-004)
//
// Two explicit tests, each pinning one behaviour so a regression
// flips exactly one assertion:
//
// - Default build (no `compat-v03`): missing header MUST return
//   400 VERSION_NOT_SUPPORTED.
// - `compat-v03` feature build: missing header MUST be accepted so
//   a2a-sdk 0.3.x clients keep working.
// ---------------------------------------------------------------------------

#[cfg(not(feature = "compat-v03"))]
#[tokio::test]
async fn a2a_version_missing_rejected_with_400_strict() {
    let router = compliance_router();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/message:send")
        .header("content-type", "application/json")
        .body(Body::from(http_send_body("m-no-version", "x")))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "default (strict) build MUST reject missing A2A-Version with 400"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_has_error_info(&body, "VERSION_NOT_SUPPORTED");
}

#[cfg(feature = "compat-v03")]
#[tokio::test]
async fn a2a_version_missing_accepted_under_compat_v03() {
    let router = compliance_router();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/message:send")
        .header("content-type", "application/json")
        .body(Body::from(http_send_body("m-no-version", "x")))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert!(
        resp.status().is_success(),
        "compat-v03 build MUST accept missing A2A-Version (a2a-sdk 0.3.x compatibility), got {}",
        resp.status()
    );
}

#[tokio::test]
async fn a2a_version_unsupported_value_always_rejected() {
    let router = compliance_router();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/message:send")
        .header("content-type", "application/json")
        .header("A2A-Version", "9.9")
        .body(Body::from(http_send_body("m-bad-version", "x")))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "unsupported A2A-Version must always be rejected with 400 \
         regardless of compat feature"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_has_error_info(&body, "VERSION_NOT_SUPPORTED");
}

// ---------------------------------------------------------------------------
// JSON-RPC method-name compliance
//
// Spec requires PascalCase method names matching the proto service
// method. An unknown method must return code -32601 (Method Not
// Found) per JSON-RPC 2.0.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn jsonrpc_unknown_method_returns_method_not_found() {
    let router = compliance_router();
    let resp = jsonrpc_call(router, "NonExistentMethod", serde_json::json!({}), 1).await;
    let code = resp
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|v| v.as_i64());
    assert_eq!(code, Some(-32601), "JSON-RPC Method Not Found per spec");
}

// ---------------------------------------------------------------------------
// ADR-015 §4.3 test 8 — skill-level security_requirements round-trip
// across every build-materializable card transport.
//
// The A2A proto has exactly one public-card RPC: HTTP discovery at
// `/.well-known/agent-card.json`. The extended card is served over
// HTTP at `/extendedAgentCard`, over JSON-RPC as
// `GetExtendedAgentCard`, and over gRPC as
// `A2AService.GetExtendedAgentCard`. There is no `GetAgentCard` /
// `A2AService.GetAgentCard` in the proto — see ADR-015 §4.3.
// ---------------------------------------------------------------------------

/// Executor whose public + extended cards both carry the same
/// skill-level `SecurityRequirement`, declaring the scheme in
/// `security_schemes` so build-time validation is satisfied.
struct Adr015RoundtripExecutor;

#[async_trait]
impl AgentExecutor for Adr015RoundtripExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let mut p = task.as_proto().clone();
        p.status = Some(turul_a2a_proto::TaskStatus {
            state: turul_a2a_proto::TaskState::Completed.into(),
            message: None,
            timestamp: None,
        });
        *task = Task::try_from(p).unwrap();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        adr015_card("ADR-015 round-trip agent")
    }

    fn extended_agent_card(
        &self,
        _claims: Option<&serde_json::Value>,
    ) -> Option<turul_a2a_proto::AgentCard> {
        Some(adr015_card("ADR-015 round-trip agent (extended)"))
    }
}

fn adr015_card(description: &str) -> turul_a2a_proto::AgentCard {
    let mut schemes_map = HashMap::new();
    schemes_map.insert(
        "bearer".to_string(),
        turul_a2a_proto::SecurityScheme {
            scheme: Some(
                turul_a2a_proto::security_scheme::Scheme::HttpAuthSecurityScheme(
                    turul_a2a_proto::HttpAuthSecurityScheme {
                        description: String::new(),
                        scheme: "Bearer".into(),
                        bearer_format: "JWT".into(),
                    },
                ),
            ),
        },
    );
    let mut skill_schemes = HashMap::new();
    skill_schemes.insert(
        "bearer".to_string(),
        turul_a2a_proto::StringList {
            list: vec!["read".into(), "write".into()],
        },
    );
    turul_a2a_proto::AgentCard {
        name: "adr015-roundtrip".into(),
        description: description.into(),
        supported_interfaces: vec![turul_a2a_proto::AgentInterface {
            url: "http://127.0.0.1:0".into(),
            protocol_binding: "HTTP+JSON".into(),
            tenant: String::new(),
            protocol_version: "1.0".into(),
        }],
        provider: None,
        version: "1.0.0".into(),
        documentation_url: None,
        capabilities: Some(turul_a2a_proto::AgentCapabilities {
            streaming: Some(false),
            push_notifications: Some(false),
            extensions: vec![],
            extended_agent_card: Some(true),
        }),
        security_schemes: schemes_map,
        security_requirements: vec![],
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![turul_a2a_proto::AgentSkill {
            id: "search".into(),
            name: "Search".into(),
            description: "Skill with ADR-015 skill-level security".into(),
            tags: vec!["adr015".into()],
            examples: vec![],
            input_modes: vec!["text/plain".into()],
            output_modes: vec!["text/plain".into()],
            security_requirements: vec![turul_a2a_proto::SecurityRequirement {
                schemes: skill_schemes,
            }],
        }],
        signatures: vec![],
        icon_url: None,
    }
}

fn adr015_compliance_router() -> axum::Router {
    A2aServer::builder()
        .executor(Adr015RoundtripExecutor)
        .storage(InMemoryA2aStorage::new())
        .build()
        .expect("ADR-015 round-trip server build")
        .into_router()
}

/// Canonicalize a JSON `skills[0].securityRequirements` array so
/// map-order differences do not cause spurious inequality.
fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: std::collections::BTreeMap<String, serde_json::Value> =
                std::collections::BTreeMap::new();
            for (k, v) in map {
                sorted.insert(k.clone(), canonicalize(v));
            }
            serde_json::to_value(sorted).unwrap()
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(canonicalize).collect())
        }
        other => other.clone(),
    }
}

fn skill_requirements_of(card: &serde_json::Value) -> serde_json::Value {
    canonicalize(
        card.pointer("/skills/0/securityRequirements")
            .expect("skills[0].securityRequirements present on card"),
    )
}

#[tokio::test]
async fn agent_card_with_skill_level_security_roundtrips_via_all_card_paths() {
    let router = adr015_compliance_router();

    // HTTP public discovery — the only normative path for the public card.
    let (status, _, public_card) = http_call(
        router.clone(),
        Method::GET,
        "/.well-known/agent-card.json",
        None,
    )
    .await;
    assert_eq!(status, 200);
    let public_reqs = skill_requirements_of(&public_card);
    assert!(
        public_reqs
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "public card must expose the skill-level securityRequirements"
    );

    // HTTP extended card.
    let (status, _, http_extended) =
        http_call(router.clone(), Method::GET, "/extendedAgentCard", None).await;
    assert_eq!(status, 200);
    let http_extended_reqs = skill_requirements_of(&http_extended);
    assert_eq!(
        http_extended_reqs, public_reqs,
        "HTTP extended card skill requirements MUST match public card"
    );

    // JSON-RPC GetExtendedAgentCard. `GetAgentCard` / `A2AService.GetAgentCard`
    // are not present in the proto service (only `GetExtendedAgentCard`),
    // so no JSON-RPC arm exists for the public card — HTTP discovery is
    // its sole transport.
    let jrpc = jsonrpc_call(
        router.clone(),
        "GetExtendedAgentCard",
        serde_json::json!({}),
        1,
    )
    .await;
    let jrpc_card = jrpc.get("result").expect("result envelope present");
    let jrpc_reqs = skill_requirements_of(jrpc_card);
    assert_eq!(
        jrpc_reqs, public_reqs,
        "JSON-RPC extended card skill requirements MUST match public card"
    );
}

#[cfg(feature = "grpc")]
mod adr015_grpc_roundtrip {
    use super::*;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::Request;
    use turul_a2a_proto::grpc::A2aServiceClient;

    async fn spawn_grpc() -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let server = A2aServer::builder()
            .executor(Adr015RoundtripExecutor)
            .storage(InMemoryA2aStorage::new())
            .bind(([127, 0, 0, 1], 0))
            .build()
            .expect("grpc server build");
        let router = server.into_tonic_router();
        let handle = tokio::spawn(async move {
            let _ = router
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (addr, handle)
    }

    /// gRPC `A2AService.GetExtendedAgentCard` arm of ADR-015 §4.3 test 8.
    #[tokio::test]
    async fn adr015_extended_card_skill_requirements_roundtrip_over_grpc() {
        let (addr, _handle) = spawn_grpc().await;
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        let mut client = A2aServiceClient::new(channel);
        let resp = client
            .get_extended_agent_card(Request::new(turul_a2a_proto::GetExtendedAgentCardRequest {
                tenant: String::new(),
            }))
            .await
            .expect("grpc extended card")
            .into_inner();
        let card_json = serde_json::to_value(&resp).expect("serialize proto card");
        let grpc_reqs = skill_requirements_of(&card_json);

        // Fetch the public card via HTTP for parity.
        let http_router = A2aServer::builder()
            .executor(Adr015RoundtripExecutor)
            .storage(InMemoryA2aStorage::new())
            .build()
            .expect("http server build")
            .into_router();
        let (status, _, public_card) = http_call(
            http_router,
            Method::GET,
            "/.well-known/agent-card.json",
            None,
        )
        .await;
        assert_eq!(status, 200);
        let public_reqs = skill_requirements_of(&public_card);

        assert_eq!(
            grpc_reqs, public_reqs,
            "gRPC extended card skill requirements MUST match public card"
        );
    }
}

#[tokio::test]
async fn jsonrpc_list_tasks_returns_paginated_result() {
    let router = compliance_router();
    let _ = http_call(
        router.clone(),
        Method::POST,
        "/message:send",
        Some(http_send_body("m-compliance-jrpc-list", "x")),
    )
    .await;
    let resp = jsonrpc_call(router, "ListTasks", serde_json::json!({}), 3).await;
    let result = resp.get("result").expect("result present");
    assert!(result.get("tasks").and_then(|v| v.as_array()).is_some());
    assert!(result.get("nextPageToken").is_some());
    assert!(result.get("pageSize").is_some());
    assert!(result.get("totalSize").is_some());
}

// ---------------------------------------------------------------------------
// Streaming
//
// SendStreamingMessage opens an SSE stream. Compliance contract:
// - Content-Type: text/event-stream
// - First event is a Task (spec §3.1.5)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_streaming_message_returns_text_event_stream() {
    let router = compliance_router();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/message:stream")
        .header("A2A-Version", "1.0")
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(Body::from(http_send_body("m-compliance-stream", "x")))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("text/event-stream"),
        "streaming response must use text/event-stream; got {ct}"
    );
    // Drain a small prefix of the SSE body to prove we got a real
    // event stream — full sequencing semantics are covered in
    // `sse_tests.rs` and `d3_streaming_tests.rs`.
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let prefix = std::str::from_utf8(&body).unwrap_or("");
    assert!(
        prefix.contains("data:"),
        "SSE body must contain at least one `data:` line, got: {prefix}"
    );
}

// =========================================================
// gRPC transport parity (ADR-014 §2.11)
// =========================================================
//
// These tests extend the compliance matrix from
// {HTTP+JSON, JSON-RPC} to {HTTP+JSON, JSON-RPC, gRPC} when the
// `grpc` feature is enabled. They prove two invariants:
//
//   1. Storage parity: a task created via HTTP+JSON is visible to a
//      gRPC client on the same underlying storage — "storage is the
//      source of truth" (ADR-009) crosses transports.
//   2. Error parity: `A2aError` variants surface with identical
//      `ErrorInfo.reason` strings and the canonical
//      `a2a-protocol.org` domain on all three transports. A bug in a
//      `core_*` function MUST produce an identically-labeled failure
//      on HTTP, JSON-RPC, and gRPC (ADR-005 extended, ADR-014 §2.11).

#[cfg(feature = "grpc")]
mod grpc_parity {
    use super::*;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::Request;
    use tonic_types::StatusExt;
    use turul_a2a_proto::grpc::A2aServiceClient;

    /// Stand up a tonic server that shares the given
    /// `InMemoryA2aStorage` with the provided axum compliance router
    /// (built from the SAME storage Arc-clone). Returns the bound
    /// address and the server join handle — caller aborts on drop.
    async fn spawn_grpc(storage: InMemoryA2aStorage) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let server = A2aServer::builder()
            .executor(ComplianceAgent)
            .storage(storage)
            .bind(([127, 0, 0, 1], 0))
            .build()
            .expect("grpc compliance server");
        let router = server.into_tonic_router();
        let handle = tokio::spawn(async move {
            let _ = router
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (addr, handle)
    }

    async fn grpc_client(addr: SocketAddr) -> A2aServiceClient<tonic::transport::Channel> {
        let ch = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        A2aServiceClient::new(ch)
    }

    /// Build a fresh compliance axum router and a gRPC server against
    /// the same `InMemoryA2aStorage`. Both transports share state.
    async fn dual_transport_harness() -> (axum::Router, SocketAddr, JoinHandle<()>) {
        let shared = InMemoryA2aStorage::new();
        let http_router = A2aServer::builder()
            .executor(ComplianceAgent)
            .storage(shared.clone())
            .build()
            .expect("http build")
            .into_router();
        let (grpc_addr, handle) = spawn_grpc(shared).await;
        (http_router, grpc_addr, handle)
    }

    /// Same task written via HTTP must be readable via gRPC.
    /// Proves the storage layer sits below the transport adapters.
    #[tokio::test]
    async fn http_write_is_visible_via_grpc_get_task() {
        let (router, grpc_addr, _guard) = dual_transport_harness().await;

        // HTTP SendMessage → task_id
        let (status, _, body) = http_call(
            router.clone(),
            Method::POST,
            "/message:send",
            Some(http_send_body("cross-1", "cross transport")),
        )
        .await;
        assert_eq!(status, 200);
        let task_id = body
            .get("task")
            .and_then(|t| t.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        assert!(!task_id.is_empty());

        // gRPC GetTask on same id → same task.
        let mut grpc = grpc_client(grpc_addr).await;
        let got = grpc
            .get_task(Request::new(turul_a2a_proto::GetTaskRequest {
                tenant: String::new(),
                id: task_id.clone(),
                history_length: None,
            }))
            .await
            .expect("grpc get_task ok")
            .into_inner();
        assert_eq!(got.id, task_id);
    }

    /// `TaskNotFound` surfaces the same `reason` + `domain` across
    /// all three transports.
    #[tokio::test]
    async fn task_not_found_error_parity_http_jsonrpc_grpc() {
        let (router, grpc_addr, _guard) = dual_transport_harness().await;

        // HTTP
        let (http_status, _, http_body) =
            http_call(router.clone(), Method::GET, "/tasks/does-not-exist", None).await;
        assert_eq!(http_status, 404);
        assert_has_error_info(&http_body, "TASK_NOT_FOUND");

        // JSON-RPC
        let jr = jsonrpc_call(
            router,
            "GetTask",
            serde_json::json!({"id": "does-not-exist"}),
            1,
        )
        .await;
        assert_eq!(
            jr.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|v| v.as_i64()),
            Some(-32001)
        );
        assert_has_error_info(&jr, "TASK_NOT_FOUND");

        // gRPC
        let mut grpc = grpc_client(grpc_addr).await;
        let err = grpc
            .get_task(Request::new(turul_a2a_proto::GetTaskRequest {
                tenant: String::new(),
                id: "does-not-exist".into(),
                history_length: None,
            }))
            .await
            .expect_err("grpc must NOT_FOUND");
        assert_eq!(err.code(), tonic::Code::NotFound);
        let info = err.get_details_error_info().expect("ErrorInfo");
        assert_eq!(info.reason, "TASK_NOT_FOUND");
        assert_eq!(info.domain, "a2a-protocol.org");
    }

    /// Terminal-subscribe: `UnsupportedOperation` surfaces with the
    /// same `UNSUPPORTED_OPERATION` reason on every transport.
    #[tokio::test]
    async fn terminal_subscribe_error_parity_http_jsonrpc_grpc() {
        let (router, grpc_addr, _guard) = dual_transport_harness().await;

        // Create + complete a task via HTTP so all three transports
        // see the same terminal state.
        let (status, _, body) = http_call(
            router.clone(),
            Method::POST,
            "/message:send",
            Some(http_send_body("t-sub", "terminal for subscribe parity")),
        )
        .await;
        assert_eq!(status, 200);
        let task_id = body
            .get("task")
            .and_then(|t| t.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        // JSON-RPC SubscribeToTask → error -32004 + UNSUPPORTED_OPERATION.
        let jr = jsonrpc_call(
            router,
            "SubscribeToTask",
            serde_json::json!({"id": task_id}),
            1,
        )
        .await;
        assert_eq!(
            jr.get("error")
                .and_then(|e| e.get("code"))
                .and_then(|v| v.as_i64()),
            Some(-32004)
        );
        assert_has_error_info(&jr, "UNSUPPORTED_OPERATION");

        // gRPC SubscribeToTask → FAILED_PRECONDITION + UNSUPPORTED_OPERATION.
        let mut grpc = grpc_client(grpc_addr).await;
        let err = grpc
            .subscribe_to_task(Request::new(turul_a2a_proto::SubscribeToTaskRequest {
                tenant: String::new(),
                id: task_id,
            }))
            .await
            .expect_err("grpc must reject terminal");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        let info = err.get_details_error_info().expect("ErrorInfo");
        assert_eq!(info.reason, "UNSUPPORTED_OPERATION");
        assert_eq!(info.domain, "a2a-protocol.org");
    }
}
