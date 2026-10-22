//! Offline smoke test for the post-task-hook example agent.
//!
//! Runs the already-built agent binary (`CARGO_BIN_EXE_*`) on a free
//! loopback port per test (parallel-safe, no nested `cargo run`) and
//! verifies the TerminalHook fires exactly once per skill invocation.
//! The `metrics` skill snapshots the counters before its own hook fires,
//! so each metrics reading excludes that call. No network egress.

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};

/// The card name the spawned binary advertises.
const AGENT_NAME: &str = "Post-Task Hook Agent";

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

/// A port the OS reports free right now, never handed out twice in this
/// test binary (its tests run in parallel). The listener is dropped
/// before the agent binds the port.
fn free_port() -> u16 {
    static ISSUED: Mutex<Vec<u16>> = Mutex::new(Vec::new());
    let mut issued = ISSUED.lock().expect("port registry");
    loop {
        let port = std::net::TcpListener::bind("0.0.0.0:0")
            .and_then(|l| l.local_addr())
            .expect("reserve free port")
            .port();
        if !issued.contains(&port) {
            issued.push(port);
            return port;
        }
    }
}

/// Poll the agent card until this test's own child serves it. Fails fast
/// if the child exits (for example because its port was taken in the
/// meantime) and refuses a card served by a different listener.
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

async fn spawn_agent() -> AgentGuard {
    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_post-task-hook-agent"))
        .env("A2A_PORT", port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent");
    let mut guard = AgentGuard {
        child,
        base: format!("http://127.0.0.1:{port}"),
    };
    wait_until_ready(&mut guard).await;
    guard
}

async fn send_text(base: &str, id: &str, text: &str) -> Value {
    let body = json!({
        "message": {
            "messageId": id,
            "role": "ROLE_USER",
            "parts": [ { "text": text } ]
        }
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/message:send"))
        .header("a2a-version", "1.0")
        .json(&body)
        .send()
        .await
        .expect("send req");
    assert_eq!(resp.status().as_u16(), 200);
    resp.json::<Value>().await.expect("send json")
}

fn task_state(resp: &Value) -> &str {
    resp.pointer("/task/status/state")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected SendMessageResponse.task.status.state: {resp:#}"))
}

/// The JSON payload of the first artifact of a spec `SendMessageResponse`.
fn first_artifact_json(resp: &Value) -> Value {
    let text = resp
        .pointer("/task/artifacts/0/parts/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected SendMessageResponse.task with an artifact: {resp:#}"));
    serde_json::from_str(text).expect("artifact is JSON")
}

/// `(success, failure)` hook counts recorded before this call.
async fn metrics(base: &str, id: &str) -> (u64, u64) {
    let m = first_artifact_json(&send_text(base, id, "metrics").await);
    (
        m["success"].as_u64().expect("success counter"),
        m["failure"].as_u64().expect("failure counter"),
    )
}

#[tokio::test]
async fn hook_fires_on_success() {
    let agent = spawn_agent().await;

    // count 3 → squared 9
    let resp = send_text(&agent.base, "smoke-success-1", "count 3").await;
    assert_eq!(task_state(&resp), "TASK_STATE_COMPLETED");
    assert_eq!(first_artifact_json(&resp), json!({ "squared": 9 }));

    assert_eq!(metrics(&agent.base, "smoke-success-2").await, (1, 0));
}

/// "count three" fails input-schema validation: the handler returns
/// `SkillError::InvalidRequest`, the hook records a Failure, and the
/// executor's mapped `A2aError::InvalidRequest` becomes the failed
/// task's status message.
#[tokio::test]
async fn hook_fires_on_failure() {
    let agent = spawn_agent().await;

    let resp = send_text(&agent.base, "smoke-failure-1", "count three").await;
    assert_eq!(task_state(&resp), "TASK_STATE_FAILED", "{resp:#}");
    let reason = resp
        .pointer("/task/status/message/parts/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        reason.contains("Invalid request: inputSchema violation"),
        "failed task must carry the mapped InvalidRequest reason: {resp:#}"
    );

    assert_eq!(metrics(&agent.base, "smoke-failure-2").await, (0, 1));
}

#[tokio::test]
async fn hook_fires_once_per_call() {
    let agent = spawn_agent().await;

    for i in 0..5 {
        let resp = send_text(&agent.base, &format!("smoke-hot-{i}"), "count 2").await;
        assert_eq!(task_state(&resp), "TASK_STATE_COMPLETED");
    }

    assert_eq!(
        metrics(&agent.base, "smoke-hot-metrics").await,
        (5, 0),
        "exactly one hook fire per call"
    );
    // The metrics call itself fired the hook once more.
    assert_eq!(metrics(&agent.base, "smoke-hot-metrics-2").await, (6, 0));
}
