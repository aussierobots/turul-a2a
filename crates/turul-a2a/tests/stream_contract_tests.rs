//! Cross-transport stream contract (A2A v1.0.1).
//!
//! Every test runs the same assertions over HTTP+SSE, JSON-RPC+SSE and,
//! with the `grpc` feature, gRPC server streaming:
//!
//! - both streaming RPCs open with a `Task` snapshot (§3.1.2, §3.1.6);
//! - no event already reflected in the snapshot is replayed after it;
//! - streaming send honours `message.taskId`: unknown id →
//!   TaskNotFound, terminal task → UnsupportedOperation, mismatched
//!   contextId → rejected, missing contextId → inferred (§3.4.3);
//! - streams close on terminal states and INPUT_REQUIRED (§11.7) and
//!   stay open across AUTH_REQUIRED (§7.6.1).

use std::time::Duration;

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
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};

/// Scripted by the first message's text:
/// - `input` → INPUT_REQUIRED; a follow-up `more` completes the task;
/// - `auth` → AUTH_REQUIRED, then (credential arriving out of band)
///   WORKING, then COMPLETED, all on the same stream;
/// - `auth-hold` → like `auth`, but holds AUTH_REQUIRED for long
///   enough that a test can act while the executor is still running;
/// - anything else → COMPLETED.
struct ScriptAgent;

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
            "auth" | "auth-hold" => {
                ctx.events.require_auth(None).await?;
                let hold = if msg.joined_text() == "auth" {
                    100
                } else {
                    1500
                };
                tokio::time::sleep(Duration::from_millis(hold)).await;
                ctx.events.set_status(TaskState::Working, None).await?;
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

/// Either the stream's items, normalised to `StreamResponse` JSON
/// (`{"task": …}` / `{"statusUpdate": …}` / `{"artifactUpdate": …}`),
/// or the error reason. Errors without an `ErrorInfo` (InvalidRequest)
/// normalise to `INVALID_ARGUMENT`.
#[derive(Debug)]
enum Outcome {
    Items(Vec<Value>),
    Error(String),
}

impl Outcome {
    fn items(self) -> Vec<Value> {
        match self {
            Outcome::Items(items) => items,
            Outcome::Error(reason) => panic!("expected a stream, got error {reason}"),
        }
    }
    fn error(self) -> String {
        match self {
            Outcome::Error(reason) => reason,
            Outcome::Items(items) => panic!("expected an error, got stream {items:?}"),
        }
    }
}

struct Harness {
    router: axum::Router,
    #[cfg(feature = "grpc")]
    grpc_addr: std::net::SocketAddr,
    #[cfg(feature = "grpc")]
    _grpc: tokio::task::JoinHandle<()>,
}

async fn harness() -> Harness {
    let storage = InMemoryA2aStorage::new();
    let router = A2aServer::builder()
        .executor(ScriptAgent)
        .storage(storage.clone())
        .build()
        .expect("http build")
        .into_router();
    #[cfg(feature = "grpc")]
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc_addr = listener.local_addr().unwrap();
        let tonic_router = A2aServer::builder()
            .executor(ScriptAgent)
            .storage(storage)
            .bind(([127, 0, 0, 1], 0))
            .build()
            .expect("grpc build")
            .into_tonic_router();
        let grpc = tokio::spawn(async move {
            let _ = tonic_router
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        Harness {
            router,
            grpc_addr,
            _grpc: grpc,
        }
    }
    #[cfg(not(feature = "grpc"))]
    {
        let _ = storage;
        Harness { router }
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

/// SSE frames as `(id, data)` pairs.
fn sse_frames(body: &[u8]) -> Vec<(Option<String>, Value)> {
    let text = std::str::from_utf8(body).expect("utf8 SSE body");
    text.split("\n\n")
        .filter_map(|frame| {
            let mut id = None;
            let mut data = None;
            for line in frame.lines() {
                if let Some(v) = line.strip_prefix("id:") {
                    id = Some(v.trim().to_string());
                } else if let Some(v) = line.strip_prefix("data:") {
                    data = Some(serde_json::from_str(v.trim()).expect("SSE data is JSON"));
                }
            }
            data.map(|d| (id, d))
        })
        .collect()
}

fn reason_or_invalid(error: &Value, details_key: &str) -> String {
    error[details_key][0]["reason"]
        .as_str()
        .unwrap_or("INVALID_ARGUMENT")
        .to_string()
}

async fn sse_request(router: &axum::Router, req: Request<Body>) -> (u16, Vec<u8>) {
    let resp = tokio::time::timeout(Duration::from_secs(10), router.clone().oneshot(req))
        .await
        .expect("response within 10s")
        .unwrap();
    let status = resp.status().as_u16();
    let body = tokio::time::timeout(Duration::from_secs(10), resp.into_body().collect())
        .await
        .expect("stream closes within 10s")
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, body)
}

/// Raw HTTP+SSE frames for `SendStreamingMessage`, used where a test
/// needs the SSE `id:` lines.
async fn http_stream_frames(h: &Harness, msg: Value) -> Vec<(Option<String>, Value)> {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/message:stream")
        .header("A2A-Version", "1.0")
        .header("content-type", "application/json")
        .body(Body::from(json!({"message": msg}).to_string()))
        .unwrap();
    let (status, body) = sse_request(&h.router, req).await;
    assert_eq!(status, 200);
    sse_frames(&body)
}

async fn stream_send(h: &Harness, t: Transport, msg: Value) -> Outcome {
    match t {
        Transport::Http => {
            let req = Request::builder()
                .method(Method::POST)
                .uri("/message:stream")
                .header("A2A-Version", "1.0")
                .header("content-type", "application/json")
                .body(Body::from(json!({"message": msg}).to_string()))
                .unwrap();
            let (status, body) = sse_request(&h.router, req).await;
            if status != 200 {
                let v: Value = serde_json::from_slice(&body).unwrap();
                return Outcome::Error(reason_or_invalid(&v["error"], "details"));
            }
            Outcome::Items(sse_frames(&body).into_iter().map(|(_, d)| d).collect())
        }
        Transport::JsonRpc => {
            jsonrpc_stream(h, "SendStreamingMessage", json!({"message": msg}), None).await
        }
        #[cfg(feature = "grpc")]
        Transport::Grpc => {
            let req: turul_a2a_proto::SendMessageRequest =
                serde_json::from_value(json!({"message": msg})).unwrap();
            let mut client = grpc_client(h).await;
            grpc_collect(
                client
                    .send_streaming_message(tonic::Request::new(req))
                    .await,
            )
            .await
        }
    }
}

/// `SubscribeToTask`; `resume` is a `Last-Event-ID` (gRPC:
/// `a2a-last-event-id` metadata).
async fn subscribe(h: &Harness, t: Transport, task_id: &str, resume: Option<&str>) -> Outcome {
    match t {
        Transport::Http => {
            let mut req = Request::builder()
                .method(Method::GET)
                .uri(format!("/tasks/{task_id}:subscribe"))
                .header("A2A-Version", "1.0");
            if let Some(id) = resume {
                req = req.header("Last-Event-ID", id);
            }
            let req = req.body(Body::empty()).unwrap();
            let (status, body) = sse_request(&h.router, req).await;
            if status != 200 {
                let v: Value = serde_json::from_slice(&body).unwrap();
                return Outcome::Error(reason_or_invalid(&v["error"], "details"));
            }
            Outcome::Items(sse_frames(&body).into_iter().map(|(_, d)| d).collect())
        }
        Transport::JsonRpc => {
            jsonrpc_stream(h, "SubscribeToTask", json!({"id": task_id}), resume).await
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
            grpc_collect(client.subscribe_to_task(req).await).await
        }
    }
}

async fn jsonrpc_stream(h: &Harness, method: &str, params: Value, resume: Option<&str>) -> Outcome {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/jsonrpc")
        .header("A2A-Version", "1.0")
        .header("content-type", "application/json");
    if let Some(id) = resume {
        req = req.header("Last-Event-ID", id);
    }
    let req = req
        .body(Body::from(
            json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}).to_string(),
        ))
        .unwrap();
    let (_, body) = sse_request(&h.router, req).await;
    // Errors raised before the stream opens come back as a plain
    // JSON-RPC error object rather than an SSE body.
    if let Ok(v) = serde_json::from_slice::<Value>(&body) {
        return Outcome::Error(reason_or_invalid(&v["error"], "data"));
    }
    let mut items = Vec::new();
    for (_, envelope) in sse_frames(&body) {
        assert_eq!(envelope["id"], 7, "every frame echoes the request id");
        items.push(envelope["result"].clone());
    }
    Outcome::Items(items)
}

#[cfg(feature = "grpc")]
async fn grpc_client(
    h: &Harness,
) -> turul_a2a_proto::grpc::A2aServiceClient<tonic::transport::Channel> {
    turul_a2a_proto::grpc::A2aServiceClient::connect(format!("http://{}", h.grpc_addr))
        .await
        .expect("grpc connect")
}

#[cfg(feature = "grpc")]
async fn grpc_collect(
    resp: Result<tonic::Response<tonic::Streaming<turul_a2a_proto::StreamResponse>>, tonic::Status>,
) -> Outcome {
    use tonic_types::StatusExt;
    let reason = |s: &tonic::Status| {
        s.get_details_error_info()
            .map(|i| i.reason)
            .unwrap_or_else(|| "INVALID_ARGUMENT".into())
    };
    let mut stream = match resp {
        Ok(r) => r.into_inner(),
        Err(s) => return Outcome::Error(reason(&s)),
    };
    let mut items = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), stream.message())
            .await
            .expect("grpc stream closes within 10s")
        {
            Ok(Some(item)) => items.push(serde_json::to_value(&item).unwrap()),
            Ok(None) => break,
            Err(s) => return Outcome::Error(reason(&s)),
        }
    }
    Outcome::Items(items)
}

