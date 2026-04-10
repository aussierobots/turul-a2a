//! Handler integration tests — exercise handlers through HTTP via storage.
//!
//! Tests cover exact response envelopes, ErrorInfo on error paths,
//! owner/tenant scoping, historyLength, pagination, and push config CRUD.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::router::{build_router, AppState};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Message, Task};

/// Test executor that transitions task to Working then Completed.
struct CompletingExecutor;

#[async_trait::async_trait]
impl AgentExecutor for CompletingExecutor {
    async fn execute(&self, task: &mut Task, _message: &Message) -> Result<(), A2aError> {
        // Move to Working, then Completed
        let mut proto = task.as_proto().clone();
        proto.status = Some(turul_a2a_proto::TaskStatus {
            state: turul_a2a_proto::TaskState::Completed.into(),
            message: None,
            timestamp: None,
        });
        proto.artifacts.push(turul_a2a_proto::Artifact {
            artifact_id: "result-1".into(),
            name: "Result".into(),
            description: String::new(),
            parts: vec![turul_a2a_proto::Part {
                content: Some(turul_a2a_proto::part::Content::Text("done".into())),
                metadata: None,
                filename: String::new(),
                media_type: String::new(),
            }],
            metadata: None,
            extensions: vec![],
        });
        *task = Task::try_from(proto).unwrap();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        test_agent_card()
    }
}

