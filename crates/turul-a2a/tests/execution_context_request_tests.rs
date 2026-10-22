//! `SendMessageConfiguration.accepted_output_modes`,
//! `SendMessageRequest.metadata` and the negotiated extensions reach the
//! executor.
//!
//! a2a.proto `SendMessageConfiguration.accepted_output_modes`: "A list of
//! media types the client is prepared to accept for response parts.
//! Agents SHOULD use this to tailor their output." — so the agent must be
//! able to see it. `SendMessageRequest.metadata` is "a flexible key-value
//! map for passing additional context or parameters" to the agent.
//! Extensions the client activates (A2A v1.0.1 §3.3.4 / the
//! `A2A-Extensions` service parameter) that the agent advertises are
//! reported as `ExecutionContext::activated_extensions`, so an executor
//! applies extension behaviour only when the client asked for it.
//!
//! Covered paths: blocking send (HTTP + JSON-RPC), streaming send, and
//! the durable-queue envelope (the consumer side is covered in the
//! Lambda crate's SQS tests).

mod common;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::durable_executor::{DurableExecutorQueue, QueueError, QueuedExecutorJob};
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::router::AppState;
use turul_a2a::server::A2aServer;
use turul_a2a::server::RuntimeConfig;
use turul_a2a::server::in_flight::InFlightRegistry;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a::streaming::TaskEventBroker;
use turul_a2a_types::{Message, Task};

/// `(accepted_output_modes, request_metadata, activated_extensions)` per
/// execution.
type Inputs = (Vec<String>, Option<serde_json::Value>, Vec<String>);
type Observed = Arc<Mutex<Vec<Inputs>>>;

/// Extensions the observer advertises (neither required).
const EXT_A: &str = "https://example.test/ext/a";
const EXT_B: &str = "https://example.test/ext/b";
/// Activation header value for requests "with inputs": both advertised
/// extensions, out of order, plus one the agent does not advertise.
const ACTIVATION: &str =
    "https://example.test/ext/b, https://unknown.test/x, https://example.test/ext/a";

struct ObservingExecutor {
    observed: Observed,
}

#[async_trait]
impl AgentExecutor for ObservingExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        _message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        self.observed.lock().unwrap().push((
            ctx.accepted_output_modes.clone(),
            ctx.request_metadata.clone(),
            ctx.activated_extensions.clone(),
        ));
        ctx.events.complete(None).await?;
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a::card_builder::AgentCardBuilder::new("observer", "1.0.0")
            .description("records request-level executor inputs")
            .url("http://localhost", "JSONRPC", "1.0")
            .streaming(true)
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain", "application/json"])
            .extension(advertised(EXT_A))
            .extension(advertised(EXT_B))
            .build()
            .unwrap()
    }
}

fn advertised(uri: &str) -> turul_a2a_proto::AgentExtension {
    turul_a2a_proto::AgentExtension {
        uri: uri.into(),
        ..Default::default()
    }
}

fn router() -> (axum::Router, Observed) {
    let observed = Observed::default();
    let router = A2aServer::builder()
        .executor(ObservingExecutor {
            observed: observed.clone(),
        })
        .storage(InMemoryA2aStorage::new())
        .build()
        .unwrap()
        .into_router();
    (router, observed)
}

fn send_request(with_inputs: bool, return_immediately: bool) -> serde_json::Value {
    let mut req = serde_json::json!({
        "message": {
            "messageId": uuid::Uuid::now_v7().to_string(),
            "role": "ROLE_USER",
            "parts": [{"text": "hi"}],
        },
        "configuration": { "returnImmediately": return_immediately },
    });
    if with_inputs {
        req["configuration"]["acceptedOutputModes"] =
            serde_json::json!(["application/json", "text/plain"]);
        req["metadata"] = serde_json::json!({"traceId": "trace-77", "flags": {"debug": true}});
    }
    req
}

/// Negotiated set: the advertised extensions the client activated,
/// sorted; the unadvertised URI is dropped.
fn expected_extensions() -> Vec<String> {
    vec![EXT_A.to_string(), EXT_B.to_string()]
}

fn expected_inputs() -> Inputs {
    (
        vec!["application/json".to_string(), "text/plain".to_string()],
        Some(serde_json::json!({"traceId": "trace-77", "flags": {"debug": true}})),
        expected_extensions(),
    )
}

/// A request "with inputs" also activates [`ACTIVATION`].
fn post(uri: &str, body: String) -> Request<Body> {
    Request::post(uri)
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .header("a2a-extensions", ACTIVATION)
        .body(Body::from(body))
        .unwrap()
}

