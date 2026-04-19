//! Client integration tests with wiremock mock server.

use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use turul_a2a_client::{A2aClient, ClientAuth, ListTasksParams};

// =========================================================
// Discovery
// =========================================================

#[tokio::test]
async fn discover_fetches_agent_card() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "Test Agent",
            "description": "A test agent",
            "version": "1.0.0",
            "supportedInterfaces": [{"url": server.uri(), "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}],
            "capabilities": {"streaming": false},
            "defaultInputModes": ["text/plain"],
            "defaultOutputModes": ["text/plain"],
            "skills": []
        })))
        .mount(&server)
        .await;

    let client = A2aClient::discover(&server.uri()).await.unwrap();
    let card = client.agent_card().unwrap();
    assert_eq!(card.name, "Test Agent");
}

#[tokio::test]
async fn discover_handles_server_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let result = A2aClient::discover(&server.uri()).await;
    assert!(result.is_err());
}

// =========================================================
// SendMessage — returns SendMessageResponse, not raw Task
// =========================================================

#[tokio::test]
async fn send_message_returns_send_message_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/message:send"))
        .and(header("a2a-version", "1.0"))
        .and(header("content-type", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task": {
                "id": "task-1",
                "status": {"state": "TASK_STATE_COMPLETED"},
                "history": [],
                "artifacts": []
            }
        })))
        .mount(&server)
        .await;

    let client = A2aClient::new(server.uri());
    let request = turul_a2a_proto::SendMessageRequest {
        tenant: String::new(),
        message: Some(turul_a2a_proto::Message {
            message_id: "m-1".into(),
            role: turul_a2a_proto::Role::User.into(),
            parts: vec![turul_a2a_proto::Part {
                content: Some(turul_a2a_proto::part::Content::Text("hello".into())),
                metadata: None,
                filename: String::new(),
                media_type: String::new(),
            }],
            context_id: String::new(),
            task_id: String::new(),
            metadata: None,
            extensions: vec![],
            reference_task_ids: vec![],
        }),
        configuration: None,
        metadata: None,
    };

    let response = client.send_message(request).await.unwrap();
    let task = response.into_task().expect("Expected Task");
    assert_eq!(task.id(), "task-1");
}

// =========================================================
// GetTask
// =========================================================

#[tokio::test]
async fn get_task_returns_task() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tasks/task-42"))
        .and(header("a2a-version", "1.0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "task-42",
            "status": {"state": "TASK_STATE_WORKING"},
            "history": [],
            "artifacts": []
        })))
        .mount(&server)
        .await;

    let client = A2aClient::new(server.uri());
    let task = client.get_task("task-42", None).await.unwrap();
    assert_eq!(task.id(), "task-42");
}

#[tokio::test]
async fn get_task_not_found_returns_a2a_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tasks/nonexistent"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": {
                "code": 404,
                "message": "Task not found",
                "details": [{
                    "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                    "reason": "TASK_NOT_FOUND",
                    "domain": "a2a-protocol.org"
                }]
            }
        })))
        .mount(&server)
        .await;

    let client = A2aClient::new(server.uri());
    let err = client.get_task("nonexistent", None).await.unwrap_err();
    assert_eq!(err.status(), Some(404));
    assert_eq!(err.reason(), Some("TASK_NOT_FOUND"));
}

// =========================================================
// CancelTask — 409 error parsing
// =========================================================

#[tokio::test]
async fn cancel_terminal_task_returns_409_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tasks/completed-task:cancel"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": {
                "code": 409,
                "message": "Task not cancelable",
                "details": [{
                    "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                    "reason": "TASK_NOT_CANCELABLE",
                    "domain": "a2a-protocol.org"
                }]
            }
        })))
        .mount(&server)
        .await;

    let client = A2aClient::new(server.uri());
    let err = client.cancel_task("completed-task").await.unwrap_err();
    assert_eq!(err.status(), Some(409));
    assert_eq!(err.reason(), Some("TASK_NOT_CANCELABLE"));
}