fn test_agent_card() -> turul_a2a_proto::AgentCard {
    turul_a2a_proto::AgentCard {
        name: "Test Agent".into(),
        description: "A test agent".into(),
        supported_interfaces: vec![turul_a2a_proto::AgentInterface {
            url: "http://localhost:3000".into(),
            protocol_binding: "JSONRPC".into(),
            tenant: String::new(),
            protocol_version: "1.0".into(),
        }],
        provider: None,
        version: "1.0.0".into(),
        documentation_url: None,
        capabilities: Some(turul_a2a_proto::AgentCapabilities {
            streaming: Some(false),
            push_notifications: Some(true),
            extensions: vec![],
            extended_agent_card: Some(false),
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

fn test_state() -> AppState {
    let storage = InMemoryA2aStorage::new();
    AppState {
        executor: Arc::new(CompletingExecutor),
        task_storage: Arc::new(storage.clone()),
        push_storage: Arc::new(storage),
    }
}

async fn json_response(
    router: axum::Router,
    req: Request<Body>,
) -> (u16, serde_json::Value) {
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&body).unwrap_or_default();
    (status, json)
}

fn send_message_json(message_id: &str, text: &str) -> String {
    serde_json::json!({
        "message": {
            "messageId": message_id,
            "role": "ROLE_USER",
            "parts": [{"text": text}]
        }
    })
    .to_string()
}

// =========================================================
// SendMessage — response envelope is SendMessageResponse
// =========================================================

#[tokio::test]
async fn send_message_returns_send_message_response_with_task() {
    let router = build_router(test_state());
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .body(Body::from(send_message_json("m-1", "hello")))
        .unwrap();

    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);

    // SendMessageResponse has oneof: task or message
    // Our executor completes the task, so we expect "task" variant
    assert!(
        body.get("task").is_some() || body.get("message").is_some(),
        "SendMessageResponse must have 'task' or 'message' field, got: {body}"
    );

    if let Some(task) = body.get("task") {
        assert!(task.get("id").is_some(), "Task must have id");
        assert!(task.get("status").is_some(), "Task must have status");
    }
}

#[tokio::test]
async fn send_message_tenant_prefixed_works() {
    let router = build_router(test_state());
    let req = Request::post("/acme/message:send")
        .header("content-type", "application/json")
        .body(Body::from(send_message_json("m-t", "hello tenant")))
        .unwrap();

    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);
    assert!(
        body.get("task").is_some() || body.get("message").is_some(),
        "Tenant-prefixed SendMessage must return valid response"
    );
}

// =========================================================
// GetTask — returns Task with correct fields
// =========================================================

#[tokio::test]
async fn get_task_returns_task_after_send() {
    let state = test_state();
    let router = build_router(state.clone());

    // First: send a message to create a task
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .body(Body::from(send_message_json("m-g1", "create task")))
        .unwrap();
    let (_, send_body) = json_response(router, req).await;
    let task_id = send_body["task"]["id"].as_str().unwrap();

    // Then: get the task
    let router = build_router(state);
    let req = Request::get(&format!("/tasks/{task_id}"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["id"].as_str().unwrap(), task_id);
    assert!(body.get("status").is_some());
}

#[tokio::test]
async fn get_task_nonexistent_returns_404_with_error_info() {
    let router = build_router(test_state());
    let req = Request::get("/tasks/nonexistent-id")
        .body(Body::empty())
        .unwrap();

    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 404);

    // Must have ErrorInfo
    let details = body["error"]["details"].as_array().unwrap();
    assert!(!details.is_empty());
    assert_eq!(
        details[0]["@type"],
        "type.googleapis.com/google.rpc.ErrorInfo"
    );
    assert_eq!(details[0]["reason"], "TASK_NOT_FOUND");
    assert_eq!(details[0]["domain"], "a2a-protocol.org");
}

#[tokio::test]
async fn get_task_history_length_zero_omits_history() {
    let state = test_state();

    // Create task via send
    let router = build_router(state.clone());
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .body(Body::from(send_message_json("m-hl", "with history")))
        .unwrap();
    let (_, send_body) = json_response(router, req).await;
    let task_id = send_body["task"]["id"].as_str().unwrap();

    // Get with historyLength=0
    let router = build_router(state);
    let req = Request::get(&format!("/tasks/{task_id}?historyLength=0"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);
    // History should be empty or absent (proto3 omits empty repeated)
    let history = body.get("history");
    assert!(
        history.is_none() || history.unwrap().as_array().map_or(true, |a| a.is_empty()),
        "historyLength=0 should omit history"
    );
}

// =========================================================
// CancelTask — 409 on terminal, 200 on cancelable
// =========================================================

#[tokio::test]
async fn cancel_completed_task_returns_409_with_error_info() {
    let state = test_state();

    // Create and complete a task
    let router = build_router(state.clone());
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .body(Body::from(send_message_json("m-c", "complete me")))
        .unwrap();
    let (_, send_body) = json_response(router, req).await;
    let task_id = send_body["task"]["id"].as_str().unwrap();

    // Cancel the completed task
    let router = build_router(state);
    let req = Request::post(&format!("/tasks/{task_id}:cancel"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 409);

    let details = body["error"]["details"].as_array().unwrap();
    assert_eq!(details[0]["reason"], "TASK_NOT_CANCELABLE");
    assert_eq!(details[0]["domain"], "a2a-protocol.org");
}

#[tokio::test]
async fn cancel_nonexistent_task_returns_404_with_error_info() {
    let router = build_router(test_state());
    let req = Request::post("/tasks/no-such-task:cancel")
        .body(Body::empty())
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 404);

    let details = body["error"]["details"].as_array().unwrap();
    assert_eq!(details[0]["reason"], "TASK_NOT_FOUND");
}

// =========================================================
// ListTasks — pagination fields always present
// =========================================================

#[tokio::test]
async fn list_tasks_returns_required_pagination_fields() {
    let router = build_router(test_state());
    let req = Request::get("/tasks").body(Body::empty()).unwrap();

    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);

    // All REQUIRED per proto ListTasksResponse
    assert!(body.get("tasks").is_some(), "must have tasks array");
    assert!(
        body.get("nextPageToken").is_some(),
        "must have nextPageToken"
    );
    assert!(body.get("pageSize").is_some(), "must have pageSize");
    assert!(body.get("totalSize").is_some(), "must have totalSize");
}

#[tokio::test]
async fn list_tasks_empty_result_still_has_all_fields() {
    let router = build_router(test_state());
    let req = Request::get("/tasks").body(Body::empty()).unwrap();

    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["tasks"].as_array().unwrap().len(), 0);
    assert_eq!(body["totalSize"], 0);
}

