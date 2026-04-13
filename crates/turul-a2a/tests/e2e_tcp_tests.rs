//! E2E tests: real TCP Rust client against real TCP Rust server.
//!
//! These tests bind a real A2aServer to localhost:0, get the assigned port,
//! and use A2aClient making actual HTTP requests over the network.
//! No tower mocks, no oneshot — real HTTP connections.

use std::sync::Arc;

use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::storage::InMemoryA2aStorage;
use futures::StreamExt;
use turul_a2a_client::{A2aClient, A2aClientError, ClientAuth, ListTasksParams, MessageBuilder};
use turul_a2a_client::response::{response_task, response_task_id};
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};

// =========================================================
// Test executors
// =========================================================

struct CompletingExecutor;

#[async_trait::async_trait]
impl AgentExecutor for CompletingExecutor {
    async fn execute(&self, task: &mut Task, _msg: &Message, _ctx: &turul_a2a::executor::ExecutionContext) -> Result<(), A2aError> {
        task.push_text_artifact("tcp-art", "Result", "tcp e2e result");
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("TCP E2E Agent", "1.0.0")
            .description("Agent for real TCP E2E tests")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build()
            .unwrap()
    }
}

struct PausingExecutor;

#[async_trait::async_trait]
impl AgentExecutor for PausingExecutor {
    async fn execute(&self, task: &mut Task, _msg: &Message, _ctx: &turul_a2a::executor::ExecutionContext) -> Result<(), A2aError> {
        // First call: pause at INPUT_REQUIRED
        // Second call (continuation): complete
        if task.history().len() <= 1 {
            task.set_status(TaskStatus::new(TaskState::InputRequired));
        } else {
            task.push_text_artifact("multi-art", "Result", "multi-turn result");
            task.complete();
        }
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Pausing E2E Agent", "1.0.0")
            .description("Pauses at INPUT_REQUIRED for multi-turn tests")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build()
            .unwrap()
    }
}

// =========================================================
// Server helper
// =========================================================

/// Start a real TCP server and return its base URL.
/// Server runs in a background task and shuts down when the returned guard is dropped.
async fn start_server(executor: impl AgentExecutor + 'static) -> String {
    let storage = InMemoryA2aStorage::new();
    let server = turul_a2a::server::A2aServer::builder()
        .executor(executor)
        .storage(storage)
        .bind(([127, 0, 0, 1], 0u16))
        .build()
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server.into_router();

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Small delay for server to be ready
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    format!("http://{addr}")
}

/// Start a server with API key auth.
async fn start_server_with_api_key(
    executor: impl AgentExecutor + 'static,
    expected_key: &str,
) -> String {
    use turul_a2a_auth::ApiKeyMiddleware;

    struct StaticKeyLookup {
        key: String,
    }

    #[async_trait::async_trait]
    impl turul_a2a_auth::ApiKeyLookup for StaticKeyLookup {
        async fn lookup(&self, key: &str) -> Option<String> {
            if key == self.key { Some("test-owner".into()) } else { None }
        }
    }

    let storage = InMemoryA2aStorage::new();
    let auth = ApiKeyMiddleware::new(
        Arc::new(StaticKeyLookup { key: expected_key.to_string() }),
        "X-API-Key",
    );

    let server = turul_a2a::server::A2aServer::builder()
        .executor(executor)
        .storage(storage)
        .middleware(Arc::new(auth))
        .bind(([127, 0, 0, 1], 0u16))
        .build()
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server.into_router();

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    format!("http://{addr}")
}

fn send_request(id: &str, text: &str) -> turul_a2a_proto::SendMessageRequest {
    turul_a2a_proto::SendMessageRequest {
        message: Some(turul_a2a_proto::Message {
            message_id: id.into(),
            role: turul_a2a_proto::Role::User.into(),
            parts: vec![turul_a2a_proto::Part {
                content: Some(turul_a2a_proto::part::Content::Text(text.into())),
                metadata: None,
                filename: String::new(),
                media_type: String::new(),
            }],
            ..Default::default()
        }),
        configuration: None,
        metadata: None,
        tenant: String::new(),
    }
}

fn send_request_with_task(id: &str, text: &str, task_id: &str) -> turul_a2a_proto::SendMessageRequest {
    let mut req = send_request(id, text);
    if let Some(ref mut msg) = req.message {
        msg.task_id = task_id.into();
    }
    req
}

// =========================================================
// Path A: Real TCP E2E tests
// =========================================================

#[tokio::test]
async fn e2e_tcp_discover_agent_card() {
    let url = start_server(CompletingExecutor).await;
    let client = A2aClient::discover(&url).await.unwrap();
    let card = client.agent_card().unwrap();
    assert_eq!(card.name, "TCP E2E Agent");
    assert!(!card.supported_interfaces.is_empty());
}

#[tokio::test]
async fn e2e_tcp_send_message_creates_task() {
    let url = start_server(CompletingExecutor).await;
    let client = A2aClient::new(&url);

    let resp = client.send_message(send_request("tcp-send-1", "hello")).await.unwrap();

    // Response should contain a task
    let task = response_task(&resp).expect("Expected Task payload");
    assert!(!task.id().is_empty());
    assert_eq!(task.status().unwrap().state().unwrap(), TaskState::Completed);
    assert!(!task.artifacts().is_empty());
}