// =========================================================
// ListTasks — pagination
// =========================================================

#[tokio::test]
async fn list_tasks_with_pagination() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tasks"))
        .and(header("a2a-version", "1.0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tasks": [{"id": "t-1", "status": {"state": "TASK_STATE_COMPLETED"}}],
            "nextPageToken": "token-2",
            "pageSize": 10,
            "totalSize": 25
        })))
        .mount(&server)
        .await;

    let client = A2aClient::new(server.uri());
    let response = client
        .list_tasks(&ListTasksParams::default())
        .await
        .unwrap();
    assert_eq!(response.tasks.len(), 1);
    assert_eq!(response.next_page_token, "token-2");
    assert_eq!(response.total_size, 25);
}

// =========================================================
// Auth — Bearer token sent
// =========================================================

#[tokio::test]
async fn bearer_auth_sends_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tasks"))
        .and(header("authorization", "Bearer my-jwt-token"))
        .and(header("a2a-version", "1.0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tasks": [], "nextPageToken": "", "pageSize": 50, "totalSize": 0
        })))
        .mount(&server)
        .await;

    let client = A2aClient::new(server.uri()).with_auth(ClientAuth::Bearer("my-jwt-token".into()));
    client
        .list_tasks(&ListTasksParams::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn api_key_auth_sends_custom_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tasks"))
        .and(header("x-api-key", "secret-key"))
        .and(header("a2a-version", "1.0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tasks": [], "nextPageToken": "", "pageSize": 50, "totalSize": 0
        })))
        .mount(&server)
        .await;

    let client = A2aClient::new(server.uri()).with_auth(ClientAuth::ApiKey {
        header: "X-API-Key".into(),
        key: "secret-key".into(),
    });
    client
        .list_tasks(&ListTasksParams::default())
        .await
        .unwrap();
}

// =========================================================
// Tenant prefixing
// =========================================================

#[tokio::test]
async fn tenant_prefix_applied_to_routes() {
    let server = MockServer::start().await;
    // Expect the tenant-prefixed path
    Mock::given(method("GET"))
        .and(path("/acme/tasks"))
        .and(header("a2a-version", "1.0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tasks": [], "nextPageToken": "", "pageSize": 50, "totalSize": 0
        })))
        .mount(&server)
        .await;

    let client = A2aClient::new(server.uri()).with_tenant("acme");
    client
        .list_tasks(&ListTasksParams::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn tenant_prefix_on_send_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/my-tenant/message:send"))
        .and(header("a2a-version", "1.0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task": {"id": "t-1", "status": {"state": "TASK_STATE_COMPLETED"}}
        })))
        .mount(&server)
        .await;

    let client = A2aClient::new(server.uri()).with_tenant("my-tenant");
    let request = turul_a2a_proto::SendMessageRequest {
        tenant: String::new(),
        message: Some(turul_a2a_proto::Message {
            message_id: "m-t".into(),
            role: turul_a2a_proto::Role::User.into(),
            parts: vec![],
            context_id: String::new(),
            task_id: String::new(),
            metadata: None,
            extensions: vec![],
            reference_task_ids: vec![],
        }),
        configuration: None,
        metadata: None,
    };
    client.send_message(request).await.unwrap();
}

// =========================================================
// A2A-Version header always sent
// =========================================================

#[tokio::test]
async fn a2a_version_header_sent_on_all_requests() {
    let server = MockServer::start().await;
    // This mock only matches if a2a-version is present
    Mock::given(method("GET"))
        .and(path("/tasks/check-version"))
        .and(header("a2a-version", "1.0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "check-version",
            "status": {"state": "TASK_STATE_WORKING"}
        })))
        .mount(&server)
        .await;

    let client = A2aClient::new(server.uri());
    client.get_task("check-version", None).await.unwrap();
}

// =========================================================
// MessageBuilder — wrapper part methods
// =========================================================