// =========================================================
// Push notification config — CRUD through HTTP
// =========================================================

#[tokio::test]
async fn push_config_crud_through_http() {
    let state = test_state();

    // Create a task first
    let router = build_router(state.clone());
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .body(Body::from(send_message_json("m-pc", "for push")))
        .unwrap();
    let (_, send_body) = json_response(router, req).await;
    let task_id = send_body["task"]["id"].as_str().unwrap();

    // Create push config
    let router = build_router(state.clone());
    let config_body = serde_json::json!({
        "taskId": task_id,
        "url": "https://example.com/webhook"
    })
    .to_string();
    let req = Request::post(&format!("/tasks/{task_id}/pushNotificationConfigs"))
        .header("content-type", "application/json")
        .body(Body::from(config_body))
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);
    assert!(!body["id"].as_str().unwrap_or("").is_empty(), "config must have server-generated id");
    let config_id = body["id"].as_str().unwrap();

    // Get push config
    let router = build_router(state.clone());
    let req = Request::get(&format!(
        "/tasks/{task_id}/pushNotificationConfigs/{config_id}"
    ))
    .body(Body::empty())
    .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["id"].as_str().unwrap(), config_id);

    // List push configs
    let router = build_router(state.clone());
    let req = Request::get(&format!("/tasks/{task_id}/pushNotificationConfigs"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);
    assert!(body.get("configs").is_some());

    // Delete push config
    let router = build_router(state.clone());
    let req = Request::builder()
        .method("DELETE")
        .uri(&format!(
            "/tasks/{task_id}/pushNotificationConfigs/{config_id}"
        ))
        .body(Body::empty())
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 200);

    // Get after delete returns 404
    let router = build_router(state);
    let req = Request::get(&format!(
        "/tasks/{task_id}/pushNotificationConfigs/{config_id}"
    ))
    .body(Body::empty())
    .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 404);
}

// =========================================================
// Agent card discovery — exact paths
// =========================================================

#[tokio::test]
async fn well_known_agent_card_has_all_required_fields() {
    let router = build_router(test_state());
    let req = Request::get("/.well-known/agent-card.json")
        .body(Body::empty())
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);
    assert!(body.get("name").is_some());
    assert!(body.get("description").is_some());
    assert!(body.get("version").is_some());
    assert!(body.get("supportedInterfaces").is_some());
    assert!(body.get("capabilities").is_some());
    assert!(body.get("defaultInputModes").is_some());
    assert!(body.get("defaultOutputModes").is_some());
}

// =========================================================
// [P1] Tenant isolation — tenant from path actually scopes data
// =========================================================

#[tokio::test]
async fn tenant_prefixed_send_scopes_to_tenant() {
    let state = test_state();

    // Create task under tenant "acme"
    let router = build_router(state.clone());
    let req = Request::post("/acme/message:send")
        .header("content-type", "application/json")
        .body(Body::from(send_message_json("m-ta", "acme task")))
        .unwrap();
    let (status, send_body) = json_response(router, req).await;
    assert_eq!(status, 200);
    let task_id = send_body["task"]["id"].as_str().unwrap();

    // Get the task under tenant "acme" — should find it
    let router = build_router(state.clone());
    let req = Request::get(&format!("/acme/tasks/{task_id}"))
        .body(Body::empty())
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 200, "Task should be visible under its own tenant");

    // Get the same task under default (no tenant) — should NOT find it
    let router = build_router(state.clone());
    let req = Request::get(&format!("/tasks/{task_id}"))
        .body(Body::empty())
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 404, "Task should be invisible under different tenant");

    // Get the same task under tenant "other" — should NOT find it
    let router = build_router(state.clone());
    let req = Request::get(&format!("/other/tasks/{task_id}"))
        .body(Body::empty())
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 404, "Task should be invisible under wrong tenant");

    // List under "acme" should include it
    let router = build_router(state.clone());
    let req = Request::get("/acme/tasks")
        .body(Body::empty())
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["totalSize"], 1);

    // List under default should NOT include it
    let router = build_router(state);
    let req = Request::get("/tasks")
        .body(Body::empty())
        .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);
    assert_eq!(body["totalSize"], 0, "Default tenant should not see acme's tasks");
}

