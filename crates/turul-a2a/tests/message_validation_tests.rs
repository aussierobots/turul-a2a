//! Executor-defined message validation before acceptance.
//!
//! A2A v1.0.1 Section 3.3.2: servers MUST validate all input parameters
//! before processing, and a validation failure is HTTP 400, JSON-RPC
//! `-32602` or gRPC `INVALID_ARGUMENT`. `AgentExecutor::validate_message`
//! lets an agent add its own checks: a refused SendMessage or
//! SendStreamingMessage returns the executor's error on every transport,
//! creates no task, leaves a continued task untouched and never runs the
//! executor.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use http::{Method, Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext, ValidationContext};
use turul_a2a::server::A2aServer;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Message, Task};

const REFUSAL: &str = "field `at_utc`: not an RFC 3339 timestamp";

/// The message every transport carries for the refusal.
fn refusal() -> String {
    A2aError::InvalidRequest {
        message: REFUSAL.into(),
    }
    .to_string()
}

/// Refuses any message whose text is "bad". "ask" ends the turn
/// INPUT_REQUIRED; anything else completes. Counts `execute` calls.
struct Validates {
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentExecutor for Validates {
    async fn validate_message(
        &self,
        message: &Message,
        _ctx: &ValidationContext<'_>,
    ) -> Result<(), A2aError> {
        if message.text_parts().join(" ") == "bad" {
            return Err(A2aError::InvalidRequest {
                message: REFUSAL.into(),
            });
        }
        Ok(())
    }

    async fn execute(
        &self,
        _task: &mut Task,
        message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        if message.text_parts().join(" ") == "ask" {
            ctx.events.require_input(None).await?;
        } else {
            ctx.events.complete(None).await?;
        }
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            name: "message validation".into(),
            version: "1.0.0".into(),
            capabilities: Some(turul_a2a_proto::AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

fn server() -> (A2aServer, Arc<AtomicUsize>) {
    let executions = Arc::new(AtomicUsize::new(0));
    let server = A2aServer::builder()
        .executor(Validates {
            executions: executions.clone(),
        })
        .storage(InMemoryA2aStorage::new())
        .build()
        .expect("server build");
    (server, executions)
}

async fn http_call(
    router: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("A2A-Version", "1.0")
        .header("content-type", "application/json")
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
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    http_call(router, Method::POST, "/jsonrpc", Some(body))
        .await
        .1
}

fn send_body(text: &str, task_id: Option<&str>) -> Value {
    let mut message = json!({
        "messageId": uuid::Uuid::now_v7().to_string(),
        "role": "ROLE_USER",
        "parts": [{"text": text}],
    });
    if let Some(id) = task_id {
        message["taskId"] = json!(id);
    }
    json!({"message": message})
}

async fn task_count(router: &axum::Router) -> u64 {
    let (status, page) = http_call(router, Method::GET, "/tasks", None).await;
    assert_eq!(status, 200, "{page}");
    page["totalSize"].as_u64().unwrap()
}

#[tokio::test]
async fn refused_new_message_creates_nothing_on_http_and_jsonrpc() {
    let (server, executions) = server();
    let router = server.into_router();

    for path in ["/message:send", "/message:stream"] {
        let (status, body) =
            http_call(&router, Method::POST, path, Some(send_body("bad", None))).await;
        assert_eq!(status, 400, "POST {path}: {body}");
        assert_eq!(
            body["error"]["status"], "INVALID_ARGUMENT",
            "{path}: {body}"
        );
        assert_eq!(body["error"]["message"], refusal(), "{path}: {body}");
    }
    for method in ["SendMessage", "SendStreamingMessage"] {
        let resp = jsonrpc(&router, method, send_body("bad", None)).await;
        assert_eq!(resp["error"]["code"], -32602, "{method}: {resp}");
        assert_eq!(resp["error"]["message"], refusal(), "{method}: {resp}");
    }

    assert_eq!(
        task_count(&router).await,
        0,
        "a refused send creates no task"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 0, "execute never ran");

    // A message the validator accepts runs as before.
    let (status, body) = http_call(
        &router,
        Method::POST,
        "/message:send",
        Some(send_body("go", None)),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["task"]["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn refused_follow_up_leaves_the_task_untouched() {
    let (server, executions) = server();
    let router = server.into_router();

    let (status, body) = http_call(
        &router,
        Method::POST,
        "/message:send",
        Some(send_body("ask", None)),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["task"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED");
    let task_id = body["task"]["id"].as_str().unwrap().to_string();
    let history_before = body["task"]["history"].as_array().map_or(0, Vec::len);

    let (status, body) = http_call(
        &router,
        Method::POST,
        "/message:send",
        Some(send_body("bad", Some(&task_id))),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["message"], refusal(), "{body}");

    let (status, task) = http_call(&router, Method::GET, &format!("/tasks/{task_id}"), None).await;
    assert_eq!(status, 200, "{task}");
    assert_eq!(task["status"]["state"], "TASK_STATE_INPUT_REQUIRED");
    assert_eq!(
        task["history"].as_array().map_or(0, Vec::len),
        history_before,
        "the refused message is not appended"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "only the first turn ran"
    );
}

#[cfg(feature = "grpc")]
#[tokio::test]
async fn refused_message_is_invalid_argument_on_grpc() {
    use tokio_stream::wrappers::TcpListenerStream;
    use turul_a2a_proto as pb;

    let (server, executions) = server();
    let (router, tonic_router) = server.into_routers();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve = tokio::spawn(async move {
        let _ = tonic_router
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await;
    });
    common::wait_grpc_ready(addr).await;
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client: pb::grpc::A2aServiceClient<common::VersionedChannel> =
        pb::grpc::A2aServiceClient::with_interceptor(channel, common::with_a2a_version as _);

    let send: pb::SendMessageRequest = serde_json::from_value(send_body("bad", None)).unwrap();
    let blocking = client.send_message(send.clone()).await.map(|_| ());
    let streaming = client.send_streaming_message(send).await.map(|_| ());
    for (rpc, outcome) in [
        ("SendMessage", blocking),
        ("SendStreamingMessage", streaming),
    ] {
        let status = outcome.expect_err(rpc);
        assert_eq!(
            status.code(),
            tonic::Code::InvalidArgument,
            "{rpc}: {status:?}"
        );
        assert_eq!(status.message(), refusal(), "{rpc}: {status:?}");
    }
    assert_eq!(
        task_count(&router).await,
        0,
        "a refused send creates no task"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 0, "execute never ran");
    serve.abort();
}
