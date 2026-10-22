//! ListTasks request handling across transports (A2A v1.0.1 §3.1.4).
//!
//! HTTP+JSON, JSON-RPC and (under `--features grpc`) gRPC share one core
//! handler; these tests pin that every transport forwards the same
//! `ListTasksRequest` fields and rejects the same invalid input:
//!
//! - `includeArtifacts` — false (default) omits the artifacts field
//!   entirely; true includes it.
//! - `pageSize` — 1..=100 (a2a.proto `ListTasksRequest.page_size`);
//!   anything else is an invalid request, not silently clamped.
//! - `statusTimestampAfter` — only tasks whose status timestamp is at or
//!   after the bound.

mod common;

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
use turul_a2a_types::{Artifact, Message, Part, Task};

/// Emits one artifact, then completes.
struct ArtifactAgent;

#[async_trait]
impl AgentExecutor for ArtifactAgent {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let artifact = Artifact::new("out", vec![Part::text("result")]);
        ctx.events.emit_artifact(artifact, false, true).await?;
        ctx.events.complete(None).await?;
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            name: "list-tasks test agent".into(),
            description: "ListTasks transport parity".into(),
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
                push_notifications: Some(false),
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
}

fn router_on(storage: InMemoryA2aStorage) -> axum::Router {
    A2aServer::builder()
        .executor(ArtifactAgent)
        .storage(storage)
        .build()
        .expect("server build")
        .into_router()
}

async fn http_call(
    router: axum::Router,
    method: Method,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("A2A-Version", "1.0");
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let req = req
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn jsonrpc(router: axum::Router, params: serde_json::Value) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "method": "ListTasks", "params": params, "id": 1,
    });
    http_call(router, Method::POST, "/jsonrpc", Some(body))
        .await
        .1
}

/// Send one message over HTTP; returns the completed task.
async fn send(router: axum::Router, message_id: &str) -> serde_json::Value {
    let (status, body) = http_call(
        router,
        Method::POST,
        "/message:send",
        Some(serde_json::json!({
            "message": {"messageId": message_id, "role": "ROLE_USER", "parts": [{"text": "go"}]}
        })),
    )
    .await;
    assert_eq!(status, 200, "send failed: {body}");
    body["task"].clone()
}

fn ids(page: &serde_json::Value) -> Vec<String> {
    page["tasks"]
        .as_array()
        .unwrap_or_else(|| panic!("no tasks array: {page}"))
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_string())
        .collect()
}

fn status_timestamp(task: &serde_json::Value) -> String {
    task["status"]["timestamp"]
        .as_str()
        .unwrap_or_else(|| panic!("status.timestamp missing: {task}"))
        .to_string()
}

fn status_time(task: &serde_json::Value) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(&status_timestamp(task))
        .unwrap()
        .with_timezone(&chrono::Utc)
}

