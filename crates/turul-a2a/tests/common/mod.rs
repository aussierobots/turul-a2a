//! Helpers shared by the integration tests: reading an SSE body while
//! telling "the server closed the stream" apart from "the wait elapsed
//! with the stream still open", listing the states of a stream's status
//! updates, waiting for a spawned server to answer before the first
//! request, and (with `grpc`) a tonic channel that sends the
//! `a2a-version` metadata a conforming client sends.

// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::time::Duration;

use axum::body::Body;
use http_body_util::BodyExt;
use serde_json::Value;

/// What an SSE body delivered within the wait.
#[derive(Debug)]
pub struct SseRead {
    /// One JSON value per complete event, in order.
    pub events: Vec<Value>,
    /// `true` when the server ended the stream; `false` when the wait
    /// elapsed with the stream still open.
    pub closed: bool,
}

impl SseRead {
    /// The events of a stream that must have ended within the wait.
    pub fn closed(self) -> Vec<Value> {
        assert!(
            self.closed,
            "stream was still open when the wait elapsed; got {:?}",
            self.events
        );
        self.events
    }

    /// The events of a stream that must still be open when the wait
    /// elapses.
    pub fn still_open(self) -> Vec<Value> {
        assert!(
            !self.closed,
            "stream closed, expected it to stay open; got {:?}",
            self.events
        );
        self.events
    }
}

/// Read `body` until the server ends it or `wait` elapses.
pub async fn read_sse(mut body: Body, wait: Duration) -> SseRead {
    let deadline = tokio::time::Instant::now() + wait;
    let mut buf = Vec::new();
    let closed = loop {
        match tokio::time::timeout_at(deadline, body.frame()).await {
            Err(_) => break false,
            Ok(None) => break true,
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    buf.extend_from_slice(data);
                }
            }
            Ok(Some(Err(e))) => panic!("SSE body failed: {e}"),
        }
    };
    let text = std::str::from_utf8(&buf).expect("SSE body is UTF-8");
    SseRead {
        events: parse_sse(text),
        closed,
    }
}

/// The `data` of every complete event in an SSE body, parsed as JSON.
///
/// Events end at a blank line; an event's `data:` lines are joined with
/// `\n` (W3C Server-Sent Events, "Interpreting an event stream").
/// Comment-only events (keep-alives) carry no data and are skipped. A
/// trailing incomplete event is ignored. Panics on a non-JSON payload:
/// every A2A stream event is a JSON `StreamResponse`.
pub fn parse_sse(text: &str) -> Vec<Value> {
    let complete = match text.rfind("\n\n") {
        Some(end) => &text[..end],
        None => return Vec::new(),
    };
    complete
        .split("\n\n")
        .filter_map(|event| {
            let data: Vec<&str> = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|value| value.strip_prefix(' ').unwrap_or(value))
                .collect();
            if data.is_empty() {
                return None;
            }
            let data = data.join("\n");
            Some(
                serde_json::from_str(&data)
                    .unwrap_or_else(|e| panic!("SSE data is not JSON ({e}): {data}")),
            )
        })
        .collect()
}

/// The state of every `statusUpdate` event, in order.
pub fn status_states(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| e["statusUpdate"]["status"]["state"].as_str())
        .collect()
}

/// Poll an HTTP server's agent card until it answers 200.
pub async fn wait_http_ready(base_url: &str) {
    let url = format!("{base_url}/.well-known/agent-card.json");
    let client = reqwest::Client::new();
    for _ in 0..250 {
        if let Ok(resp) = client.get(&url).send().await
            && resp.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("HTTP server at {base_url} never became ready");
}

/// Poll a gRPC server until it answers an RPC (any status other than
/// UNAVAILABLE counts: an unknown task, or an auth refusal, is still an
/// answer).
#[cfg(feature = "grpc")]
pub async fn wait_grpc_ready(addr: std::net::SocketAddr) {
    use turul_a2a_proto::grpc::A2aServiceClient;
    let endpoint = format!("http://{addr}");
    for _ in 0..250 {
        if let Ok(mut client) = A2aServiceClient::connect(endpoint.clone()).await {
            let probe = client
                .get_task(tonic::Request::new(turul_a2a_proto::GetTaskRequest {
                    tenant: String::new(),
                    id: "readiness-probe".into(),
                    history_length: None,
                }))
                .await;
            match probe {
                Err(status) if status.code() == tonic::Code::Unavailable => {}
                _ => return,
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("gRPC server at {addr} never became ready");
}

/// Raw tonic client channel that stamps every call with the `a2a-version`
/// metadata a conforming client sends (spec Section 3.6.1).
#[cfg(feature = "grpc")]
pub type VersionedChannel = tonic::service::interceptor::InterceptedService<
    tonic::transport::Channel,
    fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
>;

/// Interceptor that adds `a2a-version: <supported version>` to a call.
#[cfg(feature = "grpc")]
pub fn with_a2a_version(mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
    use turul_a2a_types::wire::protocol;
    req.metadata_mut().insert(
        tonic::metadata::MetadataKey::from_bytes(protocol::VERSION_PARAM.as_bytes())
            .expect("valid metadata key"),
        tonic::metadata::MetadataValue::from_static(protocol::VERSION),
    );
    Ok(req)
}
