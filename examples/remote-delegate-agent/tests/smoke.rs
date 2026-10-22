//! End-to-end smoke for the remote-delegate-agent.
//!
//! The upstream A2A agent is an in-process `wiremock` server serving an
//! agent card (advertising the skill-invocation profile) and a spec
//! `SendMessageResponse`. The delegate is the already-built binary
//! (`CARGO_BIN_EXE_*`) on a free loopback port, pointed at the mock.
//! Tests are hermetic and parallel-safe.
//!
//! Asserts that the delegate re-emits the upstream's artifacts in a
//! spec `SendMessageResponse`, and that it activates the profile on the
//! upstream call (`A2A-Extensions` header) only when the forwarded
//! message carries `a2a.skillId` routing metadata.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROFILE_URI: &str = "https://turul.dev/a2a/extensions/skill-invocation/v1";
const UPSTREAM_TEXT: &str = "hello from the upstream";

struct AgentGuard {
    child: Child,
    base: String,
}

impl Drop for AgentGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A port the OS reports free right now. The listener is dropped before
/// the delegate binds it.
fn free_port() -> u16 {
    std::net::TcpListener::bind("0.0.0.0:0")
        .and_then(|l| l.local_addr())
        .expect("reserve free port")
        .port()
}

async fn mock_upstream() -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "Mock Upstream",
            "description": "wiremock upstream for the delegate smoke test",
            "supportedInterfaces": [{
                "url": upstream.uri(),
                "protocolBinding": "HTTP+JSON",
                "protocolVersion": "1.0"
            }],
            "version": "1.0.0",
            "capabilities": { "extensions": [ { "uri": PROFILE_URI } ] },
            "defaultInputModes": ["text/plain"],
            "defaultOutputModes": ["text/plain"],
            "skills": []
        })))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/message:send"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task": {
                "id": "upstream-task",
                "contextId": "upstream-ctx",
                "status": { "state": "TASK_STATE_COMPLETED" },
                "artifacts": [{
                    "artifactId": "upstream-artifact",
                    "name": "greeting",
                    "parts": [ { "text": UPSTREAM_TEXT } ]
                }]
            }
        })))
        .mount(&upstream)
        .await;
    upstream
}

async fn spawn_delegate(upstream: &MockServer) -> AgentGuard {
    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_remote-delegate-agent"))
        .env("A2A_PORT", port.to_string())
        .env("REMOTE_AGENT_URL", upstream.uri())
        // Short-circuit any local .env so the delegate's auth/timeout
        // settings are deterministic across machines.
        .env("REMOTE_AGENT_BEARER", "")
        .env("REMOTE_TIMEOUT_SECS", "10")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn delegate");
    let guard = AgentGuard {
        child,
        base: format!("http://127.0.0.1:{port}"),
    };
    let url = format!("{}/.well-known/agent-card.json", guard.base);
    for _ in 0..150 {
        if let Ok(r) = reqwest::get(&url).await
            && r.status().is_success()
        {
            return guard;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("delegate did not become ready at {url} within 15s");
}

async fn send_through(delegate: &AgentGuard, message: Value) -> Value {
    let resp = reqwest::Client::new()
        .post(format!("{}/message:send", delegate.base))
        .header("a2a-version", "1.0")
        .json(&json!({ "message": message }))
        .send()
        .await
        .expect("send to delegate");
    assert_eq!(resp.status().as_u16(), 200);
    resp.json().await.expect("delegate response is JSON")
}

/// The one `message:send` the upstream received, as
/// `(A2A-Extensions header, body)`.
async fn upstream_send(upstream: &MockServer) -> (Option<String>, Value) {
    let sends: Vec<_> = upstream
        .received_requests()
        .await
        .expect("request recording enabled")
        .into_iter()
        .filter(|r| r.url.path() == "/message:send")
        .collect();
    assert_eq!(sends.len(), 1, "exactly one upstream send");
    let header = sends[0]
        .headers
        .get("a2a-extensions")
        .map(|v| v.to_str().unwrap().to_string());
    (header, serde_json::from_slice(&sends[0].body).unwrap())
}

#[tokio::test]
async fn delegate_reemits_upstream_artifacts_in_a_spec_response() {
    let upstream = mock_upstream().await;
    let delegate = spawn_delegate(&upstream).await;

    let payload = send_through(
        &delegate,
        json!({
            "messageId": "smoke-1",
            "role": "ROLE_USER",
            "parts": [ { "text": "hi" } ]
        }),
    )
    .await;

    assert_eq!(
        payload
            .pointer("/task/status/state")
            .and_then(Value::as_str),
        Some("TASK_STATE_COMPLETED"),
        "{payload:#}"
    );
    assert_eq!(
        payload
            .pointer("/task/artifacts/0/parts/0/text")
            .and_then(Value::as_str),
        Some(UPSTREAM_TEXT),
        "{payload:#}"
    );
    assert_eq!(
        payload
            .pointer("/task/artifacts/0/name")
            .and_then(Value::as_str),
        Some("greeting")
    );

    let (activation, body) = upstream_send(&upstream).await;
    assert_eq!(activation, None, "plain message activates nothing upstream");
    assert_ne!(
        body.pointer("/message/messageId"),
        Some(&json!("smoke-1")),
        "the delegate regenerates the message id"
    );
}

#[tokio::test]
async fn delegate_activates_skill_invocation_upstream_for_skill_targeted_message() {
    let upstream = mock_upstream().await;
    let delegate = spawn_delegate(&upstream).await;

    let payload = send_through(
        &delegate,
        json!({
            "messageId": "smoke-2",
            "role": "ROLE_USER",
            "parts": [ { "text": "dispatch" } ],
            "metadata": {
                "a2a.skillId": "echo_loud",
                "a2a.skillParams": { "text": "hi" }
            }
        }),
    )
    .await;
    assert_eq!(
        payload
            .pointer("/task/status/state")
            .and_then(Value::as_str),
        Some("TASK_STATE_COMPLETED"),
        "{payload:#}"
    );

    let (activation, body) = upstream_send(&upstream).await;
    assert_eq!(activation.as_deref(), Some(PROFILE_URI));
    assert_eq!(
        body.pointer("/message/metadata/a2a.skillId"),
        Some(&json!("echo_loud")),
        "routing metadata is forwarded: {body:#}"
    );
}
