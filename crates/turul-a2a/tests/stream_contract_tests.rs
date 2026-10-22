//! Cross-transport stream and send-target contract (A2A v1.0.1).
//!
//! Every test runs the same assertions over HTTP+JSON, JSON-RPC and,
//! with the `grpc` feature, gRPC — all served by one `A2aServer`, so the
//! transports share storage, the event broker and the in-flight executor
//! registry:
//!
//! - both streaming RPCs open with a `Task` snapshot (spec §3.1.2,
//!   §3.1.6), including a client reconnecting with `Last-Event-ID`,
//!   which is ignored (§3.5.2: reconnecting opens a new stream);
//! - no event already reflected in the snapshot is replayed after it;
//! - blocking and streaming send honour `message.taskId` identically:
//!   unknown id → TaskNotFound (§3.4.2), terminal task →
//!   UnsupportedOperation, mismatched contextId → rejected, missing
//!   contextId → inferred (§3.4.3), SUBMITTED / WORKING task →
//!   InvalidRequest, and a follow-up while the previous turn's executor
//!   is still running → UnsupportedOperation, with nothing written;
//! - subscribing to a terminal task is UnsupportedOperation (§3.1.6),
//!   to an unknown one TaskNotFound;
//! - streams close on terminal states and INPUT_REQUIRED and stay open
//!   across AUTH_REQUIRED;
//! - artifact chunks keep `append` / `lastChunk` as stored.