/// Blocking `SendMessage` over HTTP; returns the resulting task JSON.
async fn send_blocking(h: &Harness, msg: Value) -> (u16, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/message:send")
        .header("A2A-Version", "1.0")
        .header("content-type", "application/json")
        .body(Body::from(json!({"message": msg}).to_string()))
        .unwrap();
    let resp = h.router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

fn state_of(item: &Value) -> Option<&str> {
    item["task"]["status"]["state"]
        .as_str()
        .or_else(|| item["statusUpdate"]["status"]["state"].as_str())
}

fn status_states(items: &[Value]) -> Vec<&str> {
    items
        .iter()
        .filter(|i| i.get("statusUpdate").is_some())
        .filter_map(state_of)
        .collect()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_send_opens_with_task_and_closes_on_completed() {
    for t in transports() {
        let h = harness().await;
        let items = stream_send(&h, t, message("m1", "hello", None, None))
            .await
            .items();
        assert!(
            items[0].get("task").is_some(),
            "{t:?}: first item must be a Task, got {}",
            items[0]
        );
        assert_eq!(
            items.iter().filter(|i| i.get("task").is_some()).count(),
            1,
            "{t:?}: exactly one Task snapshot"
        );
        assert_eq!(
            status_states(&items).last().copied(),
            Some("TASK_STATE_COMPLETED"),
            "{t:?}: stream ends with COMPLETED"
        );
        assert!(
            !status_states(&items).contains(&"TASK_STATE_SUBMITTED"),
            "{t:?}: SUBMITTED is already reflected in the snapshot and must not be replayed"
        );
    }
}

#[tokio::test]
async fn stream_closes_on_input_required_and_continuation_resumes_same_task() {
    for t in transports() {
        let h = harness().await;
        let first = stream_send(&h, t, message("m1", "input", None, None))
            .await
            .items();
        assert_eq!(
            status_states(&first).last().copied(),
            Some("TASK_STATE_INPUT_REQUIRED"),
            "{t:?}: stream closes after INPUT_REQUIRED"
        );
        let task_id = first[0]["task"]["id"].as_str().unwrap().to_string();
        let context_id = first[0]["task"]["contextId"].as_str().unwrap().to_string();

        // Continuation names only the taskId: the contextId is inferred.
        let second = stream_send(&h, t, message("m2", "more", Some(&task_id), None))
            .await
            .items();
        assert_eq!(
            second[0]["task"]["id"],
            task_id.as_str(),
            "{t:?}: same task"
        );
        assert_eq!(
            second[0]["task"]["contextId"],
            context_id.as_str(),
            "{t:?}: contextId inferred from the task"
        );
        let states = status_states(&second);
        assert!(
            !states.contains(&"TASK_STATE_SUBMITTED")
                && !states.contains(&"TASK_STATE_INPUT_REQUIRED"),
            "{t:?}: the first turn's history must not be replayed, got {states:?}"
        );
        assert_eq!(
            states.last().copied(),
            Some("TASK_STATE_COMPLETED"),
            "{t:?}"
        );
    }
}

#[tokio::test]
async fn stream_stays_open_across_auth_required() {
    for t in transports() {
        let h = harness().await;
        let items = stream_send(&h, t, message("m1", "auth", None, None))
            .await
            .items();
        let states = status_states(&items);
        let auth = states.iter().position(|s| *s == "TASK_STATE_AUTH_REQUIRED");
        let tail = auth.map(|a| &states[a..]);
        assert_eq!(
            tail,
            Some(
                &[
                    "TASK_STATE_AUTH_REQUIRED",
                    "TASK_STATE_WORKING",
                    "TASK_STATE_COMPLETED"
                ][..]
            ),
            "{t:?}: AUTH_REQUIRED → WORKING → COMPLETED on one stream, got {states:?}"
        );
    }
}

#[tokio::test]
async fn streaming_send_to_unknown_task_is_task_not_found() {
    for t in transports() {
        let h = harness().await;
        let reason = stream_send(&h, t, message("m1", "more", Some("no-such-task"), None))
            .await
            .error();
        assert_eq!(reason, "TASK_NOT_FOUND", "{t:?}");
    }
}

#[tokio::test]
async fn sending_to_a_terminal_task_is_unsupported_operation() {
    let h = harness().await;
    let (status, done) = send_blocking(&h, message("m1", "hello", None, None)).await;
    assert_eq!(status, 200);
    let task_id = done["task"]["id"].as_str().unwrap().to_string();

    for t in transports() {
        let reason = stream_send(&h, t, message("m2", "more", Some(&task_id), None))
            .await
            .error();
        assert_eq!(reason, "UNSUPPORTED_OPERATION", "{t:?}: streaming send");
    }

    let (status, body) = send_blocking(&h, message("m3", "more", Some(&task_id), None)).await;
    assert_eq!(status, 400, "blocking send");
    assert_eq!(
        body["error"]["details"][0]["reason"], "UNSUPPORTED_OPERATION",
        "blocking send to a terminal task"
    );
}

#[tokio::test]
async fn streaming_send_with_mismatched_context_is_rejected() {
    for t in transports() {
        let h = harness().await;
        let first = stream_send(&h, t, message("m1", "input", None, None))
            .await
            .items();
        let task_id = first[0]["task"]["id"].as_str().unwrap().to_string();
        let reason = stream_send(
            &h,
            t,
            message("m2", "more", Some(&task_id), Some("some-other-context")),
        )
        .await
        .error();
        assert_eq!(reason, "INVALID_ARGUMENT", "{t:?}");
    }
}

#[tokio::test]
async fn subscribe_to_an_input_required_task_yields_the_snapshot_and_closes() {
    for t in transports() {
        let h = harness().await;
        let first = stream_send(&h, t, message("m1", "input", None, None))
            .await
            .items();
        let task_id = first[0]["task"]["id"].as_str().unwrap().to_string();

        // The task is INPUT_REQUIRED: the snapshot says so, and the
        // stream closes straight after it.
        let items = subscribe(&h, t, &task_id, None).await.items();
        assert_eq!(items.len(), 1, "{t:?}: only the snapshot, got {items:?}");
        assert_eq!(
            state_of(&items[0]),
            Some("TASK_STATE_INPUT_REQUIRED"),
            "{t:?}"
        );
    }
}

#[tokio::test]
async fn http_snapshot_event_id_resumes_after_the_snapshot() {
    let h = harness().await;
    let frames = http_stream_frames(&h, message("m1", "hello", None, None)).await;
    let (snapshot_id, snapshot) = &frames[0];
    assert!(snapshot.get("task").is_some());
    let task_id = snapshot["task"]["id"].as_str().unwrap();
    let snapshot_seq: u64 = snapshot_id
        .as_deref()
        .and_then(|id| id.strip_prefix(&format!("{task_id}:")))
        .and_then(|s| s.parse().ok())
        .expect("snapshot id is {task_id}:{sequence}");
    assert!(
        snapshot_seq > 0,
        "the snapshot id carries the sequence it reflects, so Last-Event-ID resumes after it"
    );
    for (id, _) in &frames[1..] {
        let seq: u64 = id
            .as_deref()
            .unwrap()
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            seq > snapshot_seq,
            "events after the snapshot have higher sequences"
        );
    }
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
    let mut files = Vec::new();
    rust_files(&src, &mut files);

    let core = src.join("streaming/task_stream.rs");
    let storage = src.join("storage");
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        if *file != core {
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
    }

    for name in ["jsonrpc.rs", "grpc/streaming.rs"] {
        let text = std::fs::read_to_string(src.join(name)).unwrap();
        assert!(
            text.contains("open_send_stream") && text.contains("open_subscribe_stream"),
            "{name} must open both streaming RPCs through the shared core"
        );
    }
}

/// Poll `GetTask` until the task reaches `state`.
async fn wait_for_state(h: &Harness, task_id: &str, state: &str) {
    for _ in 0..100 {
        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("/tasks/{task_id}"))
            .header("A2A-Version", "1.0")
            .body(Body::empty())
            .unwrap();
        let resp = h.router.clone().oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let task: Value = serde_json::from_slice(&body).unwrap();
        if task["status"]["state"] == state {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {task_id} never reached {state}");
}

/// Start an `auth-hold` task on a background stream; returns its id
/// once it is AUTH_REQUIRED, plus the stream's handle.
async fn start_held_task(h: &Harness) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
    let frames = http_stream_frames_background(h, message("m1", "auth-hold", None, None));
    let task_id = loop {
        if let Some(id) = latest_task_id(h).await {
            break id;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    wait_for_state(h, &task_id, "TASK_STATE_AUTH_REQUIRED").await;
    (task_id, frames)
}

fn http_stream_frames_background(h: &Harness, msg: Value) -> tokio::task::JoinHandle<Vec<Value>> {
    let router = h.router.clone();
    tokio::spawn(async move {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/message:stream")
            .header("A2A-Version", "1.0")
            .header("content-type", "application/json")
            .body(Body::from(json!({"message": msg}).to_string()))
            .unwrap();
        let (_, body) = sse_request(&router, req).await;
        sse_frames(&body).into_iter().map(|(_, d)| d).collect()
    })
}

async fn latest_task_id(h: &Harness) -> Option<String> {
    let req = Request::builder()
        .method(Method::GET)
        .uri("/tasks")
        .header("A2A-Version", "1.0")
        .body(Body::empty())
        .unwrap();
    let resp = h.router.clone().oneshot(req).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let page: Value = serde_json::from_slice(&body).unwrap();
    page["tasks"][0]["id"].as_str().map(str::to_string)
}

#[tokio::test]
async fn subscribe_to_a_live_task_delivers_only_later_events() {
    for t in transports() {
        let h = harness().await;
        let (task_id, sender) = start_held_task(&h).await;

        let items = subscribe(&h, t, &task_id, None).await.items();
        assert_eq!(
            state_of(&items[0]),
            Some("TASK_STATE_AUTH_REQUIRED"),
            "{t:?}"
        );
        assert_eq!(
            status_states(&items),
            ["TASK_STATE_WORKING", "TASK_STATE_COMPLETED"],
            "{t:?}: only events after the snapshot, got {items:?}"
        );
        sender.await.unwrap();
    }
}

#[tokio::test]
async fn follow_up_while_the_executor_is_still_running_is_refused_without_side_effects() {
    let h = harness().await;
    let (task_id, sender) = start_held_task(&h).await;

    // The task is AUTH_REQUIRED but its executor has not returned, so a
    // second executor cannot be started for it.
    let (status, body) = send_blocking(&h, message("m2", "more", Some(&task_id), None)).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        body["error"]["details"][0]["reason"],
        "UNSUPPORTED_OPERATION"
    );

    // The refusal wrote nothing: the original turn still completes.
    let items = sender.await.unwrap();
    assert_eq!(
        status_states(&items).last().copied(),
        Some("TASK_STATE_COMPLETED")
    );
    let history = {
        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("/tasks/{task_id}"))
            .header("A2A-Version", "1.0")
            .body(Body::empty())
            .unwrap();
        let resp = h.router.clone().oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<Value>(&body).unwrap()["history"].clone()
    };
    assert!(
        history
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["messageId"] != "m2"),
        "the refused message must not be appended"
    );
}

#[tokio::test]
async fn resume_replays_only_later_events_and_closes_when_caught_up() {
    for t in transports() {
        let h = harness().await;
        // SUBMITTED (1), WORKING (2), INPUT_REQUIRED (3).
        let first = stream_send(&h, t, message("m1", "input", None, None))
            .await
            .items();
        let task_id = first[0]["task"]["id"].as_str().unwrap().to_string();

        let items = subscribe(&h, t, &task_id, Some(&format!("{task_id}:1")))
            .await
            .items();
        assert!(
            items.iter().all(|i| i.get("task").is_none()),
            "{t:?}: no snapshot on resume"
        );
        assert_eq!(
            status_states(&items),
            ["TASK_STATE_WORKING", "TASK_STATE_INPUT_REQUIRED"],
            "{t:?}"
        );

        // Already caught up on a task whose state closes streams: the
        // resumed stream closes at once instead of idling.
        let items = subscribe(&h, t, &task_id, Some(&format!("{task_id}:3")))
            .await
            .items();
        assert!(items.is_empty(), "{t:?}: got {items:?}");
    }
}
