//! Capability validation (A2A v1.0.1 §3.3.4) on every transport.
//!
//! "When clients attempt to use operations or features that require
//! capabilities not declared as supported in the Agent Card, the agent
//! MUST return an appropriate error response":
//!
//! - `capabilities.pushNotifications` false or not present → push config
//!   Create / Get / List / Delete return `PushNotificationNotSupportedError`.
//!   A send carrying an inline `configuration.taskPushNotificationConfig`
//!   is refused the same way, before any task is created.
//! - `capabilities.streaming` false or not present → `SendStreamingMessage`
//!   and `SubscribeToTask` return `UnsupportedOperationError`.
//! - `capabilities.extendedAgentCard` false or not present →
//!   `GetExtendedAgentCard` returns `UnsupportedOperationError`; declared
//!   but not configured → `ExtendedAgentCardNotConfiguredError`.
//!
//! Each rule is exercised for a card that sets the flag `false` and for
//! one that leaves it unset, over HTTP+JSON, JSON-RPC and gRPC.

mod common;

use async_trait::async_trait;
use axum::body::Body;
use http::{Method, Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::server::A2aServer;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::wire::{errors, jsonrpc as methods};
use turul_a2a_types::{Message, Task};

/// How a card leaves a capability unsupported.
#[derive(Debug, Clone, Copy)]
enum Unsupported {
    False,
    Absent,
}

impl Unsupported {
    fn flag(self) -> Option<bool> {
        match self {
            Self::False => Some(false),
            Self::Absent => None,
        }
    }
}

const BOTH: [Unsupported; 2] = [Unsupported::False, Unsupported::Absent];

#[derive(Clone)]
struct CapabilityAgent {
    streaming: Option<bool>,
    push_notifications: Option<bool>,
    extended_agent_card: Option<bool>,
    /// Whether the executor supplies an extended card.
    serves_extended_card: bool,
}

impl CapabilityAgent {
    /// Every optional capability declared, extended card served.
    fn all_declared() -> Self {
        Self {
            streaming: Some(true),
            push_notifications: Some(true),
            extended_agent_card: Some(true),
            serves_extended_card: true,
        }
    }

    fn card(&self, name: &str) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            name: name.into(),
            description: "capability validation fixture".into(),
            version: "1.0.0".into(),
            supported_interfaces: vec![turul_a2a_proto::AgentInterface {
                url: "http://localhost".into(),
                protocol_binding: "JSONRPC".into(),
                tenant: String::new(),
                protocol_version: "1.0".into(),
            }],
            capabilities: Some(turul_a2a_proto::AgentCapabilities {
                streaming: self.streaming,
                push_notifications: self.push_notifications,
                extended_agent_card: self.extended_agent_card,
                extensions: vec![],
            }),
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            ..Default::default()
        }
    }
}

#[async_trait]
impl AgentExecutor for CapabilityAgent {
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
        self.card("Capability Agent")
    }

    fn extended_agent_card(
        &self,
        _claims: Option<&serde_json::Value>,
    ) -> Option<turul_a2a_proto::AgentCard> {
        self.serves_extended_card
            .then(|| self.card("Capability Agent (extended)"))
    }
}

fn router(agent: CapabilityAgent) -> axum::Router {
    A2aServer::builder()
        .executor(agent)
        .storage(InMemoryA2aStorage::new())
        .build()
        .expect("server builds")
        .into_router()
}

async fn call(
    router: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn jsonrpc(router: &axum::Router, method: &str, params: Value) -> Value {
    let (_, body) = call(
        router,
        Method::POST,
        "/jsonrpc",
        Some(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})),
    )
    .await;
    body
}

fn send_body(push_url: Option<&str>) -> Value {
    let mut body = json!({
        "message": {
            "messageId": uuid::Uuid::now_v7().to_string(),
            "role": "ROLE_USER",
            "parts": [{"text": "hi"}],
        },
    });
    if let Some(url) = push_url {
        body["configuration"] = json!({"taskPushNotificationConfig": {"url": url}});
    }
    body
}

