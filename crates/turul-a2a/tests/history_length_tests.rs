//! `historyLength` validation on every operation and transport
//! (A2A v1.0.1).
//!
//! Section 3.2.4 gives `historyLength` three meanings: unset, `0`, or a
//! positive count. The specification's validation-error example rejects
//! `historyLength=-5` with "Must be non-negative integer". A negative
//! value is therefore an invalid request on GetTask, ListTasks and the
//! `SendMessageConfiguration` of SendMessage / SendStreamingMessage, on
//! HTTP+JSON, JSON-RPC and (with `grpc`) gRPC; a refused send writes
//! nothing.

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
use turul_a2a_types::{Message, Task};

/// Completes every turn.
struct Completes;

#[async_trait]
impl AgentExecutor for Completes {
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
            name: "history length".into(),
            version: "1.0.0".into(),
            capabilities: Some(turul_a2a_proto::AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

fn server(storage: InMemoryA2aStorage) -> A2aServer {
    A2aServer::builder()
        .executor(Completes)
        .storage(storage)
        .build()
        .expect("server build")
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

fn send_body(history_length: i32) -> Value {
    json!({
        "message": {"messageId": uuid::Uuid::now_v7().to_string(), "role": "ROLE_USER", "parts": [{"text": "go"}]},
        "configuration": {"historyLength": history_length},
    })
}

/// A completed task to read back; returns its id.
async fn seed_task(router: &axum::Router) -> String {
    let (status, body) = http_call(router, Method::POST, "/message:send", Some(send_body(1))).await;
    assert_eq!(status, 200, "{body}");
    body["task"]["id"].as_str().unwrap().to_string()
}

async fn task_count(router: &axum::Router) -> u64 {
    let (status, page) = http_call(router, Method::GET, "/tasks", None).await;
    assert_eq!(status, 200, "{page}");
    page["totalSize"].as_u64().unwrap()
}

#[tokio::test]
async fn negative_history_length_is_invalid_on_http_and_jsonrpc() {
    let router = server(InMemoryA2aStorage::new()).into_router();
    let task_id = seed_task(&router).await;

    for uri in [
        format!("/tasks/{task_id}?historyLength=-5"),
        "/tasks?historyLength=-5".to_string(),
    ] {
        let (status, body) = http_call(&router, Method::GET, &uri, None).await;
        assert_eq!(status, 400, "GET {uri}: {body}");
        assert_eq!(
            body["error"]["status"], "INVALID_ARGUMENT",
            "GET {uri}: {body}"
        );
    }
    for path in ["/message:send", "/message:stream"] {
        let (status, body) = http_call(&router, Method::POST, path, Some(send_body(-5))).await;
        assert_eq!(status, 400, "POST {path}: {body}");
        assert_eq!(
            body["error"]["status"], "INVALID_ARGUMENT",
            "POST {path}: {body}"
        );
    }

    for (method, params) in [
        ("GetTask", json!({"id": task_id, "historyLength": -5})),
        ("ListTasks", json!({"historyLength": -5})),
        ("SendMessage", send_body(-5)),
        ("SendStreamingMessage", send_body(-5)),
    ] {
        let resp = jsonrpc(&router, method, params).await;
        assert_eq!(resp["error"]["code"], -32602, "{method}: {resp}");
    }

    assert_eq!(
        task_count(&router).await,
        1,
        "a refused send writes nothing"
    );

    // Zero is a valid length: no history.
    let (status, body) = http_call(
        &router,
        Method::GET,
        &format!("/tasks/{task_id}?historyLength=0"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

#[cfg(feature = "grpc")]
#[tokio::test]
async fn negative_history_length_is_invalid_on_grpc() {
    use tokio_stream::wrappers::TcpListenerStream;
    use turul_a2a_proto as pb;

    let (router, tonic_router) = server(InMemoryA2aStorage::new()).into_routers();
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

    let task_id = seed_task(&router).await;
    let send: pb::SendMessageRequest = serde_json::from_value(send_body(-5)).unwrap();

    let get = client
        .get_task(pb::GetTaskRequest {
            tenant: String::new(),
            id: task_id,
            history_length: Some(-5),
        })
        .await
        .map(|_| ());
    let list = client
        .list_tasks(pb::ListTasksRequest {
            history_length: Some(-5),
            ..Default::default()
        })
        .await
        .map(|_| ());
    let blocking = client.send_message(send.clone()).await.map(|_| ());
    let streaming = client.send_streaming_message(send).await.map(|_| ());

    for (rpc, outcome) in [
        ("GetTask", get),
        ("ListTasks", list),
        ("SendMessage", blocking),
        ("SendStreamingMessage", streaming),
    ] {
        let status = outcome.expect_err(rpc);
        assert_eq!(
            status.code(),
            tonic::Code::InvalidArgument,
            "{rpc}: {status:?}"
        );
    }
    assert_eq!(
        task_count(&router).await,
        1,
        "a refused send writes nothing"
    );
    serve.abort();
}