#[tokio::test]
async fn e2e_tcp_get_task_after_send() {
    let url = start_server(CompletingExecutor).await;
    let client = A2aClient::new(&url);

    let resp = client.send_message(send_request("tcp-get-1", "create")).await.unwrap();
    let task_id = response_task_id(&resp).unwrap().to_string();

    // Get with no history limit
    let task = client.get_task(&task_id, None).await.unwrap();
    assert_eq!(task.id, task_id);
    assert_eq!(task.status.as_ref().unwrap().state, i32::from(turul_a2a_proto::TaskState::Completed));

    // Get with history_length=0 should omit history
    let task = client.get_task(&task_id, Some(0)).await.unwrap();
    assert!(task.history.is_empty());
}

#[tokio::test]
async fn e2e_tcp_get_task_not_found() {
    let url = start_server(CompletingExecutor).await;
    let client = A2aClient::new(&url);

    let err = client.get_task("nonexistent", None).await.unwrap_err();
    assert_eq!(err.status(), Some(404));
    assert_eq!(err.reason(), Some("TASK_NOT_FOUND"));
}

#[tokio::test]
async fn e2e_tcp_cancel_completed_task() {
    let url = start_server(CompletingExecutor).await;
    let client = A2aClient::new(&url);

    let resp = client.send_message(send_request("tcp-cancel-1", "complete")).await.unwrap();
    let task_id = response_task_id(&resp).unwrap().to_string();

    let err = client.cancel_task(&task_id).await.unwrap_err();
    assert_eq!(err.status(), Some(409));
    assert_eq!(err.reason(), Some("TASK_NOT_CANCELABLE"));
}

#[tokio::test]
async fn e2e_tcp_list_tasks_empty() {
    let url = start_server(CompletingExecutor).await;
    let client = A2aClient::new(&url);

    let resp = client.list_tasks(&ListTasksParams::default()).await.unwrap();
    assert!(resp.tasks.is_empty());
    assert_eq!(resp.total_size, 0);
}