/// Create a task through a plain blocking send and return its id.
async fn existing_task(router: &axum::Router) -> String {
    let (status, body) = call(router, Method::POST, "/message:send", Some(send_body(None))).await;
    assert_eq!(status, 200, "plain send succeeds: {body}");
    body["task"]["id"].as_str().unwrap().to_string()
}

async fn task_count(router: &axum::Router) -> usize {
    let (status, body) = call(router, Method::GET, "/tasks", None).await;
    assert_eq!(status, 200, "{body}");
    body["tasks"].as_array().map_or(0, Vec::len)
}

fn assert_http_error(status: u16, body: &Value, http: u16, reason: &str, context: &str) {
    assert_eq!(status, http, "{context}: {body}");
    assert_eq!(
        body["error"]["details"][0]["reason"], reason,
        "{context}: {body}"
    );
    assert_eq!(
        body["error"]["status"], "FAILED_PRECONDITION",
        "{context}: {body}"
    );
}

fn assert_jsonrpc_error(body: &Value, code: i32, reason: &str, context: &str) {
    assert_eq!(body["error"]["code"], code, "{context}: {body}");
    assert_eq!(
        body["error"]["data"][0]["reason"], reason,
        "{context}: {body}"
    );
}

fn push_off(unsupported: Unsupported) -> CapabilityAgent {
    CapabilityAgent {
        push_notifications: unsupported.flag(),
        ..CapabilityAgent::all_declared()
    }
}

fn streaming_off(unsupported: Unsupported) -> CapabilityAgent {
    CapabilityAgent {
        streaming: unsupported.flag(),
        ..CapabilityAgent::all_declared()
    }
}

fn extended_off(unsupported: Unsupported) -> CapabilityAgent {
    CapabilityAgent {
        extended_agent_card: unsupported.flag(),
        ..CapabilityAgent::all_declared()
    }
}

// =========================================================
// Push notifications
// =========================================================

#[tokio::test]
async fn http_push_config_crud_requires_push_capability() {
    for unsupported in BOTH {
        let router = router(push_off(unsupported));
        let task_id = existing_task(&router).await;
        let base = format!("/tasks/{task_id}/pushNotificationConfigs");
        let item = format!("{base}/cfg-1");
        for (method, uri, body) in [
            (
                Method::POST,
                base.as_str(),
                Some(json!({"url": "https://example.test/hook"})),
            ),
            (Method::GET, base.as_str(), None),
            (Method::GET, item.as_str(), None),
            (Method::DELETE, item.as_str(), None),
        ] {
            let context = format!("{unsupported:?} {method} {uri}");
            let (status, body) = call(&router, method, uri, body).await;
            assert_http_error(
                status,
                &body,
                errors::HTTP_PUSH_NOTIFICATION_NOT_SUPPORTED,
                errors::REASON_PUSH_NOTIFICATION_NOT_SUPPORTED,
                &context,
            );
        }
    }
}

#[tokio::test]
async fn jsonrpc_push_config_crud_requires_push_capability() {
    for unsupported in BOTH {
        let router = router(push_off(unsupported));
        let task_id = existing_task(&router).await;
        for (method, params) in [
            (
                methods::CREATE_TASK_PUSH_NOTIFICATION_CONFIG,
                json!({"taskId": task_id, "url": "https://example.test/hook"}),
            ),
            (
                methods::GET_TASK_PUSH_NOTIFICATION_CONFIG,
                json!({"taskId": task_id, "id": "cfg-1"}),
            ),
            (
                methods::LIST_TASK_PUSH_NOTIFICATION_CONFIGS,
                json!({"taskId": task_id}),
            ),
            (
                methods::DELETE_TASK_PUSH_NOTIFICATION_CONFIG,
                json!({"taskId": task_id, "id": "cfg-1"}),
            ),
        ] {
            let body = jsonrpc(&router, method, params).await;
            assert_jsonrpc_error(
                &body,
                errors::JSONRPC_PUSH_NOTIFICATION_NOT_SUPPORTED,
                errors::REASON_PUSH_NOTIFICATION_NOT_SUPPORTED,
                &format!("{unsupported:?} {method}"),
            );
        }
    }
}