mod common;
#[cfg(feature = "grpc")]
use common::{VersionedChannel, with_a2a_version};

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use http::{Method, Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::server::A2aServer;
use turul_a2a::storage::{A2aEventStore, A2aTaskStorage, InMemoryA2aStorage};
use turul_a2a::streaming::StreamEvent;
use turul_a2a_types::{Artifact, Message, Part, Task, TaskState, TaskStatus};

/// How long a stream that is expected to close may take to do so.
const STREAM_WAIT: Duration = Duration::from_secs(10);

/// Scripted by the message's text:
/// - `input` → INPUT_REQUIRED; a follow-up `more` completes the task;
/// - `auth` → AUTH_REQUIRED, then (credential arriving out of band)
///   WORKING, then COMPLETED, all on the same stream;
/// - `auth-hold` → like `auth`, but holds AUTH_REQUIRED until the test
///   releases it (`Harness::release`);
/// - `auth-pause` → like `auth`, but holds AUTH_REQUIRED for `PAUSE`,
///   for a test that must subscribe while the executor runs: when a
///   subscriber has attached is not observable from outside, so the
///   release cannot be tied to it;
/// - `hold` → stays WORKING until the test releases it, then completes;
/// - `chunks` → one artifact in three chunks, then COMPLETED;
/// - anything else → COMPLETED.
struct ScriptAgent {
    /// One permit releases one held executor.
    gate: Arc<Semaphore>,
}

const PAUSE: Duration = Duration::from_millis(1500);

impl ScriptAgent {
    async fn wait_for_release(&self) {
        self.gate
            .acquire()
            .await
            .expect("gate is never closed")
            .forget();
    }
}

#[async_trait]
impl AgentExecutor for ScriptAgent {
    async fn execute(
        &self,
        task: &mut Task,
        msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        match msg.joined_text().as_str() {
            "input" => {
                task.set_status(TaskStatus::new(TaskState::InputRequired));
            }
            text @ ("auth" | "auth-hold" | "auth-pause") => {
                ctx.events.require_auth(None).await?;
                match text {
                    "auth" => tokio::time::sleep(Duration::from_millis(100)).await,
                    "auth-pause" => tokio::time::sleep(PAUSE).await,
                    _ => self.wait_for_release().await,
                }
                ctx.events.set_status(TaskState::Working, None).await?;
                ctx.events.complete(None).await?;
            }
            "hold" => {
                self.wait_for_release().await;
                ctx.events.complete(None).await?;
            }
            "chunks" => {
                for (text, append, last_chunk) in [
                    ("one", false, false),
                    (" two", true, false),
                    (" three", true, true),
                ] {
                    let chunk = Artifact::new("chunked", vec![Part::text(text)]);
                    ctx.events.emit_artifact(chunk, append, last_chunk).await?;
                }
                ctx.events.complete(None).await?;
            }
            _ => {
                ctx.events.complete(None).await?;
            }
        }
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            name: "stream contract".into(),
            version: "1.0.0".into(),
            capabilities: Some(turul_a2a_proto::AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Transport {
    Http,
    JsonRpc,
    #[cfg(feature = "grpc")]
    Grpc,
}

fn transports() -> Vec<Transport> {
    vec![
        Transport::Http,
        Transport::JsonRpc,
        #[cfg(feature = "grpc")]
        Transport::Grpc,
    ]
}

/// An error returned instead of a response or stream, as the transport
/// put it on the wire.
#[derive(Debug)]
enum Failure {
    /// HTTP status and the body's `error` object.
    Http { status: u16, error: Value },
    /// The JSON-RPC `error` object.
    JsonRpc { error: Value },
    /// The gRPC status code and its `ErrorInfo` `(reason, domain)`.
    #[cfg(feature = "grpc")]
    Grpc {
        code: tonic::Code,
        error_info: Option<(String, String)>,
    },
}

#[derive(Debug, Clone, Copy)]
enum Expected {
    TaskNotFound,
    UnsupportedOperation,
    /// A validation error (spec §3.3.2): HTTP 400, JSON-RPC -32602, gRPC
    /// INVALID_ARGUMENT, with no A2A `ErrorInfo`.
    InvalidRequest,
}

impl Expected {
    fn reason(self) -> Option<&'static str> {
        match self {
            Expected::TaskNotFound => Some("TASK_NOT_FOUND"),
            Expected::UnsupportedOperation => Some("UNSUPPORTED_OPERATION"),
            Expected::InvalidRequest => None,
        }
    }
}

/// Assert `failure` is `expected` as spec §5.4 maps it onto `t`.
fn assert_failure(t: Transport, failure: &Failure, expected: Expected, what: &str) {
    let ctx = format!("{t:?} {what}: expected {expected:?}, got {failure:?}");
    match failure {
        Failure::Http { status, error } => {
            let want = match expected {
                Expected::TaskNotFound => 404,
                Expected::UnsupportedOperation | Expected::InvalidRequest => 400,
            };
            assert_eq!(*status, want, "{ctx}");
            assert_error_info(&error["details"], expected.reason(), &ctx);
        }
        Failure::JsonRpc { error } => {
            let want = match expected {
                Expected::TaskNotFound => -32001,
                Expected::UnsupportedOperation => -32004,
                Expected::InvalidRequest => -32602,
            };
            assert_eq!(error["code"], want, "{ctx}");
            assert_error_info(&error["data"], expected.reason(), &ctx);
        }
        #[cfg(feature = "grpc")]
        Failure::Grpc { code, error_info } => {
            let want = match expected {
                Expected::TaskNotFound => tonic::Code::NotFound,
                Expected::UnsupportedOperation => tonic::Code::FailedPrecondition,
                Expected::InvalidRequest => tonic::Code::InvalidArgument,
            };
            assert_eq!(*code, want, "{ctx}");
            match expected.reason() {
                Some(reason) => assert_eq!(
                    error_info.as_ref().map(|(r, d)| (r.as_str(), d.as_str())),
                    Some((reason, "a2a-protocol.org")),
                    "{ctx}"
                ),
                None => assert!(error_info.is_none(), "{ctx}: no ErrorInfo expected"),
            }
        }
    }
}

fn assert_error_info(details: &Value, reason: Option<&str>, ctx: &str) {
    match reason {
        Some(reason) => {
            assert_eq!(details[0]["reason"], reason, "{ctx}");
            assert_eq!(details[0]["domain"], "a2a-protocol.org", "{ctx}");
        }
        None => assert!(details.is_null(), "{ctx}: no ErrorInfo expected"),
    }
}

struct Harness {
    router: axum::Router,
    storage: InMemoryA2aStorage,
    gate: Arc<Semaphore>,
    #[cfg(feature = "grpc")]
    grpc_addr: std::net::SocketAddr,
    #[cfg(feature = "grpc")]
    grpc: tokio::task::JoinHandle<()>,
}

#[cfg(feature = "grpc")]
impl Drop for Harness {
    fn drop(&mut self) {
        self.grpc.abort();
    }
}

impl Harness {
    /// Let one `hold` / `auth-hold` executor finish.
    fn release(&self) {
        self.gate.add_permits(1);
    }
}

/// One server, every transport.
async fn harness() -> Harness {
    let storage = InMemoryA2aStorage::new();
    let gate = Arc::new(Semaphore::new(0));
    let server = A2aServer::builder()
        .executor(ScriptAgent { gate: gate.clone() })
        .storage(storage.clone())
        .bind(([127, 0, 0, 1], 0))
        .build()
        .expect("server build");
    #[cfg(feature = "grpc")]
    {
        let (router, tonic_router) = server.into_routers();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc_addr = listener.local_addr().unwrap();
        let grpc = tokio::spawn(async move {
            let _ = tonic_router
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await;
        });
        common::wait_grpc_ready(grpc_addr).await;
        Harness {
            router,
            storage,
            gate,
            grpc_addr,
            grpc,
        }
    }
    #[cfg(not(feature = "grpc"))]
    {
        Harness {
            router: server.into_router(),
            storage,
            gate,
        }
    }
}

fn message(id: &str, text: &str, task_id: Option<&str>, context_id: Option<&str>) -> Value {
    let mut m = json!({
        "messageId": id,
        "role": "ROLE_USER",
        "parts": [{"text": text}],
    });
    if let Some(t) = task_id {
        m["taskId"] = json!(t);
    }
    if let Some(c) = context_id {
        m["contextId"] = json!(c);
    }
    m
}

fn is_event_stream(resp: &http::Response<Body>) -> bool {
    resp.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"))
}

async fn json_body(resp: http::Response<Body>) -> Value {
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).expect("JSON body")
}

async fn call(router: &axum::Router, req: Request<Body>) -> http::Response<Body> {
    tokio::time::timeout(STREAM_WAIT, router.clone().oneshot(req))
        .await
        .expect("response head within the wait")
        .unwrap()
}

fn http_request(method: Method, uri: &str, body: Option<Value>) -> Request<Body> {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("A2A-Version", "1.0")
        .header("content-type", "application/json");
    match body {
        Some(b) => req.body(Body::from(b.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    }
}

fn jsonrpc_request(method: &str, params: Value, resume: Option<&str>) -> Request<Body> {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/jsonrpc")
        .header("A2A-Version", "1.0")
        .header("content-type", "application/json");
    if let Some(id) = resume {
        req = req.header("Last-Event-ID", id);
    }
    req.body(Body::from(
        json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}).to_string(),
    ))
    .unwrap()
}

/// The stream's items as `StreamResponse` JSON, after the stream closed.
async fn http_stream(router: &axum::Router, req: Request<Body>) -> Result<Vec<Value>, Failure> {
    let resp = call(router, req).await;
    let status = resp.status().as_u16();
    if !is_event_stream(&resp) {
        let body = json_body(resp).await;
        return Err(Failure::Http {
            status,
            error: body["error"].clone(),
        });
    }
    Ok(common::read_sse(resp.into_body(), STREAM_WAIT)
        .await
        .closed())
}

async fn jsonrpc_stream(router: &axum::Router, req: Request<Body>) -> Result<Vec<Value>, Failure> {
    let resp = call(router, req).await;
    if !is_event_stream(&resp) {
        let body = json_body(resp).await;
        return Err(Failure::JsonRpc {
            error: body["error"].clone(),
        });
    }
    let frames = common::read_sse(resp.into_body(), STREAM_WAIT)
        .await
        .closed();
    Ok(frames
        .into_iter()
        .map(|envelope| {
            assert_eq!(envelope["id"], 7, "every frame echoes the request id");
            envelope["result"].clone()
        })
        .collect())
}

#[cfg(feature = "grpc")]
fn grpc_failure(status: &tonic::Status) -> Failure {
    use tonic_types::StatusExt;
    Failure::Grpc {
        code: status.code(),
        error_info: status
            .get_details_error_info()
            .map(|i| (i.reason, i.domain)),
    }
}

#[cfg(feature = "grpc")]
async fn grpc_client(h: &Harness) -> turul_a2a_proto::grpc::A2aServiceClient<VersionedChannel> {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{}", h.grpc_addr))
        .expect("grpc endpoint")
        .connect()
        .await
        .expect("grpc connect");
    turul_a2a_proto::grpc::A2aServiceClient::with_interceptor(channel, with_a2a_version as _)
}

/// The stream's items after it closed. A status arriving after the
/// stream opened is a test failure, not an outcome.
#[cfg(feature = "grpc")]
async fn grpc_stream(
    resp: Result<tonic::Response<tonic::Streaming<turul_a2a_proto::StreamResponse>>, tonic::Status>,
) -> Result<Vec<Value>, Failure> {
    let mut stream = resp.map_err(|s| grpc_failure(&s))?.into_inner();
    let mut items = Vec::new();
    loop {
        match tokio::time::timeout(STREAM_WAIT, stream.message())
            .await
            .expect("grpc stream closes within the wait")
        {
            Ok(Some(item)) => items.push(serde_json::to_value(&item).unwrap()),
            Ok(None) => return Ok(items),
            Err(s) => panic!("grpc stream failed mid-stream after {items:?}: {s:?}"),
        }
    }
}

async fn stream_send(h: &Harness, t: Transport, msg: Value) -> Result<Vec<Value>, Failure> {
    match t {
        Transport::Http => {
            let req = http_request(
                Method::POST,
                "/message:stream",
                Some(json!({"message": msg})),
            );
            http_stream(&h.router, req).await
        }
        Transport::JsonRpc => {
            let req = jsonrpc_request("SendStreamingMessage", json!({"message": msg}), None);
            jsonrpc_stream(&h.router, req).await
        }
        #[cfg(feature = "grpc")]
        Transport::Grpc => {
            let req: turul_a2a_proto::SendMessageRequest =
                serde_json::from_value(json!({"message": msg})).unwrap();
            let mut client = grpc_client(h).await;
            grpc_stream(
                client
                    .send_streaming_message(tonic::Request::new(req))
                    .await,
            )
            .await
        }
    }
}

/// Blocking `SendMessage`; the resulting task JSON.
async fn send(h: &Harness, t: Transport, msg: Value) -> Result<Value, Failure> {
    match t {
        Transport::Http => {
            let req = http_request(Method::POST, "/message:send", Some(json!({"message": msg})));
            let resp = call(&h.router, req).await;
            let status = resp.status().as_u16();
            let body = json_body(resp).await;
            if status != 200 {
                return Err(Failure::Http {
                    status,
                    error: body["error"].clone(),
                });
            }
            Ok(body["task"].clone())
        }
        Transport::JsonRpc => {
            let req = jsonrpc_request("SendMessage", json!({"message": msg}), None);
            let body = json_body(call(&h.router, req).await).await;
            if body.get("error").is_some() {
                return Err(Failure::JsonRpc {
                    error: body["error"].clone(),
                });
            }
            Ok(body["result"]["task"].clone())
        }
        #[cfg(feature = "grpc")]
        Transport::Grpc => {
            let req: turul_a2a_proto::SendMessageRequest =
                serde_json::from_value(json!({"message": msg})).unwrap();
            let mut client = grpc_client(h).await;
            let resp = client
                .send_message(tonic::Request::new(req))
                .await
                .map_err(|s| grpc_failure(&s))?;
            Ok(serde_json::to_value(resp.into_inner()).unwrap()["task"].clone())
        }
    }
}

/// `SubscribeToTask`; `resume` is a `Last-Event-ID` (gRPC:
/// `a2a-last-event-id` metadata).
async fn subscribe(
    h: &Harness,
    t: Transport,
    task_id: &str,
    resume: Option<&str>,
) -> Result<Vec<Value>, Failure> {
    match t {
        Transport::Http => {
            let mut req = http_request(Method::GET, &format!("/tasks/{task_id}:subscribe"), None);
            if let Some(id) = resume {
                req.headers_mut()
                    .insert("Last-Event-ID", id.parse().unwrap());
            }
            http_stream(&h.router, req).await
        }
        Transport::JsonRpc => {
            let req = jsonrpc_request("SubscribeToTask", json!({"id": task_id}), resume);
            jsonrpc_stream(&h.router, req).await
        }
        #[cfg(feature = "grpc")]
        Transport::Grpc => {
            let mut client = grpc_client(h).await;
            let mut req = tonic::Request::new(turul_a2a_proto::SubscribeToTaskRequest {
                tenant: String::new(),
                id: task_id.to_string(),
            });
            if let Some(id) = resume {
                req.metadata_mut()
                    .insert("a2a-last-event-id", id.parse().unwrap());
            }
            grpc_stream(client.subscribe_to_task(req).await).await
        }
    }
}

async fn get_task(h: &Harness, task_id: &str) -> Value {
    let req = http_request(Method::GET, &format!("/tasks/{task_id}"), None);
    json_body(call(&h.router, req).await).await
}

/// The snapshot that must open every stream.
fn snapshot(t: Transport, items: &[Value]) -> &Value {
    let first = items
        .first()
        .unwrap_or_else(|| panic!("{t:?}: empty stream"));
    assert!(
        first.get("task").is_some(),
        "{t:?}: first item must be a Task, got {first}"
    );
    assert_eq!(
        items.iter().filter(|i| i.get("task").is_some()).count(),
        1,
        "{t:?}: exactly one Task snapshot, got {items:?}"
    );
    &first["task"]
}

/// Poll `GetTask` until the task reaches `state`.
async fn wait_for_state(h: &Harness, task_id: &str, state: &str) {
    for _ in 0..100 {
        if get_task(h, task_id).await["status"]["state"] == state {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {task_id} never reached {state}");
}

/// Start `text` on a background HTTP stream. Returns the task id as
/// soon as the snapshot arrives, and the handle yielding every item
/// once the stream closes.
async fn start_background(
    h: &Harness,
    text: &str,
) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
    let router = h.router.clone();
    let req = http_request(
        Method::POST,
        "/message:stream",
        Some(json!({"message": message("m1", text, None, None)})),
    );
    let (id_tx, id_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        let resp = call(&router, req).await;
        assert!(is_event_stream(&resp), "background stream did not open");
        let mut body = resp.into_body();
        let mut buf = String::new();
        let mut id_tx = Some(id_tx);
        let deadline = tokio::time::Instant::now() + STREAM_WAIT;
        while let Some(frame) = tokio::time::timeout_at(deadline, body.frame())
            .await
            .expect("background stream closes within the wait")
        {
            if let Some(data) = frame.unwrap().data_ref() {
                buf.push_str(std::str::from_utf8(data).unwrap());
            }
            if let Some(first) = common::parse_sse(&buf).first()
                && let Some(tx) = id_tx.take()
            {
                let _ = tx.send(first["task"]["id"].as_str().unwrap().to_string());
            }
        }
        common::parse_sse(&buf)
    });
    let task_id = tokio::time::timeout(STREAM_WAIT, id_rx)
        .await
        .expect("snapshot within the wait")
        .expect("background stream sent the snapshot");
    (task_id, handle)
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_send_opens_with_working_task_and_closes_on_completed() {
    let h = harness().await;
    for t in transports() {
        let items = stream_send(&h, t, message("m1", "hello", None, None))
            .await
            .unwrap();
        // The turn has started before the snapshot is taken.
        assert_eq!(
            snapshot(t, &items)["status"]["state"],
            "TASK_STATE_WORKING",
            "{t:?}"
        );
        assert_eq!(
            common::status_states(&items),
            ["TASK_STATE_COMPLETED"],
            "{t:?}: SUBMITTED and WORKING are in the snapshot and must not be replayed"
        );
    }
}

#[tokio::test]
async fn stream_closes_on_input_required_and_continuation_resumes_same_task() {
    let h = harness().await;
    for t in transports() {
        let first = stream_send(&h, t, message("m1", "input", None, None))
            .await
            .unwrap();
        assert_eq!(
            common::status_states(&first),
            ["TASK_STATE_INPUT_REQUIRED"],
            "{t:?}: stream closes after INPUT_REQUIRED"
        );
        let opened = snapshot(t, &first);
        let task_id = opened["id"].as_str().unwrap().to_string();
        let context_id = opened["contextId"].as_str().unwrap().to_string();

        // Continuation names only the taskId: the contextId is inferred.
        let second = stream_send(&h, t, message("m2", "more", Some(&task_id), None))
            .await
            .unwrap();
        let resumed = snapshot(t, &second);
        assert_eq!(resumed["id"], task_id.as_str(), "{t:?}: same task");
        assert_eq!(
            resumed["contextId"],
            context_id.as_str(),
            "{t:?}: contextId inferred from the task"
        );
        assert_eq!(resumed["status"]["state"], "TASK_STATE_WORKING", "{t:?}");
        assert_eq!(
            common::status_states(&second),
            ["TASK_STATE_COMPLETED"],
            "{t:?}: neither the first turn nor this turn's WORKING is replayed"
        );
    }
}

#[tokio::test]
async fn stream_stays_open_across_auth_required() {
    let h = harness().await;
    for t in transports() {
        let items = stream_send(&h, t, message("m1", "auth", None, None))
            .await
            .unwrap();
        assert_eq!(
            common::status_states(&items),
            [
                "TASK_STATE_AUTH_REQUIRED",
                "TASK_STATE_WORKING",
                "TASK_STATE_COMPLETED"
            ],
            "{t:?}: AUTH_REQUIRED → WORKING → COMPLETED on one stream"
        );
    }
}

#[tokio::test]
async fn send_to_an_unknown_task_is_task_not_found() {
    let h = harness().await;
    for t in transports() {
        let msg = message("m1", "more", Some("no-such-task"), None);
        let blocking = send(&h, t, msg.clone()).await.unwrap_err();
        assert_failure(t, &blocking, Expected::TaskNotFound, "blocking send");
        let streaming = stream_send(&h, t, msg).await.unwrap_err();
        assert_failure(t, &streaming, Expected::TaskNotFound, "streaming send");
    }
}

#[tokio::test]
async fn send_to_a_terminal_task_is_unsupported_operation() {
    let h = harness().await;
    for t in transports() {
        let done = send(&h, t, message("m1", "hello", None, None))
            .await
            .unwrap();
        assert_eq!(done["status"]["state"], "TASK_STATE_COMPLETED", "{t:?}");
        let task_id = done["id"].as_str().unwrap();

        let msg = message("m2", "more", Some(task_id), None);
        let blocking = send(&h, t, msg.clone()).await.unwrap_err();
        assert_failure(
            t,
            &blocking,
            Expected::UnsupportedOperation,
            "blocking send",
        );
        let streaming = stream_send(&h, t, msg).await.unwrap_err();
        assert_failure(
            t,
            &streaming,
            Expected::UnsupportedOperation,
            "streaming send",
        );
    }
}

#[tokio::test]
async fn send_with_a_mismatched_context_is_invalid_request() {
    let h = harness().await;
    for t in transports() {
        let paused = send(&h, t, message("m1", "input", None, None))
            .await
            .unwrap();
        assert_eq!(
            paused["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
            "{t:?}"
        );
        let task_id = paused["id"].as_str().unwrap();

        let msg = message("m2", "more", Some(task_id), Some("some-other-context"));
        let blocking = send(&h, t, msg.clone()).await.unwrap_err();
        assert_failure(t, &blocking, Expected::InvalidRequest, "blocking send");
        let streaming = stream_send(&h, t, msg).await.unwrap_err();
        assert_failure(t, &streaming, Expected::InvalidRequest, "streaming send");

        let task = get_task(&h, task_id).await;
        assert_eq!(
            task["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
            "{t:?}: a rejected message changes nothing"
        );
    }
}

#[tokio::test]
async fn blocking_continuation_infers_the_context_id() {
    let h = harness().await;
    for t in transports() {
        let paused = send(&h, t, message("m1", "input", None, None))
            .await
            .unwrap();
        let task_id = paused["id"].as_str().unwrap();
        let context_id = paused["contextId"].as_str().unwrap();

        let done = send(&h, t, message("m2", "more", Some(task_id), None))
            .await
            .unwrap();
        assert_eq!(done["id"], task_id, "{t:?}: same task");
        assert_eq!(done["contextId"], context_id, "{t:?}: contextId inferred");
        assert_eq!(done["status"]["state"], "TASK_STATE_COMPLETED", "{t:?}");
    }
}

#[tokio::test]
async fn send_to_a_submitted_or_working_task_is_invalid_request() {
    let h = harness().await;
    for t in transports() {
        let submitted = format!("submitted-{t:?}");
        h.storage
            .create_task(
                "",
                "anonymous",
                Task::new(&submitted, TaskStatus::new(TaskState::Submitted))
                    .with_context_id("ctx-submitted"),
            )
            .await
            .unwrap();
        let (working, run) = start_background(&h, "hold").await;
        assert_eq!(
            get_task(&h, &working).await["status"]["state"],
            "TASK_STATE_WORKING"
        );

        for task_id in [&submitted, &working] {
            let msg = message("m2", "more", Some(task_id), None);
            let blocking = send(&h, t, msg.clone()).await.unwrap_err();
            assert_failure(t, &blocking, Expected::InvalidRequest, "blocking send");
            let streaming = stream_send(&h, t, msg).await.unwrap_err();
            assert_failure(t, &streaming, Expected::InvalidRequest, "streaming send");
        }
        h.release();
        assert_eq!(
            common::status_states(&run.await.unwrap()),
            ["TASK_STATE_COMPLETED"],
            "{t:?}: the running turn is undisturbed"
        );
    }
}

#[tokio::test]
async fn subscribe_to_an_input_required_task_yields_the_snapshot_and_closes() {
    let h = harness().await;
    for t in transports() {
        let paused = send(&h, t, message("m1", "input", None, None))
            .await
            .unwrap();
        let task_id = paused["id"].as_str().unwrap();

        // The task is INPUT_REQUIRED: the snapshot says so, and the
        // stream closes straight after it.
        let items = subscribe(&h, t, task_id, None).await.unwrap();
        assert_eq!(items.len(), 1, "{t:?}: only the snapshot, got {items:?}");
        assert_eq!(
            snapshot(t, &items)["status"]["state"],
            "TASK_STATE_INPUT_REQUIRED",
            "{t:?}"
        );
    }
}

#[tokio::test]
async fn subscribe_to_a_terminal_or_unknown_task_is_refused() {
    let h = harness().await;
    for t in transports() {
        let done = send(&h, t, message("m1", "hello", None, None))
            .await
            .unwrap();
        let task_id = done["id"].as_str().unwrap();
        let terminal = subscribe(&h, t, task_id, None).await.unwrap_err();
        assert_failure(
            t,
            &terminal,
            Expected::UnsupportedOperation,
            "subscribe to a terminal task",
        );
        let unknown = subscribe(&h, t, "no-such-task", None).await.unwrap_err();
        assert_failure(
            t,
            &unknown,
            Expected::TaskNotFound,
            "subscribe to an unknown task",
        );
    }
}

#[tokio::test]
async fn subscribe_to_a_live_task_delivers_only_later_events() {
    let h = harness().await;
    for t in transports() {
        let (task_id, sender) = start_background(&h, "auth-pause").await;
        wait_for_state(&h, &task_id, "TASK_STATE_AUTH_REQUIRED").await;

        let items = subscribe(&h, t, &task_id, None).await.unwrap();
        assert_eq!(
            snapshot(t, &items)["status"]["state"],
            "TASK_STATE_AUTH_REQUIRED",
            "{t:?}"
        );
        assert_eq!(
            common::status_states(&items),
            ["TASK_STATE_WORKING", "TASK_STATE_COMPLETED"],
            "{t:?}: only events after the snapshot"
        );
        sender.await.unwrap();
    }
}

#[tokio::test]
async fn follow_up_while_the_executor_is_still_running_is_refused_without_side_effects() {
    let h = harness().await;
    for t in transports() {
        let (task_id, sender) = start_background(&h, "auth-hold").await;
        wait_for_state(&h, &task_id, "TASK_STATE_AUTH_REQUIRED").await;

        // The task is AUTH_REQUIRED but its executor has not returned,
        // so a second executor cannot be started for it.
        let blocking = send(&h, t, message("m2", "more", Some(&task_id), None))
            .await
            .unwrap_err();
        assert_failure(
            t,
            &blocking,
            Expected::UnsupportedOperation,
            "blocking send",
        );
        let streaming = stream_send(&h, t, message("m3", "more", Some(&task_id), None))
            .await
            .unwrap_err();
        assert_failure(
            t,
            &streaming,
            Expected::UnsupportedOperation,
            "streaming send",
        );

        h.release();
        // The refusals wrote nothing: the original turn runs to the end
        // undisturbed and neither message is in the history.
        assert_eq!(
            common::status_states(&sender.await.unwrap()),
            [
                "TASK_STATE_AUTH_REQUIRED",
                "TASK_STATE_WORKING",
                "TASK_STATE_COMPLETED"
            ],
            "{t:?}"
        );
        let task = get_task(&h, &task_id).await;
        let ids: Vec<&str> = task["history"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["messageId"].as_str())
            .collect();
        assert_eq!(ids, ["m1"], "{t:?}: refused messages must not be appended");
    }
}

#[tokio::test]
async fn reconnecting_with_last_event_id_still_opens_with_the_snapshot() {
    let h = harness().await;
    for t in transports() {
        let paused = send(&h, t, message("m1", "input", None, None))
            .await
            .unwrap();
        let task_id = paused["id"].as_str().unwrap();

        let items = subscribe(&h, t, task_id, Some(&format!("{task_id}:1")))
            .await
            .unwrap();
        assert_eq!(items.len(), 1, "{t:?}: the snapshot only, got {items:?}");
        assert_eq!(
            snapshot(t, &items)["status"]["state"],
            "TASK_STATE_INPUT_REQUIRED",
            "{t:?}"
        );
    }
}

/// `(append, lastChunk)` of every artifact update, absent flags read as
/// false (proto3 JSON omits default values).
fn chunk_flags(items: &[Value]) -> Vec<(bool, bool)> {
    items
        .iter()
        .filter_map(|i| i.get("artifactUpdate"))
        .map(|u| {
            (
                u["append"].as_bool().unwrap_or(false),
                u["lastChunk"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

#[tokio::test]
async fn artifact_chunks_keep_append_and_last_chunk_on_every_transport() {
    let h = harness().await;
    for t in transports() {
        let items = stream_send(&h, t, message("m1", "chunks", None, None))
            .await
            .unwrap();
        let task_id = snapshot(t, &items)["id"].as_str().unwrap().to_string();

        let stored: Vec<(bool, bool)> = h
            .storage
            .get_events_after("", &task_id, 0)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|(_, event)| match event {
                StreamEvent::ArtifactUpdate { artifact_update } => {
                    Some((artifact_update.append, artifact_update.last_chunk))
                }
                StreamEvent::StatusUpdate { .. } => None,
            })
            .collect();
        assert_eq!(
            stored,
            [(false, false), (true, false), (true, true)],
            "{t:?}: the store keeps what the executor emitted"
        );
        assert_eq!(
            chunk_flags(&items),
            stored,
            "{t:?}: the wire shows what is stored"
        );
        assert_eq!(
            common::status_states(&items),
            ["TASK_STATE_COMPLETED"],
            "{t:?}"
        );
    }
}

/// Strip `//` and `/* */` comments, keeping string and char literals,
/// so the routing guard below only matches code.
fn strip_comments(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            '"' => {
                out.push('"');
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        out.push(chars[i]);
                        i += 1;
                    }
                    if let Some(&c) = chars.get(i) {
                        out.push(c);
                    }
                    i += 1;
                }
                out.push('"');
                i += 1;
            }
            // A char literal such as '"' or '\''; a lifetime falls through.
            '\'' if chars.get(i + 2) == Some(&'\'') || chars.get(i + 1) == Some(&'\\') => {
                out.push('\'');
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    if chars[i] == '\\' {
                        out.push(chars[i]);
                        i += 1;
                    }
                    if let Some(&c) = chars.get(i) {
                        out.push(c);
                    }
                    i += 1;
                }
                out.push('\'');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

#[test]
fn strip_comments_keeps_code_and_literals() {
    let src =
        "let a = \"http://x\"; // open_send_stream\n/* .get_events_after( */ let b = '\"'; f()";
    let code = strip_comments(src);
    assert!(code.contains("\"http://x\""));
    assert!(code.contains("let b = '\"'; f()"));
    assert!(!code.contains("open_send_stream"));
    assert!(!code.contains("get_events_after"));
}

/// One stream loop and one poll interval: the transports only encode
/// items from the shared core stream, so they cannot drift apart.
#[test]
fn transports_route_through_the_core_stream() {
    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let code = |path: &std::path::Path| strip_comments(&std::fs::read_to_string(path).unwrap());
    let mut files = Vec::new();
    rust_files(&src, &mut files);

    let core = src.join("streaming/task_stream.rs");
    assert!(code(&core).contains("STORE_POLL_INTERVAL"));
    let storage = src.join("storage");
    for file in &files {
        if *file == core {
            continue;
        }
        let text = code(file);
        assert!(
            !text.contains("STORE_POLL_INTERVAL"),
            "{} defines a poll interval outside the core stream",
            file.display()
        );
        if !file.starts_with(&storage) {
            assert!(
                !text.contains(".get_events_after("),
                "{} reads the event store directly instead of using the core stream",
                file.display()
            );
        }
    }

    for name in ["jsonrpc.rs", "grpc/streaming.rs"] {
        let text = code(&src.join(name));
        assert!(
            text.contains("open_send_stream") && text.contains("open_subscribe_stream"),
            "{name} must open both streaming RPCs through the shared core"
        );
    }
}
