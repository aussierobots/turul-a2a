//! End-to-end smoke for the remote-delegate-agent.
//!
//! The upstream A2A agent is an in-process `wiremock` server serving an
//! agent card (advertising the skill-invocation profile) and a spec
//! `SendMessageResponse`. The delegate is the already-built binary
//! (`CARGO_BIN_EXE_*`) on an OS-assigned loopback port, pointed at the mock.
//! Tests are hermetic and parallel-safe.
//!
//! Asserts that the delegate re-emits the upstream's artifacts in a
//! spec `SendMessageResponse`, and that it forwards the caller's
//! activation of the profile to the upstream call (`A2A-Extensions`
//! header): routing metadata alone activates nothing.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROFILE_URI: &str = "https://turul.dev/a2a/extensions/skill-invocation/v1";
const UPSTREAM_TEXT: &str = "hello from the upstream";

/// The card name the spawned binary advertises.
const AGENT_NAME: &str = "Remote Delegate Agent";

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

/// Read the agent's stdout until it reports the address it bound
/// (`... listening on http://<addr>`). The agent binds port 0, so the OS
/// assigns a port no other process holds. The reader thread keeps draining
/// stdout afterwards so the agent never blocks on a full pipe.
async fn bound_port(child: &mut Child) -> u16 {
    let stdout = child.stdout.take().expect("agent stdout is piped");
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut tx = Some(tx);
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some((_, addr)) = line.split_once(" listening on http://")
                && let Ok(addr) = addr.parse::<SocketAddr>()
                && let Some(tx) = tx.take()
            {
                let _ = tx.send(addr.port());
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(15), rx)
        .await
        .expect("agent reported its bound address within 15s")
        .expect("agent exited before reporting its bound address")
}

/// Poll the agent card until this test's own child serves it. Fails fast
/// if the child exits and refuses a card served by a different listener.
async fn wait_until_ready(guard: &mut AgentGuard) {
    let url = format!("{}/.well-known/agent-card.json", guard.base);
    for _ in 0..150 {
        if let Some(status) = guard.child.try_wait().expect("poll agent process") {
            panic!("agent exited before it was ready ({status})");
        }
        if let Ok(r) = reqwest::get(&url).await
            && r.status().is_success()
        {
            let card: Value = r.json().await.expect("agent card JSON");
            assert_eq!(
                card["name"], AGENT_NAME,
                "{url} is served by another process"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("agent did not become ready at {url} within 15s");
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
    let child = Command::new(env!("CARGO_BIN_EXE_remote-delegate-agent"))
        .env("A2A_PORT", "0")
        .env("REMOTE_AGENT_URL", upstream.uri())
        // Short-circuit any local .env so the delegate's auth/timeout
        // settings are deterministic across machines.
        .env("REMOTE_AGENT_BEARER", "")
        .env("REMOTE_TIMEOUT_SECS", "10")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn delegate");
    let mut guard = AgentGuard {
        child,
        base: String::new(),
    };
    let port = bound_port(&mut guard.child).await;
    guard.base = format!("http://127.0.0.1:{port}");
    wait_until_ready(&mut guard).await;
    guard
}

async fn send_through(delegate: &AgentGuard, message: Value, activate: Option<&str>) -> Value {
    let mut req = reqwest::Client::new()
        .post(format!("{}/message:send", delegate.base))
        .header("a2a-version", "1.0");
    if let Some(uri) = activate {
        req = req.header("a2a-extensions", uri);
    }
    let resp = req
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
        None,
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

fn skill_targeted(message_id: &str) -> Value {
    json!({
        "messageId": message_id,
        "role": "ROLE_USER",
        "parts": [ { "text": "dispatch" } ],
        "metadata": {
            "a2a.skillId": "echo_loud",
            "a2a.skillParams": { "text": "hi" }
        }
    })
}

#[tokio::test]
async fn delegate_advertises_the_profile_the_upstream_advertises() {
    let upstream = mock_upstream().await;
    let delegate = spawn_delegate(&upstream).await;
    let card: Value = reqwest::get(format!("{}/.well-known/agent-card.json", delegate.base))
        .await
        .expect("card")
        .json()
        .await
        .expect("card JSON");
    assert_eq!(
        card.pointer("/capabilities/extensions/0/uri"),
        Some(&json!(PROFILE_URI)),
        "{card:#}"
    );
}

#[tokio::test]
async fn delegate_forwards_the_callers_profile_activation_upstream() {
    let upstream = mock_upstream().await;
    let delegate = spawn_delegate(&upstream).await;

    let payload = send_through(&delegate, skill_targeted("smoke-2"), Some(PROFILE_URI)).await;
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

/// Routing metadata without the caller's activation header: the delegate
/// forwards the metadata but activates nothing upstream.
#[tokio::test]
async fn delegate_does_not_activate_upstream_without_caller_activation() {
    let upstream = mock_upstream().await;
    let delegate = spawn_delegate(&upstream).await;

    send_through(&delegate, skill_targeted("smoke-3"), None).await;

    let (activation, body) = upstream_send(&upstream).await;
    assert_eq!(activation, None);
    assert_eq!(
        body.pointer("/message/metadata/a2a.skillId"),
        Some(&json!("echo_loud")),
        "routing metadata is still forwarded: {body:#}"
    );
}