/// An inline push config on an agent without push capability is refused
/// before the task is created, on blocking and streaming sends alike.
#[tokio::test]
async fn inline_push_config_requires_push_capability_and_persists_nothing() {
    for unsupported in BOTH {
        let router = router(push_off(unsupported));
        let request = send_body(Some("https://example.test/hook"));

        for path in ["/message:send", "/message:stream"] {
            let (status, body) = call(&router, Method::POST, path, Some(request.clone())).await;
            assert_http_error(
                status,
                &body,
                errors::HTTP_PUSH_NOTIFICATION_NOT_SUPPORTED,
                errors::REASON_PUSH_NOTIFICATION_NOT_SUPPORTED,
                &format!("{unsupported:?} {path}"),
            );
        }
        for method in [methods::SEND_MESSAGE, methods::SEND_STREAMING_MESSAGE] {
            let body = jsonrpc(&router, method, request.clone()).await;
            assert_jsonrpc_error(
                &body,
                errors::JSONRPC_PUSH_NOTIFICATION_NOT_SUPPORTED,
                errors::REASON_PUSH_NOTIFICATION_NOT_SUPPORTED,
                &format!("{unsupported:?} {method}"),
            );
        }
        assert_eq!(
            task_count(&router).await,
            0,
            "{unsupported:?}: no task persisted"
        );
    }
}