fn post_without_activation(uri: &str, body: String) -> Request<Body> {
    Request::post(uri)
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn blocking_send_delivers_accepted_output_modes_and_request_metadata() {
    let (router, observed) = router();
    let resp = router
        .oneshot(post("/message:send", send_request(true, false).to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(*observed.lock().unwrap(), vec![expected_inputs()]);
}

#[tokio::test]
async fn blocking_send_without_inputs_yields_empty_modes_and_no_metadata() {
    let (router, observed) = router();
    let resp = router
        .oneshot(post_without_activation(
            "/message:send",
            send_request(false, false).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(*observed.lock().unwrap(), vec![(vec![], None, vec![])]);
}

#[tokio::test]
async fn jsonrpc_send_delivers_accepted_output_modes_and_request_metadata() {
    let (router, observed) = router();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "SendMessage",
        "params": send_request(true, false),
    });
    let resp = router
        .oneshot(post("/jsonrpc", body.to_string()))
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(json.get("result").is_some(), "got {json}");
    assert_eq!(*observed.lock().unwrap(), vec![expected_inputs()]);
}

#[tokio::test]
async fn streaming_send_delivers_accepted_output_modes_and_request_metadata() {
    let (router, observed) = router();
    let resp = router
        .oneshot(post(
            "/message:stream",
            send_request(true, false).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    // The stream closes after the terminal event, so draining it means
    // the executor has run.
    tokio::time::timeout(Duration::from_secs(10), resp.into_body().collect())
        .await
        .expect("stream closes after terminal")
        .unwrap();
    assert_eq!(*observed.lock().unwrap(), vec![expected_inputs()]);
}

#[cfg(feature = "grpc")]
mod grpc {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use turul_a2a_proto::grpc::A2aServiceClient;

    #[tokio::test]
    async fn grpc_send_delivers_accepted_output_modes_and_request_metadata() {
        let observed = Observed::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = A2aServer::builder()
            .executor(ObservingExecutor {
                observed: observed.clone(),
            })
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
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect_lazy();
        let request: turul_a2a_proto::SendMessageRequest =
            serde_json::from_value(send_request(true, false)).unwrap();
        let mut client: A2aServiceClient<common::VersionedChannel> =
            A2aServiceClient::with_interceptor(channel, common::with_a2a_version as _);
        let mut send = tonic::Request::new(request.clone());
        send.metadata_mut()
            .insert("a2a-extensions", ACTIVATION.parse().unwrap());
        client.send_message(send).await.expect("grpc send");
        assert_eq!(*observed.lock().unwrap(), vec![expected_inputs()]);

        let mut stream = tonic::Request::new(request);
        stream
            .metadata_mut()
            .insert("a2a-extensions", ACTIVATION.parse().unwrap());
        let mut frames = client
            .send_streaming_message(stream)
            .await
            .expect("grpc stream")
            .into_inner();
        while tokio::time::timeout(Duration::from_secs(10), frames.message())
            .await
            .expect("stream closes after terminal")
            .expect("stream frame")
            .is_some()
        {}
        assert_eq!(
            *observed.lock().unwrap(),
            vec![expected_inputs(), expected_inputs()]
        );
    }
}

// ---------------------------------------------------------------------------
// Durable-queue envelope
// ---------------------------------------------------------------------------

struct CapturingQueue {
    jobs: Arc<tokio::sync::Mutex<Vec<QueuedExecutorJob>>>,
}

#[async_trait]
impl DurableExecutorQueue for CapturingQueue {
    fn max_payload_bytes(&self) -> usize {
        256 * 1024
    }

    async fn enqueue(&self, job: QueuedExecutorJob) -> Result<(), QueueError> {
        self.jobs.lock().await.push(job);
        Ok(())
    }

    fn kind(&self) -> &'static str {
        "capturing"
    }
}

#[tokio::test]
async fn durable_queue_envelope_carries_accepted_output_modes_and_request_metadata() {
    let jobs = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let observed = Observed::default();
    let storage = Arc::new(InMemoryA2aStorage::new());
    let mut runtime_config = RuntimeConfig::default();
    runtime_config.supports_return_immediately = true;
    let state = AppState {
        executor: Arc::new(ObservingExecutor {
            observed: observed.clone(),
        }),
        task_storage: storage.clone(),
        push_storage: storage.clone(),
        event_store: storage.clone(),
        atomic_store: storage.clone(),
        event_broker: TaskEventBroker::new(),
        middleware_stack: Arc::new(turul_a2a::middleware::MiddlewareStack::new(vec![])),
        runtime_config,
        in_flight: Arc::new(InFlightRegistry::new()),
        cancellation_supervisor: storage,
        push_delivery_store: None,
        push_dispatcher: None,
        durable_executor_queue: Some(Arc::new(CapturingQueue { jobs: jobs.clone() })),
    };

    let negotiated: HashSet<String> = expected_extensions().into_iter().collect();
    let response = turul_a2a::router::core_send_message(
        state,
        "",
        "anonymous",
        None,
        &negotiated,
        send_request(true, true).to_string(),
    )
    .await
    .expect("enqueue");
    assert_eq!(
        response.0.pointer("/task/status/state"),
        Some(&serde_json::json!("TASK_STATE_WORKING")),
        "durable path returns the working task"
    );

    let jobs = jobs.lock().await;
    assert_eq!(jobs.len(), 1);
    let (modes, metadata, extensions) = expected_inputs();
    assert_eq!(jobs[0].accepted_output_modes, modes);
    assert_eq!(jobs[0].request_metadata, metadata);
    assert_eq!(jobs[0].activated_extensions, extensions);
    assert!(
        observed.lock().unwrap().is_empty(),
        "the executor runs on the consumer side, not at enqueue"
    );

    // The envelope round-trips the new fields through its wire encoding.
    let decoded: QueuedExecutorJob =
        serde_json::from_str(&serde_json::to_string(&jobs[0]).unwrap()).unwrap();
    assert_eq!(decoded.accepted_output_modes, modes);
    assert_eq!(decoded.request_metadata, metadata);
    assert_eq!(decoded.activated_extensions, extensions);
}
