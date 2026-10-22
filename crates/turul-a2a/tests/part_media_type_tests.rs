//! Message-part media types against `AgentCard.defaultInputModes`
//! (A2A v1.0.1 §3.1.1: `ContentTypeNotSupportedError`, "a Media Type
//! provided in the request's message parts is not supported by the
//! agent"). The check runs before any task is created, on blocking and
//! streaming sends, over HTTP+JSON, JSON-RPC and gRPC.

mod common;

use async_trait::async_trait;
use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::server::A2aServer;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Message, Task};

struct ImageAgent;

#[async_trait]
impl AgentExecutor for ImageAgent {
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
            name: "image agent".into(),
            version: "1.0.0".into(),
            default_input_modes: vec!["text/plain".into(), "image/*".into()],
            capabilities: Some(turul_a2a_proto::AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

fn router() -> axum::Router {
    A2aServer::builder()
        .executor(ImageAgent)
        .storage(InMemoryA2aStorage::new())
        .build()
        .expect("server")
        .into_router()
}

fn message(media_type: Option<&str>) -> Value {
    let mut part = json!({"text": "payload"});
    if let Some(m) = media_type {
        part["mediaType"] = json!(m);
    }
    json!({"messageId": "m1", "role": "ROLE_USER", "parts": [part]})
}

async fn post(router: axum::Router, path: &str, body: Value) -> (u16, Vec<u8>) {
    let req = Request::post(path)
        .header("a2a-version", "1.0")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    (
        status,
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
}

async fn task_count(router: axum::Router) -> usize {
    let req = Request::get("/tasks")
        .header("a2a-version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let body: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    body["tasks"].as_array().map_or(0, Vec::len)
}

#[tokio::test]
async fn unsupported_part_media_type_is_refused_before_any_task_exists() {
    for path in ["/message:send", "/message:stream"] {
        let router = router();
        let (status, body) = post(
            router.clone(),
            path,
            json!({"message": message(Some("application/pdf"))}),
        )
        .await;
        assert_eq!(status, 400, "{path}");
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body["error"]["details"][0]["reason"], "CONTENT_TYPE_NOT_SUPPORTED",
            "{path}: {body}"
        );
        assert_eq!(task_count(router).await, 0, "{path}: nothing persisted");
    }

    let router = router();
    let (_, body) = post(
        router.clone(),
        "/jsonrpc",
        json!({"jsonrpc": "2.0", "id": 1, "method": "SendMessage",
               "params": {"message": message(Some("application/pdf"))}}),
    )
    .await;
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], -32005, "{body}");
    assert_eq!(task_count(router).await, 0, "JSON-RPC: nothing persisted");
}

#[tokio::test]
async fn accepted_wildcard_and_untyped_parts_pass() {
    for media_type in [Some("image/png"), Some("text/plain; charset=utf-8"), None] {
        let (status, body) = post(
            router(),
            "/message:send",
            json!({"message": message(media_type)}),
        )
        .await;
        assert_eq!(
            status,
            200,
            "{media_type:?}: {}",
            String::from_utf8_lossy(&body)
        );
    }
}

#[cfg(feature = "grpc")]
#[tokio::test]
async fn grpc_send_refuses_unsupported_part_media_type() {
    use tonic_types::StatusExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let grpc = A2aServer::builder()
        .executor(ImageAgent)
        .storage(InMemoryA2aStorage::new())
        .bind(([127, 0, 0, 1], 0))
        .build()
        .unwrap()
        .into_tonic_router();
    tokio::spawn(async move {
        let _ = grpc
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await;
    });
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect_lazy();
    let mut client: turul_a2a_proto::grpc::A2aServiceClient<common::VersionedChannel> =
        turul_a2a_proto::grpc::A2aServiceClient::with_interceptor(
            channel,
            common::with_a2a_version as _,
        );
    let request: turul_a2a_proto::SendMessageRequest =
        serde_json::from_value(json!({"message": message(Some("application/pdf"))})).unwrap();
    let status = client
        .send_message(tonic::Request::new(request))
        .await
        .expect_err("unsupported media type");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        status.get_details_error_info().expect("ErrorInfo").reason,
        "CONTENT_TYPE_NOT_SUPPORTED"
    );
}