#[tokio::test]
async fn push_capability_declared_accepts_push_config_crud() {
    let router = router(CapabilityAgent::all_declared());
    let task_id = existing_task(&router).await;
    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/tasks/{task_id}/pushNotificationConfigs"),
        Some(json!({"url": "https://example.test/hook"})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

// =========================================================
// Streaming
// =========================================================

#[tokio::test]
async fn http_streaming_requires_streaming_capability() {
    for unsupported in BOTH {
        let router = router(streaming_off(unsupported));
        let task_id = existing_task(&router).await;
        let subscribe = format!("/tasks/{task_id}:subscribe");
        for (method, uri, body) in [
            (Method::POST, "/message:stream", Some(send_body(None))),
            (Method::GET, subscribe.as_str(), None),
            (Method::POST, subscribe.as_str(), None),
        ] {
            let context = format!("{unsupported:?} {method} {uri}");
            let (status, body) = call(&router, method, uri, body).await;
            assert_http_error(
                status,
                &body,
                errors::HTTP_UNSUPPORTED_OPERATION,
                errors::REASON_UNSUPPORTED_OPERATION,
                &context,
            );
        }
        assert_eq!(
            task_count(&router).await,
            1,
            "{unsupported:?}: the refused stream created no task"
        );
    }
}

#[tokio::test]
async fn jsonrpc_streaming_requires_streaming_capability() {
    for unsupported in BOTH {
        let router = router(streaming_off(unsupported));
        let task_id = existing_task(&router).await;
        for (method, params) in [
            (methods::SEND_STREAMING_MESSAGE, send_body(None)),
            (methods::SUBSCRIBE_TO_TASK, json!({"id": task_id})),
        ] {
            let body = jsonrpc(&router, method, params).await;
            assert_jsonrpc_error(
                &body,
                errors::JSONRPC_UNSUPPORTED_OPERATION,
                errors::REASON_UNSUPPORTED_OPERATION,
                &format!("{unsupported:?} {method}"),
            );
        }
        assert_eq!(task_count(&router).await, 1, "{unsupported:?}");
    }
}

// =========================================================
// Extended agent card
// =========================================================

#[tokio::test]
async fn extended_card_requires_extended_card_capability() {
    for unsupported in BOTH {
        // The executor serves an extended card, but the public card does
        // not declare the capability.
        let router = router(extended_off(unsupported));
        let (status, body) = call(&router, Method::GET, "/extendedAgentCard", None).await;
        assert_http_error(
            status,
            &body,
            errors::HTTP_UNSUPPORTED_OPERATION,
            errors::REASON_UNSUPPORTED_OPERATION,
            &format!("{unsupported:?} HTTP"),
        );
        let body = jsonrpc(&router, methods::GET_EXTENDED_AGENT_CARD, json!({})).await;
        assert_jsonrpc_error(
            &body,
            errors::JSONRPC_UNSUPPORTED_OPERATION,
            errors::REASON_UNSUPPORTED_OPERATION,
            &format!("{unsupported:?} JSON-RPC"),
        );
    }
}

#[tokio::test]
async fn extended_card_declared_but_not_configured() {
    let router = router(CapabilityAgent {
        serves_extended_card: false,
        ..CapabilityAgent::all_declared()
    });
    let (status, body) = call(&router, Method::GET, "/extendedAgentCard", None).await;
    assert_http_error(
        status,
        &body,
        errors::HTTP_EXTENDED_AGENT_CARD_NOT_CONFIGURED,
        errors::REASON_EXTENDED_AGENT_CARD_NOT_CONFIGURED,
        "HTTP",
    );
    let body = jsonrpc(&router, methods::GET_EXTENDED_AGENT_CARD, json!({})).await;
    assert_jsonrpc_error(
        &body,
        errors::JSONRPC_EXTENDED_AGENT_CARD_NOT_CONFIGURED,
        errors::REASON_EXTENDED_AGENT_CARD_NOT_CONFIGURED,
        "JSON-RPC",
    );
}

#[tokio::test]
async fn extended_card_declared_and_configured_is_served() {
    let router = router(CapabilityAgent::all_declared());
    let (status, body) = call(&router, Method::GET, "/extendedAgentCard", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["name"], "Capability Agent (extended)");
}

// =========================================================
// gRPC
// =========================================================

#[cfg(feature = "grpc")]
mod grpc {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::{Code, Request, Status};
    use tonic_types::StatusExt;
    use turul_a2a_proto as pb;
    use turul_a2a_proto::grpc::A2aServiceClient;

    async fn client(agent: CapabilityAgent) -> A2aServiceClient<common::VersionedChannel> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = A2aServer::builder()
            .executor(agent)
            .storage(InMemoryA2aStorage::new())
            .bind(([127, 0, 0, 1], 0))
            .build()
            .unwrap()
            .into_tonic_router();
        tokio::spawn(async move {
            let _ = router
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await;
        });
        common::wait_grpc_ready(addr).await;
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect_lazy();
        A2aServiceClient::with_interceptor(channel, common::with_a2a_version as _)
    }

    fn assert_status(status: &Status, reason: &str, context: &str) {
        assert_eq!(
            status.code(),
            Code::FailedPrecondition,
            "{context}: {status:?}"
        );
        let info = status
            .get_details_error_info()
            .expect("ErrorInfo in Status.details");
        assert_eq!(info.reason, reason, "{context}");
    }

    fn send_request(push_url: Option<&str>) -> pb::SendMessageRequest {
        serde_json::from_value(send_body(push_url)).unwrap()
    }

    async fn existing_task(client: &mut A2aServiceClient<common::VersionedChannel>) -> String {
        let resp = client
            .send_message(send_request(None))
            .await
            .expect("plain send")
            .into_inner();
        match resp.payload {
            Some(pb::send_message_response::Payload::Task(task)) => task.id,
            other => panic!("expected a task, got {other:?}"),
        }
    }

    async fn task_count(client: &mut A2aServiceClient<common::VersionedChannel>) -> usize {
        client
            .list_tasks(pb::ListTasksRequest::default())
            .await
            .expect("list tasks")
            .into_inner()
            .tasks
            .len()
    }

    #[tokio::test]
    async fn grpc_push_config_crud_requires_push_capability() {
        for unsupported in BOTH {
            let mut client = client(push_off(unsupported)).await;
            let task_id = existing_task(&mut client).await;
            let reason = errors::REASON_PUSH_NOTIFICATION_NOT_SUPPORTED;

            let err = client
                .create_task_push_notification_config(pb::TaskPushNotificationConfig {
                    task_id: task_id.clone(),
                    url: "https://example.test/hook".into(),
                    ..Default::default()
                })
                .await
                .expect_err("create");
            assert_status(&err, reason, &format!("{unsupported:?} create"));

            let err = client
                .get_task_push_notification_config(pb::GetTaskPushNotificationConfigRequest {
                    task_id: task_id.clone(),
                    id: "cfg-1".into(),
                    ..Default::default()
                })
                .await
                .expect_err("get");
            assert_status(&err, reason, &format!("{unsupported:?} get"));

            let err = client
                .list_task_push_notification_configs(pb::ListTaskPushNotificationConfigsRequest {
                    task_id: task_id.clone(),
                    ..Default::default()
                })
                .await
                .expect_err("list");
            assert_status(&err, reason, &format!("{unsupported:?} list"));

            let err = client
                .delete_task_push_notification_config(pb::DeleteTaskPushNotificationConfigRequest {
                    task_id: task_id.clone(),
                    id: "cfg-1".into(),
                    ..Default::default()
                })
                .await
                .expect_err("delete");
            assert_status(&err, reason, &format!("{unsupported:?} delete"));
        }
    }

    #[tokio::test]
    async fn grpc_inline_push_config_requires_push_capability_and_persists_nothing() {
        for unsupported in BOTH {
            let mut client = client(push_off(unsupported)).await;
            let reason = errors::REASON_PUSH_NOTIFICATION_NOT_SUPPORTED;
            let request = send_request(Some("https://example.test/hook"));

            let err = client
                .send_message(request.clone())
                .await
                .expect_err("send");
            assert_status(&err, reason, &format!("{unsupported:?} send"));
            let err = client
                .send_streaming_message(request)
                .await
                .expect_err("stream");
            assert_status(&err, reason, &format!("{unsupported:?} stream"));
            assert_eq!(task_count(&mut client).await, 0, "{unsupported:?}");
        }
    }

    #[tokio::test]
    async fn grpc_streaming_requires_streaming_capability() {
        for unsupported in BOTH {
            let mut client = client(streaming_off(unsupported)).await;
            let task_id = existing_task(&mut client).await;
            let reason = errors::REASON_UNSUPPORTED_OPERATION;

            let err = client
                .send_streaming_message(send_request(None))
                .await
                .expect_err("stream");
            assert_status(&err, reason, &format!("{unsupported:?} stream"));
            let err = client
                .subscribe_to_task(pb::SubscribeToTaskRequest {
                    id: task_id,
                    ..Default::default()
                })
                .await
                .expect_err("subscribe");
            assert_status(&err, reason, &format!("{unsupported:?} subscribe"));
            assert_eq!(task_count(&mut client).await, 1, "{unsupported:?}");
        }
    }

    #[tokio::test]
    async fn grpc_extended_card_requires_extended_card_capability() {
        for unsupported in BOTH {
            let mut client = client(extended_off(unsupported)).await;
            let err = client
                .get_extended_agent_card(Request::new(pb::GetExtendedAgentCardRequest::default()))
                .await
                .expect_err("extended card");
            assert_status(
                &err,
                errors::REASON_UNSUPPORTED_OPERATION,
                &format!("{unsupported:?}"),
            );
        }

        let mut client = client(CapabilityAgent {
            serves_extended_card: false,
            ..CapabilityAgent::all_declared()
        })
        .await;
        let err = client
            .get_extended_agent_card(Request::new(pb::GetExtendedAgentCardRequest::default()))
            .await
            .expect_err("declared but not configured");
        assert_status(
            &err,
            errors::REASON_EXTENDED_AGENT_CARD_NOT_CONFIGURED,
            "not configured",
        );
    }
}