/// Send once the clock, at the microsecond precision storage stamps
/// status writes with, has moved past `prev`'s status time, so the new
/// task's status time is strictly later. Two back-to-back sends can
/// otherwise share a timestamp.
async fn send_after(
    router: axum::Router,
    message_id: &str,
    prev: &serde_json::Value,
) -> serde_json::Value {
    use chrono::SubsecRound;
    let prev_at = status_time(prev);
    while chrono::Utc::now().trunc_subsecs(6) <= prev_at {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    send(router, message_id).await
}

#[tokio::test]
async fn include_artifacts_http_and_jsonrpc() {
    let router = router_on(InMemoryA2aStorage::new());
    send(router.clone(), "m-ia").await;

    let (status, page) = http_call(router.clone(), Method::GET, "/tasks", None).await;
    assert_eq!(status, 200);
    assert!(
        page["tasks"][0].get("artifacts").is_none(),
        "default listing omits the artifacts field entirely: {page}"
    );
    let (_, page) = http_call(
        router.clone(),
        Method::GET,
        "/tasks?includeArtifacts=true",
        None,
    )
    .await;
    assert_eq!(page["tasks"][0]["artifacts"][0]["artifactId"], "out");

    let result = &jsonrpc(router.clone(), serde_json::json!({})).await["result"];
    assert!(result["tasks"][0].get("artifacts").is_none(), "{result}");
    let result = &jsonrpc(router, serde_json::json!({"includeArtifacts": true})).await["result"];
    assert_eq!(result["tasks"][0]["artifacts"][0]["artifactId"], "out");
}

#[tokio::test]
async fn page_size_out_of_range_is_rejected_http_and_jsonrpc() {
    let router = router_on(InMemoryA2aStorage::new());
    send(router.clone(), "m-ps").await;

    for ok in [1, 100] {
        let (status, page) = http_call(
            router.clone(),
            Method::GET,
            &format!("/tasks?pageSize={ok}"),
            None,
        )
        .await;
        assert_eq!(status, 200, "pageSize={ok}: {page}");
        assert_eq!(page["pageSize"], ok);
    }
    for bad in [0, -1, 101] {
        let (status, body) = http_call(
            router.clone(),
            Method::GET,
            &format!("/tasks?pageSize={bad}"),
            None,
        )
        .await;
        assert_eq!(status, 400, "pageSize={bad}: {body}");

        let resp = jsonrpc(router.clone(), serde_json::json!({"pageSize": bad})).await;
        assert_eq!(resp["error"]["code"], -32602, "pageSize={bad}: {resp}");
    }
}

#[tokio::test]
async fn status_timestamp_after_http_and_jsonrpc() {
    let router = router_on(InMemoryA2aStorage::new());
    let first = send(router.clone(), "m-st-1").await;
    let second = send_after(router.clone(), "m-st-2", &first).await;
    let bound = status_timestamp(&second);
    assert!(status_time(&first) < status_time(&second));

    let (status, page) = http_call(
        router.clone(),
        Method::GET,
        &format!("/tasks?statusTimestampAfter={bound}"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(ids(&page), [second["id"].as_str().unwrap()]);
    assert_eq!(page["totalSize"], 1);

    let result = &jsonrpc(
        router.clone(),
        serde_json::json!({"statusTimestampAfter": bound}),
    )
    .await["result"];
    assert_eq!(ids(result), [second["id"].as_str().unwrap()]);

    // Unfiltered: latest status first.
    let (_, page) = http_call(router.clone(), Method::GET, "/tasks", None).await;
    assert_eq!(
        ids(&page),
        [
            second["id"].as_str().unwrap(),
            first["id"].as_str().unwrap()
        ]
    );

    let (status, body) = http_call(
        router,
        Method::GET,
        "/tasks?statusTimestampAfter=yesterday",
        None,
    )
    .await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test]
async fn jsonrpc_rejects_mistyped_or_unknown_params() {
    let router = router_on(InMemoryA2aStorage::new());
    for params in [
        serde_json::json!({"statusTimestampAfter": 5}),
        serde_json::json!({"statusTimestampAfter": "yesterday"}),
        serde_json::json!({"status": "TASK_STATE_BOGUS"}),
        serde_json::json!({"pageSize": "many"}),
        serde_json::json!({"includeArtifacts": "yes"}),
    ] {
        let resp = jsonrpc(router.clone(), params.clone()).await;
        assert_eq!(resp["error"]["code"], -32602, "{params}: {resp}");
    }
}

#[cfg(feature = "grpc")]
mod grpc {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use turul_a2a_proto::grpc::A2aServiceClient;

    /// An HTTP router and a gRPC client over one shared storage.
    async fn dual() -> (
        axum::Router,
        A2aServiceClient<common::VersionedChannel>,
        tokio::task::JoinHandle<()>,
    ) {
        let storage = InMemoryA2aStorage::new();
        let router = router_on(storage.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tonic_router = A2aServer::builder()
            .executor(ArtifactAgent)
            .storage(storage)
            .build()
            .expect("grpc build")
            .into_tonic_router();
        let handle = tokio::spawn(async move {
            let _ = tonic_router
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await;
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect_lazy();
        (
            router,
            A2aServiceClient::with_interceptor(channel, common::with_a2a_version as _),
            handle,
        )
    }

    fn request() -> turul_a2a_proto::ListTasksRequest {
        turul_a2a_proto::ListTasksRequest {
            tenant: String::new(),
            context_id: String::new(),
            status: 0,
            page_size: None,
            page_token: String::new(),
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
        }
    }

    #[tokio::test]
    async fn grpc_honours_include_artifacts() {
        let (router, mut client, _guard) = dual().await;
        send(router, "m-grpc-ia").await;

        let page = client
            .list_tasks(tonic::Request::new(request()))
            .await
            .expect("list")
            .into_inner();
        assert!(
            page.tasks[0].artifacts.is_empty(),
            "default omits artifacts"
        );

        let page = client
            .list_tasks(tonic::Request::new(turul_a2a_proto::ListTasksRequest {
                include_artifacts: Some(true),
                ..request()
            }))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(page.tasks[0].artifacts.len(), 1);
        assert_eq!(page.tasks[0].artifacts[0].artifact_id, "out");
    }

    #[tokio::test]
    async fn grpc_page_size_and_status_timestamp_after() {
        let (router, mut client, _guard) = dual().await;
        let first = send(router.clone(), "m-grpc-1").await;
        let second = send_after(router, "m-grpc-2", &first).await;

        for bad in [0, 101] {
            let err = client
                .list_tasks(tonic::Request::new(turul_a2a_proto::ListTasksRequest {
                    page_size: Some(bad),
                    ..request()
                }))
                .await
                .expect_err("out-of-range pageSize");
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "pageSize={bad}");
        }

        let err = client
            .list_tasks(tonic::Request::new(turul_a2a_proto::ListTasksRequest {
                status: 99,
                ..request()
            }))
            .await
            .expect_err("unknown TaskState value");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        for nanos in [-1, 1_000_000_000] {
            let err = client
                .list_tasks(tonic::Request::new(turul_a2a_proto::ListTasksRequest {
                    status_timestamp_after: Some(turul_a2a_proto::Timestamp { seconds: 0, nanos }),
                    ..request()
                }))
                .await
                .expect_err("Timestamp nanos outside 0..=999_999_999");
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "nanos={nanos}");
        }

        let page = client
            .list_tasks(tonic::Request::new(turul_a2a_proto::ListTasksRequest {
                status_timestamp_after: Some(status_time(&second).into()),
                ..request()
            }))
            .await
            .expect("list")
            .into_inner();
        let got: Vec<&str> = page.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(got, [second["id"].as_str().unwrap()]);
        assert_eq!(page.total_size, 1);
    }
}
