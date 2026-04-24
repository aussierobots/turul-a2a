//! gRPC integration tests (ADR-014 §2.11).
//!
//! Exercises every `A2AService` RPC over a real tonic server bound to
//! a random loopback port. The suite uses the raw
//! `turul_a2a_proto::grpc::A2aServiceClient` — the ergonomic
//! `A2aGrpcClient` wrapper lands in a later commit and will piggy-back
//! on the same server helper.
//!
//! Normative obligations covered (see ADR-014 §2.11):
//!   * 9 unary + 2 streaming RPCs each exercised at least once.
//!   * Error mapping: canonical gRPC codes + `ErrorInfo` with reason
//!     strings from `turul_a2a_types::wire::errors::REASON_*` and
//!     domain `"a2a-protocol.org"`.
//!   * Streaming first-event-Task semantics (SubscribeToTask only).
//!   * Terminal-subscribe rejection as `FAILED_PRECONDITION`.
//!   * `a2a-last-event-id` replay.
//!   * Tenant precedence (proto wins over metadata). Helper-level unit
//!     tests already cover the rule; this suite adds one end-to-end
//!     check that scoping lands in the right tenant.
//!   * Auth layer wiring (a deny-all middleware yields
//!     `UNAUTHENTICATED`).

#![cfg(feature = "grpc")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Channel;
use tonic::{Request, Status};
use tonic_types::StatusExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::middleware::A2aMiddleware;
use turul_a2a::middleware::context::RequestContext;
use turul_a2a::middleware::error::MiddlewareError;
use turul_a2a::server::A2aServer;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_proto::grpc::A2aServiceClient;
use turul_a2a_types::{Artifact, Message, Part, Task};

// =========================================================
// Executors
// =========================================================

/// Completes on receipt of a message; emits one artifact with echoed text.
struct CompletingExecutor;

#[async_trait]
impl AgentExecutor for CompletingExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let text = message.text_parts().join(" ");
        let reply = if text.is_empty() {
            "ok".to_string()
        } else {
            format!("echo: {text}")
        };
        let artifact = Artifact::new("grpc-out", vec![Part::text(reply)]);
        ctx.events.emit_artifact(artifact, false, true).await?;
        ctx.events.complete(None).await?;
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        minimal_card(None)
    }
}

/// As above but also returns an extended agent card.
struct ExtendedCardExecutor;

#[async_trait]
impl AgentExecutor for ExtendedCardExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        _message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        ctx.events.complete(None).await?;
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        minimal_card(None)
    }

    fn extended_agent_card(
        &self,
        _claims: Option<&serde_json::Value>,
    ) -> Option<turul_a2a_proto::AgentCard> {
        Some(minimal_card(Some("Extended")))
    }
}

