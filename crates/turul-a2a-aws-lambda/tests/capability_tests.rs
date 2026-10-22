//! Capability validation (A2A v1.0.1 §3.3.4) through the Lambda adapter.
//!
//! The adapter serves the shared router, so an agent card that does not
//! declare `pushNotifications`, `streaming` or `extendedAgentCard` gets
//! the same refusals as the long-lived server: push-config CRUD and an
//! inline `taskPushNotificationConfig` → `PushNotificationNotSupportedError`
//! (the latter before any task is created), JSON-RPC streaming methods
//! and `GetExtendedAgentCard` → `UnsupportedOperationError`.

use async_trait::async_trait;
use serde_json::{Value, json};

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_aws_lambda::LambdaA2aHandler;
use turul_a2a_types::wire::errors;
use turul_a2a_types::{Message, Task};

/// Declares no optional capability (all three flags unset) but serves an
/// extended card, so the extended-card refusal comes from the flag alone.
struct NoCapabilities;

#[async_trait]
impl AgentExecutor for NoCapabilities {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        ctx.events.complete(None).await?;
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            name: "no-capabilities".into(),
            capabilities: Some(turul_a2a_proto::AgentCapabilities::default()),
            ..Default::default()
        }
    }

    fn extended_agent_card(&self, _claims: Option<&Value>) -> Option<turul_a2a_proto::AgentCard> {
        Some(self.agent_card())
    }
}

fn handler() -> LambdaA2aHandler {
    LambdaA2aHandler::builder()
        .executor(NoCapabilities)
        .storage(InMemoryA2aStorage::new())
        .build()
        .expect("lambda handler builds")
}

async fn call(
    handler: &LambdaA2aHandler,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let req = http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(body.map_or(lambda_http::Body::Empty, |b| {
            lambda_http::Body::Text(b.to_string())
        }))
        .unwrap();
    let resp = handler.handle(req).await.expect("handle");
    let status = resp.status().as_u16();
    let body = serde_json::from_slice(resp.body().as_ref()).unwrap_or(Value::Null);
    (status, body)
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

fn assert_refused(status: u16, body: &Value, reason: &str, context: &str) {
    assert_eq!(status, 400, "{context}: {body}");
    assert_eq!(
        body["error"]["details"][0]["reason"], reason,
        "{context}: {body}"
    );
}

#[tokio::test]
async fn lambda_push_config_crud_requires_push_capability() {
    let handler = handler();
    let (status, sent) = call(&handler, "POST", "/message:send", Some(send_body(None))).await;
    assert_eq!(status, 200, "{sent}");
    let task_id = sent["task"]["id"].as_str().unwrap().to_string();
    let base = format!("/tasks/{task_id}/pushNotificationConfigs");
    let item = format!("{base}/cfg-1");

    for (method, uri, body) in [
        (
            "POST",
            base.as_str(),
            Some(json!({"url": "https://example.test/hook"})),
        ),
        ("GET", base.as_str(), None),
        ("GET", item.as_str(), None),
        ("DELETE", item.as_str(), None),
    ] {
        let (status, body) = call(&handler, method, uri, body).await;
        assert_refused(
            status,
            &body,
            errors::REASON_PUSH_NOTIFICATION_NOT_SUPPORTED,
            &format!("{method} {uri}"),
        );
    }
}

#[tokio::test]
async fn lambda_inline_push_config_is_refused_before_task_creation() {
    let handler = handler();
    let (status, body) = call(
        &handler,
        "POST",
        "/message:send",
        Some(send_body(Some("https://example.test/hook"))),
    )
    .await;
    assert_refused(
        status,
        &body,
        errors::REASON_PUSH_NOTIFICATION_NOT_SUPPORTED,
        "inline push config",
    );

    let (status, listed) = call(&handler, "GET", "/tasks", None).await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(listed["tasks"], json!([]), "no task persisted: {listed}");
}

#[tokio::test]
async fn lambda_jsonrpc_streaming_requires_streaming_capability() {
    let handler = handler();
    let (_, body) = call(
        &handler,
        "POST",
        "/jsonrpc",
        Some(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "SendStreamingMessage",
            "params": send_body(None),
        })),
    )
    .await;
    assert_eq!(
        body["error"]["code"],
        errors::JSONRPC_UNSUPPORTED_OPERATION,
        "{body}"
    );
}

#[tokio::test]
async fn lambda_extended_card_requires_extended_card_capability() {
    let handler = handler();
    let (status, body) = call(&handler, "GET", "/extendedAgentCard", None).await;
    assert_refused(
        status,
        &body,
        errors::REASON_UNSUPPORTED_OPERATION,
        "extended card",
    );
}