#[tokio::test]
async fn tenant_prefixed_cancel_scopes_to_tenant() {
    let state = test_state();

    // Create task under "acme"
    let router = build_router(state.clone());
    let req = Request::post("/acme/message:send")
        .header("content-type", "application/json")
        .body(Body::from(send_message_json("m-tc", "cancel me")))
        .unwrap();
    let (_, send_body) = json_response(router, req).await;
    let task_id = send_body["task"]["id"].as_str().unwrap();

    // Cancel under wrong tenant — should fail (404)
    let router = build_router(state.clone());
    let req = Request::post(&format!("/other/tasks/{task_id}:cancel"))
        .body(Body::empty())
        .unwrap();
    let (status, _) = json_response(router, req).await;
    assert_eq!(status, 404, "Cancel under wrong tenant should return 404");
}

// =========================================================
// [P2] ListTasks status filter — actually narrows results
// =========================================================

#[tokio::test]
async fn list_tasks_status_filter_narrows_results() {
    let state = test_state();

    // Create 2 tasks — both will complete (executor completes them)
    for i in 0..2 {
        let router = build_router(state.clone());
        let req = Request::post("/message:send")
            .header("content-type", "application/json")
            .body(Body::from(send_message_json(&format!("m-sf-{i}"), "task")))
            .unwrap();
        json_response(router, req).await;
    }

    // List all — should see 2
    let router = build_router(state.clone());
    let req = Request::get("/tasks")
        .body(Body::empty())
        .unwrap();
    let (_, body) = json_response(router, req).await;
    assert_eq!(body["totalSize"], 2);

    // List with status=TASK_STATE_COMPLETED — should see 2 (both completed)
    let router = build_router(state.clone());
    let req = Request::get("/tasks?status=TASK_STATE_COMPLETED")
        .body(Body::empty())
        .unwrap();
    let (_, body) = json_response(router, req).await;
    assert_eq!(body["totalSize"], 2, "Both tasks should be completed");

    // List with status=TASK_STATE_WORKING — should see 0
    let router = build_router(state);
    let req = Request::get("/tasks?status=TASK_STATE_WORKING")
        .body(Body::empty())
        .unwrap();
    let (_, body) = json_response(router, req).await;
    assert_eq!(body["totalSize"], 0, "No tasks should be in working state");
}

// =========================================================
// [P2] Push config list pagination through HTTP
// =========================================================

#[tokio::test]
async fn push_config_list_pagination_through_http() {
    let state = test_state();

    // Create a task
    let router = build_router(state.clone());
    let req = Request::post("/message:send")
        .header("content-type", "application/json")
        .body(Body::from(send_message_json("m-pcp", "for push pagination")))
        .unwrap();
    let (_, send_body) = json_response(router, req).await;
    let task_id = send_body["task"]["id"].as_str().unwrap();

    // Create 5 push configs
    for i in 0..5 {
        let router = build_router(state.clone());
        let config_body = serde_json::json!({
            "taskId": task_id,
            "url": format!("https://example.com/hook-{i}")
        })
        .to_string();
        let req = Request::post(&format!("/tasks/{task_id}/pushNotificationConfigs"))
            .header("content-type", "application/json")
            .body(Body::from(config_body))
            .unwrap();
        let (status, _) = json_response(router, req).await;
        assert_eq!(status, 200);
    }

    // List with pageSize=2 — should paginate
    let router = build_router(state.clone());
    let req = Request::get(&format!(
        "/tasks/{task_id}/pushNotificationConfigs?pageSize=2"
    ))
    .body(Body::empty())
    .unwrap();
    let (status, body) = json_response(router, req).await;
    assert_eq!(status, 200);
    let configs = body["configs"].as_array().unwrap();
    assert!(configs.len() <= 2, "pageSize=2 should return at most 2");
    assert!(
        !body["nextPageToken"].as_str().unwrap_or("").is_empty(),
        "Should have nextPageToken for next page"
    );
}