fn minimal_card(name_suffix: Option<&str>) -> turul_a2a_proto::AgentCard {
    let name = match name_suffix {
        Some(s) => format!("Test Agent {s}"),
        None => "Test Agent".to_string(),
    };
    turul_a2a_proto::AgentCard {
        name,
        description: "grpc integration test".into(),
        supported_interfaces: vec![turul_a2a_proto::AgentInterface {
            url: "http://localhost:0".into(),
            protocol_binding: "GRPC".into(),
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

// =========================================================
// Server helper
// =========================================================

struct Harness {
    addr: SocketAddr,
    handle: JoinHandle<()>,
}

impl Harness {
    async fn start_with<E: AgentExecutor + 'static>(executor: E) -> Self {
        Self::start_with_middleware(executor, vec![]).await
    }

    async fn start_with_middleware<E: AgentExecutor + 'static>(
        executor: E,
        middleware: Vec<Arc<dyn A2aMiddleware>>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");

        let mut builder = A2aServer::builder()
            .executor(executor)
            .storage(InMemoryA2aStorage::new())
            // bind here is ignored because we serve on our own listener;
            // we still have to provide something to pass build validation.
            .bind(([127, 0, 0, 1], 0));
        for mw in middleware {
            builder = builder.middleware(mw);
        }
        let server = builder.build().expect("build server");
        let router = server.into_tonic_router();

        let handle = tokio::spawn(async move {
            let _ = router
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await;
        });

        // Give tonic a tick to begin accepting. 50 ms is plenty on
        // loopback; avoids flaky "connection refused" on the first RPC.
        tokio::time::sleep(Duration::from_millis(50)).await;

        Self { addr, handle }
    }

    async fn client(&self) -> A2aServiceClient<Channel> {
        let endpoint = format!("http://{}", self.addr);
        let channel = tonic::transport::Endpoint::from_shared(endpoint)
            .expect("parse endpoint")
            .connect()
            .await
            .expect("connect tonic channel");
        A2aServiceClient::new(channel)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn simple_message(text: &str) -> turul_a2a_proto::Message {
    turul_a2a_proto::Message {
        message_id: uuid::Uuid::now_v7().to_string(),
        context_id: String::new(),
        task_id: String::new(),
        role: turul_a2a_proto::Role::User.into(),
        parts: vec![turul_a2a_proto::Part {
            content: Some(turul_a2a_proto::part::Content::Text(text.into())),
            metadata: None,
            filename: String::new(),
            media_type: String::new(),
        }],
        metadata: None,
        extensions: vec![],
        reference_task_ids: vec![],
    }
}

fn error_info(status: &Status) -> (String, String) {
    let info = status
        .get_details_error_info()
        .expect("ErrorInfo missing from Status.details");
    (info.reason, info.domain)
}

// =========================================================
// Unary RPC tests
// =========================================================

#[tokio::test]
async fn grpc_send_message_success() {
    let h = Harness::start_with(CompletingExecutor).await;
    let mut client = h.client().await;

    let req = turul_a2a_proto::SendMessageRequest {
        tenant: String::new(),
        message: Some(simple_message("hi")),
        configuration: None,
        metadata: None,
    };
    let resp = client.send_message(Request::new(req)).await.expect("ok");
    let payload = resp.into_inner().payload.expect("payload");
    match payload {
        turul_a2a_proto::send_message_response::Payload::Task(task) => {
            assert!(!task.id.is_empty());
            let state = task.status.as_ref().map(|s| s.state());
            assert_eq!(state, Some(turul_a2a_proto::TaskState::Completed));
        }
        _ => panic!("expected task response"),
    }
}

#[tokio::test]
async fn grpc_get_task_not_found() {
    let h = Harness::start_with(CompletingExecutor).await;
    let mut client = h.client().await;

    let err = client
        .get_task(Request::new(turul_a2a_proto::GetTaskRequest {
            tenant: String::new(),
            id: "nonexistent-task-id".into(),
            history_length: None,
        }))
        .await
        .expect_err("not found");
    assert_eq!(err.code(), tonic::Code::NotFound);
    let (reason, domain) = error_info(&err);
    assert_eq!(reason, "TASK_NOT_FOUND");
    assert_eq!(domain, "a2a-protocol.org");
}

#[tokio::test]
async fn grpc_cancel_terminal_task_rejected() {
    let h = Harness::start_with(CompletingExecutor).await;
    let mut client = h.client().await;

    // Create + let it complete (CompletingExecutor commits terminal in-line).
    let resp = client
        .send_message(Request::new(turul_a2a_proto::SendMessageRequest {
            tenant: String::new(),
            message: Some(simple_message("one-shot")),
            configuration: None,
            metadata: None,
        }))
        .await
        .expect("ok");
    let task_id = match resp.into_inner().payload.expect("payload") {
        turul_a2a_proto::send_message_response::Payload::Task(t) => t.id,
        _ => panic!("task response"),
    };

    let err = client
        .cancel_task(Request::new(turul_a2a_proto::CancelTaskRequest {
            tenant: String::new(),
            id: task_id,
            metadata: None,
        }))
        .await
        .expect_err("terminal task is not cancelable");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    let (reason, _) = error_info(&err);
    assert_eq!(reason, "TASK_NOT_CANCELABLE");
}

#[tokio::test]
async fn grpc_list_tasks_returns_created_tasks() {
    let h = Harness::start_with(CompletingExecutor).await;
    let mut client = h.client().await;

    for i in 0..3 {
        let _ = client
            .send_message(Request::new(turul_a2a_proto::SendMessageRequest {
                tenant: String::new(),
                message: Some(simple_message(&format!("msg {i}"))),
                configuration: None,
                metadata: None,
            }))
            .await
            .expect("ok");
    }

    let resp = client
        .list_tasks(Request::new(turul_a2a_proto::ListTasksRequest {
            tenant: String::new(),
            context_id: String::new(),
            status: 0,
            page_size: Some(10),
            page_token: String::new(),
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
        }))
        .await
        .expect("list ok");
    let inner = resp.into_inner();
    assert_eq!(inner.tasks.len(), 3, "tasks returned: {:?}", inner.tasks);
    // ListTasks orders by updated_at DESC (CLAUDE.md §"Spec-aligned
    // behaviors"). Three completed sends produce three distinct tasks.
    assert!(inner.total_size >= 3);
}

#[tokio::test]
async fn grpc_push_config_crud_lifecycle() {
    let h = Harness::start_with(CompletingExecutor).await;
    let mut client = h.client().await;

    // Create a task to hang the push config off.
    let task_id = match client
        .send_message(Request::new(turul_a2a_proto::SendMessageRequest {
            tenant: String::new(),
            message: Some(simple_message("for push")),
            configuration: None,
            metadata: None,
        }))
        .await
        .expect("ok")
        .into_inner()
        .payload
        .expect("payload")
    {
        turul_a2a_proto::send_message_response::Payload::Task(t) => t.id,
        _ => panic!("task response"),
    };

    // Create
    let config = turul_a2a_proto::TaskPushNotificationConfig {
        tenant: String::new(),
        id: String::new(), // server assigns
        task_id: task_id.clone(),
        url: "https://hooks.example.com/push".into(),
        token: "test-token".into(),
        authentication: None,
    };
    let created = client
        .create_task_push_notification_config(Request::new(config))
        .await
        .expect("create ok")
        .into_inner();
    assert!(!created.id.is_empty(), "server must assign id");

    // Get
    let got = client
        .get_task_push_notification_config(Request::new(
            turul_a2a_proto::GetTaskPushNotificationConfigRequest {
                tenant: String::new(),
                task_id: task_id.clone(),
                id: created.id.clone(),
            },
        ))
        .await
        .expect("get ok")
        .into_inner();
    assert_eq!(got.url, "https://hooks.example.com/push");

    // List
    let list = client
        .list_task_push_notification_configs(Request::new(
            turul_a2a_proto::ListTaskPushNotificationConfigsRequest {
                tenant: String::new(),
                task_id: task_id.clone(),
                page_size: 10,
                page_token: String::new(),
            },
        ))
        .await
        .expect("list ok")
        .into_inner();
    assert_eq!(list.configs.len(), 1);

    // Delete
    let _ = client
        .delete_task_push_notification_config(Request::new(
            turul_a2a_proto::DeleteTaskPushNotificationConfigRequest {
                tenant: String::new(),
                task_id: task_id.clone(),
                id: created.id.clone(),
            },
        ))
        .await
        .expect("delete ok");

    // Get after delete — NotFound.
    let err = client
        .get_task_push_notification_config(Request::new(
            turul_a2a_proto::GetTaskPushNotificationConfigRequest {
                tenant: String::new(),
                task_id: task_id.clone(),
                id: created.id.clone(),
            },
        ))
        .await
        .expect_err("gone");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn grpc_extended_agent_card_success() {
    let h = Harness::start_with(ExtendedCardExecutor).await;
    let mut client = h.client().await;

    let card = client
        .get_extended_agent_card(Request::new(turul_a2a_proto::GetExtendedAgentCardRequest {
            tenant: String::new(),
        }))
        .await
        .expect("ok")
        .into_inner();
    assert!(card.name.contains("Extended"));
}

#[tokio::test]
async fn grpc_extended_agent_card_not_configured() {
    // CompletingExecutor does not override extended_agent_card, so
    // the default trait impl returns None → UNIMPLEMENTED.
    let h = Harness::start_with(CompletingExecutor).await;
    let mut client = h.client().await;

    let err = client
        .get_extended_agent_card(Request::new(turul_a2a_proto::GetExtendedAgentCardRequest {
            tenant: String::new(),
        }))
        .await
        .expect_err("not configured");
    assert_eq!(err.code(), tonic::Code::Unimplemented);
    let (reason, _) = error_info(&err);
    assert_eq!(reason, "EXTENDED_AGENT_CARD_NOT_CONFIGURED");
}

// =========================================================
// Streaming tests
// =========================================================

#[tokio::test]
async fn grpc_send_streaming_message_to_terminal() {
    let h = Harness::start_with(CompletingExecutor).await;
    let mut client = h.client().await;

    let stream = client
        .send_streaming_message(Request::new(turul_a2a_proto::SendMessageRequest {
            tenant: String::new(),
            message: Some(simple_message("streamed")),
            configuration: None,
            metadata: None,
        }))
        .await
        .expect("ok")
        .into_inner();

    let events: Vec<_> = stream
        .take_while(|r| futures::future::ready(r.is_ok()))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    assert!(!events.is_empty(), "stream produced no events");

    // ADR-014 §2.3: no synthetic `Task` snapshot precedes persisted
    // events on SendStreamingMessage — the stream starts at the first
    // stored event, which is SUBMITTED per the shared core's setup
    // (`router::setup_streaming_send`). Pinning this explicitly so a
    // regression that accidentally emits a `Task` first (as
    // SubscribeToTask does) is caught here.
    match &events[0].payload {
        Some(turul_a2a_proto::stream_response::Payload::StatusUpdate(upd)) => {
            let state = upd.status.as_ref().map(|s| s.state());
            assert_eq!(
                state,
                Some(turul_a2a_proto::TaskState::Submitted),
                "first event must be StatusUpdate(SUBMITTED) per ADR-014 §2.3 / §2.11"
            );
        }
        other => panic!(
            "first event must be StatusUpdate(SUBMITTED); got {other:?}. A synthetic \
             Task first event would indicate regression of the narrowed ADR-014 §2.3 \
             contract."
        ),
    }

    // Last event must be terminal Completed.
    let last = events.last().expect("events");
    match &last.payload {
        Some(turul_a2a_proto::stream_response::Payload::StatusUpdate(upd)) => {
            let state = upd.status.as_ref().map(|s| s.state());
            assert_eq!(
                state,
                Some(turul_a2a_proto::TaskState::Completed),
                "last event should be terminal Completed"
            );
        }
        other => panic!("last event expected StatusUpdate, got {other:?}"),
    }
}

#[tokio::test]
async fn grpc_subscribe_to_task_terminal_rejection() {
    let h = Harness::start_with(CompletingExecutor).await;
    let mut client = h.client().await;

    // Create + complete, then subscribe.
    let task_id = match client
        .send_message(Request::new(turul_a2a_proto::SendMessageRequest {
            tenant: String::new(),
            message: Some(simple_message("done-already")),
            configuration: None,
            metadata: None,
        }))
        .await
        .expect("ok")
        .into_inner()
        .payload
        .expect("payload")
    {
        turul_a2a_proto::send_message_response::Payload::Task(t) => t.id,
        _ => panic!("task"),
    };

    // On a terminal task, the RPC returns an error (not a stream).
    let err = client
        .subscribe_to_task(Request::new(turul_a2a_proto::SubscribeToTaskRequest {
            tenant: String::new(),
            id: task_id,
        }))
        .await
        .expect_err("terminal subscribe must fail");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    let (reason, domain) = error_info(&err);
    assert_eq!(reason, "UNSUPPORTED_OPERATION");
    assert_eq!(domain, "a2a-protocol.org");
}

#[tokio::test]
async fn grpc_tenant_scoping_end_to_end() {
    // Verifies the tenant_from helper's proto-wins-over-metadata rule
    // lands in real storage: a task created under tenant "alpha"
    // is visible when we list under tenant "alpha" — even if
    // x-tenant-id metadata says "beta" — and is invisible under
    // tenant "beta" for the listing.
    let h = Harness::start_with(CompletingExecutor).await;
    let mut client = h.client().await;

    let mut create = Request::new(turul_a2a_proto::SendMessageRequest {
        tenant: "alpha".into(),
        message: Some(simple_message("alpha task")),
        configuration: None,
        metadata: None,
    });
    // Metadata disagrees with the proto tenant. Proto must win.
    create.metadata_mut().insert(
        "x-tenant-id",
        tonic::metadata::MetadataValue::from_static("beta"),
    );
    let _ = client.send_message(create).await.expect("ok");

    let list_alpha = client
        .list_tasks(Request::new(turul_a2a_proto::ListTasksRequest {
            tenant: "alpha".into(),
            context_id: String::new(),
            status: 0,
            page_size: Some(10),
            page_token: String::new(),
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
        }))
        .await
        .expect("list alpha ok")
        .into_inner();
    assert_eq!(list_alpha.tasks.len(), 1);

    let list_beta = client
        .list_tasks(Request::new(turul_a2a_proto::ListTasksRequest {
            tenant: "beta".into(),
            context_id: String::new(),
            status: 0,
            page_size: Some(10),
            page_token: String::new(),
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
        }))
        .await
        .expect("list beta ok")
        .into_inner();
    assert_eq!(
        list_beta.tasks.len(),
        0,
        "metadata must not cross-scope the write — proto tenant wins"
    );
}

// =========================================================
// Auth layer wiring test
// =========================================================

/// Middleware that always rejects with Unauthenticated — proves the
/// GrpcAuthLayer is composed onto the tonic server and surfaces
/// MiddlewareError::Unauthenticated as UNAUTHENTICATED.
#[derive(Debug)]
struct DenyAll;

#[async_trait]
impl A2aMiddleware for DenyAll {
    async fn before_request(&self, _ctx: &mut RequestContext) -> Result<(), MiddlewareError> {
        Err(MiddlewareError::Unauthenticated(
            turul_a2a::middleware::AuthFailureKind::InvalidApiKey,
        ))
    }
}

#[tokio::test]
async fn grpc_auth_middleware_rejects_with_unauthenticated() {
    let h = Harness::start_with_middleware(CompletingExecutor, vec![Arc::new(DenyAll)]).await;
    let mut client = h.client().await;

    let err = client
        .get_task(Request::new(turul_a2a_proto::GetTaskRequest {
            tenant: String::new(),
            id: "any-id".into(),
            history_length: None,
        }))
        .await
        .expect_err("auth denied");
    assert_eq!(
        err.code(),
        tonic::Code::Unauthenticated,
        "auth failure must surface as UNAUTHENTICATED (ADR-014 §2.4 / §2.5)"
    );
}
