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
    let req = req.body(body.map(Body::from).unwrap_or_else(Body::empty)).unwrap();
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
    let info: &serde_json::Value = if let Some(details) = error.get("details").and_then(|d| d.as_array()) {
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
        panic!("error must carry ErrorInfo via either `details` (HTTP) or `data` (JSON-RPC); got {json}")
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
    let (status, _, card) = http_call(
        router,
        Method::GET,
        "/.well-known/agent-card.json",
        None,
    )
    .await;
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
    let (status, _, card) =
        http_call(router, Method::GET, "/extendedAgentCard", None).await;
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
    let (status, _, http_task) =
        http_call(router.clone(), Method::GET, &format!("/tasks/{task_id}"), None).await;
    assert_eq!(status, 200);
    assert_eq!(
        http_task.get("id").and_then(|v| v.as_str()),
        Some(task_id.as_str())
    );

    // JSON-RPC.
    let jrpc = jsonrpc_call(
        router,
        "GetTask",
        serde_json::json!({"id": task_id}),
        42,
    )
    .await;
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
    let (status, _, http_err) = http_call(
        router.clone(),
        Method::GET,
        "/tasks/does-not-exist",
        None,
    )
    .await;
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
    assert_eq!(code, -32001, "TaskNotFoundError JSON-RPC code per spec §5.4");
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
    assert_eq!(status, 409, "TaskNotCancelableError HTTP status per ADR-004");
    assert_has_error_info(&http_err, "TASK_NOT_CANCELABLE");

    // JSON-RPC.
    let jrpc = jsonrpc_call(
        router,
        "CancelTask",
        serde_json::json!({"id": task_id}),
        11,
    )
    .await;
    let code = jrpc.get("error").and_then(|e| e.get("code")).and_then(|v| v.as_i64());
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
    let (status, _, err) = http_call(
        router,
        Method::POST,
        "/tasks/ghost:cancel",
        None,
    )
    .await;
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
    let code = resp.get("error").and_then(|e| e.get("code")).and_then(|v| v.as_i64());
    assert_eq!(code, Some(-32601), "JSON-RPC Method Not Found per spec");
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