#[test]
fn message_builder_data_produces_valid_request() {
    use turul_a2a_client::MessageBuilder;

    let request = MessageBuilder::new().data(json!({"key": "value"})).build();

    let msg = request.message.unwrap();
    assert_eq!(msg.parts.len(), 1);
    // Data part should have structured data content
    assert!(msg.parts[0].content.is_some());
}

#[test]
fn message_builder_part_accepts_wrapper_part() {
    use turul_a2a_client::MessageBuilder;
    use turul_a2a_types::Part;

    let request = MessageBuilder::new()
        .part(Part::text("hello"))
        .part(Part::url("https://example.com", "text/html"))
        .build();

    let msg = request.message.unwrap();
    assert_eq!(msg.parts.len(), 2);
}

#[test]
fn message_builder_parts_accepts_iterator() {
    use turul_a2a_client::MessageBuilder;
    use turul_a2a_types::Part;

    let parts = vec![Part::text("one"), Part::text("two"), Part::text("three")];

    let request = MessageBuilder::new().parts(parts).build();

    let msg = request.message.unwrap();
    assert_eq!(msg.parts.len(), 3);
}

#[test]
fn message_builder_mixed_methods() {
    use turul_a2a_client::MessageBuilder;
    use turul_a2a_types::Part;

    let request = MessageBuilder::new()
        .text("plain text")
        .data(json!({"structured": true}))
        .part(Part::raw(vec![1, 2, 3], "application/octet-stream"))
        .build();

    let msg = request.message.unwrap();
    assert_eq!(msg.parts.len(), 3);
}

// =========================================================
// Response helpers
// =========================================================

#[test]
fn send_response_extracts_wrapper_task() {
    use turul_a2a_client::response::SendResponse;

    let proto_resp = turul_a2a_proto::SendMessageResponse {
        payload: Some(turul_a2a_proto::send_message_response::Payload::Task(
            turul_a2a_proto::Task {
                id: "task-1".into(),
                context_id: "ctx-1".into(),
                status: Some(turul_a2a_proto::TaskStatus {
                    state: turul_a2a_proto::TaskState::Completed.into(),
                    message: None,
                    timestamp: None,
                }),
                ..Default::default()
            },
        )),
    };

    let resp = SendResponse::try_from(proto_resp).unwrap();
    assert!(resp.is_task());
    let task = resp.into_task().unwrap();
    assert_eq!(task.id(), "task-1");
}

#[test]
fn send_response_task_accessor() {
    use turul_a2a_client::response::SendResponse;

    let proto_resp = turul_a2a_proto::SendMessageResponse {
        payload: Some(turul_a2a_proto::send_message_response::Payload::Task(
            turul_a2a_proto::Task {
                id: "task-2".into(),
                status: Some(turul_a2a_proto::TaskStatus {
                    state: turul_a2a_proto::TaskState::Completed.into(),
                    message: None,
                    timestamp: None,
                }),
                ..Default::default()
            },
        )),
    };

    let resp = SendResponse::try_from(proto_resp).unwrap();
    assert_eq!(resp.task().unwrap().id(), "task-2");
}

#[test]
fn send_response_message_variant() {
    use turul_a2a_client::response::SendResponse;

    let proto_resp = turul_a2a_proto::SendMessageResponse {
        payload: Some(turul_a2a_proto::send_message_response::Payload::Message(
            turul_a2a_proto::Message {
                role: turul_a2a_proto::Role::Agent.into(),
                ..Default::default()
            },
        )),
    };

    let resp = SendResponse::try_from(proto_resp).unwrap();
    assert!(!resp.is_task());
    assert!(resp.task().is_none());
    assert!(resp.message().is_some());
}

#[test]
fn artifact_text_extracts_text_from_task() {
    use turul_a2a_client::response;
    use turul_a2a_types::{Task, TaskState, TaskStatus};

    let mut task = Task::new("t-1", TaskStatus::new(TaskState::Completed));
    task.push_text_artifact("a-1", "Result", "hello world");

    let text = response::artifact_text(&task);
    assert_eq!(text, "hello world");
}