#[tokio::test]
async fn e2e_tcp_list_tasks_pagination() {
    let url = start_server(CompletingExecutor).await;
    let client = A2aClient::new(&url);

    // Create 5 tasks
    for i in 0..5 {
        client.send_message(send_request(&format!("tcp-list-{i}"), "task")).await.unwrap();
    }

    // Paginate with page_size=2
    let mut all_ids = Vec::new();
    let mut page_token = None;
    loop {
        let resp = client
            .list_tasks(&ListTasksParams {
                page_size: Some(2),
                page_token: page_token.clone(),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(resp.total_size, 5);
        all_ids.extend(resp.tasks.iter().map(|t| t.id.clone()));

        if resp.next_page_token.is_empty() {
            break;
        }
        page_token = Some(resp.next_page_token);
    }

    assert_eq!(all_ids.len(), 5);
    // No duplicates
    let unique: std::collections::HashSet<_> = all_ids.iter().collect();
    assert_eq!(unique.len(), 5);
}

#[tokio::test]
async fn e2e_tcp_multi_turn_continuation() {
    let url = start_server(PausingExecutor).await;
    let client = A2aClient::new(&url);

    // First send: pauses at INPUT_REQUIRED
    let resp = client.send_message(send_request("tcp-multi-1", "start")).await.unwrap();
    let task = response_task(&resp).expect("Expected Task");
    assert_eq!(task.status().unwrap().state().unwrap(), TaskState::InputRequired);
    let task_id = task.id().to_string();

    // Second send with same task_id: completes
    let resp = client
        .send_message(send_request_with_task("tcp-multi-2", "continue", &task_id))
        .await
        .unwrap();

    let task = response_task(&resp).expect("Expected Task");
    assert_eq!(task.id(), task_id);
    assert_eq!(task.status().unwrap().state().unwrap(), TaskState::Completed);
}

#[tokio::test]
async fn e2e_tcp_tenant_isolation() {
    let url = start_server(CompletingExecutor).await;
    let client_alpha = A2aClient::new(&url).with_tenant("alpha");
    let client_beta = A2aClient::new(&url).with_tenant("beta");
    let client_default = A2aClient::new(&url);

    // Create task under alpha
    let resp = client_alpha.send_message(send_request("tcp-tenant-1", "alpha task")).await.unwrap();
    let task_id = response_task_id(&resp).unwrap().to_string();

    // Alpha can see it
    let task = client_alpha.get_task(&task_id, None).await.unwrap();
    assert_eq!(task.id, task_id);

    // Beta cannot see it
    let err = client_beta.get_task(&task_id, None).await.unwrap_err();
    assert_eq!(err.status(), Some(404));

    // Default tenant cannot see it
    let err = client_default.get_task(&task_id, None).await.unwrap_err();
    assert_eq!(err.status(), Some(404));

    // Alpha list has 1, default has 0
    let alpha_list = client_alpha.list_tasks(&ListTasksParams::default()).await.unwrap();
    assert_eq!(alpha_list.total_size, 1);

    let default_list = client_default.list_tasks(&ListTasksParams::default()).await.unwrap();
    assert_eq!(default_list.total_size, 0);
}

#[tokio::test]
async fn e2e_tcp_api_key_auth_accepted() {
    let url = start_server_with_api_key(CompletingExecutor, "test-secret").await;
    let client = A2aClient::new(&url).with_auth(ClientAuth::ApiKey {
        header: "X-API-Key".into(),
        key: "test-secret".into(),
    });

    let resp = client.send_message(send_request("tcp-auth-1", "authed")).await.unwrap();
    let task = response_task(&resp).expect("Expected Task");
    assert_eq!(task.status().unwrap().state().unwrap(), TaskState::Completed);
}

#[tokio::test]
async fn e2e_tcp_api_key_auth_rejected() {
    let url = start_server_with_api_key(CompletingExecutor, "test-secret").await;
    let client = A2aClient::new(&url); // No auth

    let err = client.send_message(send_request("tcp-auth-2", "no auth")).await.unwrap_err();
    assert_eq!(err.status(), Some(401));
}

#[tokio::test]
async fn e2e_tcp_error_envelope_format() {
    let url = start_server(CompletingExecutor).await;
    let client = A2aClient::new(&url);

    // Any error — 404 from get_task
    let err = client.get_task("missing", None).await.unwrap_err();
    match err {
        A2aClientError::A2aError { status, message, reason } => {
            assert_eq!(status, 404);
            assert!(!message.is_empty());
            assert_eq!(reason.as_deref(), Some("TASK_NOT_FOUND"));
        }
        other => panic!("Expected A2aError, got: {other}"),
    }
}

// =========================================================
// Streaming tests
// =========================================================

#[tokio::test]
async fn e2e_tcp_send_streaming_message() {
    let url = start_server(CompletingExecutor).await;
    let client = A2aClient::new(&url);

    let request = MessageBuilder::new().text("stream me").build();
    let mut stream = client.send_streaming_message(request).await.unwrap();

    let mut events = Vec::new();
    while let Some(result) = stream.next().await {
        let event = result.unwrap();
        events.push(event);
    }

    assert!(!events.is_empty(), "Should receive streaming events");

    // Events should have IDs in {task_id}:{sequence} format
    for (i, event) in events.iter().enumerate() {
        assert!(event.id.is_some(), "Event {i} should have an id");
        let id = event.id.as_ref().unwrap();
        assert!(id.contains(':'), "Event id should be task_id:sequence, got: {id}");
    }

    // Should see terminal event
    let has_completed = events.iter().any(|e| {
        e.data.get("statusUpdate")
            .and_then(|su| su.get("status"))
            .and_then(|s| s.get("state"))
            .and_then(|s| s.as_str())
            .is_some_and(|s| s == "TASK_STATE_COMPLETED")
    });
    assert!(has_completed, "Stream should include COMPLETED event");
}

#[tokio::test]
async fn e2e_tcp_subscribe_to_non_terminal_task() {
    let url = start_server(PausingExecutor).await;
    let client = A2aClient::new(&url);

    // Create a task that pauses at INPUT_REQUIRED
    let request = MessageBuilder::new().text("pause").build();
    let resp = client.send_message(request).await.unwrap();
    let task_id = response_task_id(&resp).unwrap().to_string();

    // Subscribe — should get Task snapshot as first event
    let mut stream = client.subscribe_to_task(&task_id, None).await.unwrap();

    // Collect first event (Task snapshot)
    let first = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        stream.next(),
    ).await;

    match first {
        Ok(Some(Ok(event))) => {
            assert!(
                event.data.get("task").is_some(),
                "First event should be Task snapshot, got: {}",
                event.data
            );
        }
        Ok(Some(Err(e))) => panic!("Stream error: {e}"),
        Ok(None) => panic!("Stream ended without events"),
        Err(_) => panic!("Timeout waiting for first event"),
    }
}

#[tokio::test]
async fn e2e_tcp_subscribe_terminal_task_returns_error() {
    let url = start_server(CompletingExecutor).await;
    let client = A2aClient::new(&url);

    // Create and complete a task
    let request = MessageBuilder::new().text("complete").build();
    let resp = client.send_message(request).await.unwrap();
    let task_id = response_task_id(&resp).unwrap().to_string();

    // Subscribe to terminal task — should error
    match client.subscribe_to_task(&task_id, None).await {
        Err(err) => {
            assert_eq!(err.status(), Some(400), "Terminal subscribe should return 400");
        }
        Ok(_) => panic!("Subscribe to terminal task should return error"),
    }
}

#[tokio::test]
async fn e2e_tcp_message_builder_ergonomics() {
    let url = start_server(CompletingExecutor).await;
    let client = A2aClient::new(&url);

    // MessageBuilder hides proto nesting
    let request = MessageBuilder::new()
        .text("hello from builder")
        .build();

    let resp = client.send_message(request).await.unwrap();
    let task = response_task(&resp).expect("Expected Task");
    assert!(!task.id().is_empty());
}
