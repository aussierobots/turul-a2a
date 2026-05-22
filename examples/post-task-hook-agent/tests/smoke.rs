//! Offline smoke test for the post-task-hook example agent.
//!
//! Spins up the agent binary on an isolated port and verifies the
//! TerminalHook fires after each skill invocation. No network egress.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

// Each test owns a unique port — `cargo test` runs tests in parallel by
// default, so a shared port would collide. 38014 is the base; case offsets
// match the test names.
const TEST_PORT_BASE: u16 = 38014;

struct AgentGuard(Child);

impl Drop for AgentGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_agent(port: u16) -> AgentGuard {
    let child = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "-p",
            "post-task-hook-agent",
            "--bin",
            "post-task-hook-agent",
        ])
        .env("A2A_PORT", port.to_string())
        .env("OLLAMA_BASE_URL", "")
        .env("RUN_OLLAMA_SMOKE", "")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent");
    AgentGuard(child)
}

async fn wait_for_card(client: &reqwest::Client, base: &str) {
    let url = format!("{base}/.well-known/agent-card.json");
    for _ in 0..240 {
        if let Ok(r) = client.get(&url).send().await
            && r.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("agent did not become ready at {url} within 60s");
}

async fn send_text(client: &reqwest::Client, base: &str, id: &str, text: &str) -> Value {
    let body = json!({
        "message": {
            "messageId": id,
            "role": "ROLE_USER",
            "parts": [ { "text": text } ]
        }
    });
    client
        .post(format!("{base}/message:send"))
        .header("a2a-version", "1.0")
        .json(&body)
        .send()
        .await
        .expect("send req")
        .json::<Value>()
        .await
        .expect("send json")
}

fn first_artifact_text(resp: &Value) -> Option<&str> {
    let artifacts = resp["artifacts"]
        .as_array()
        .or_else(|| resp["task"]["artifacts"].as_array())?;
    artifacts.first()?["parts"][0]["text"].as_str()
}

async fn metrics(client: &reqwest::Client, base: &str, id: &str) -> Value {
    let resp = send_text(client, base, id, "metrics").await;
    let text = first_artifact_text(&resp)
        .unwrap_or_else(|| panic!("metrics response missing artifact text: {resp:#}"));
    serde_json::from_str(text).expect("metrics artifact is JSON")
}

#[tokio::test]
async fn hook_fires_on_success() {
    let port = TEST_PORT_BASE; // 38014
    let _guard = spawn_agent(port);
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    wait_for_card(&client, &base).await;

    // count 3 → squared 9
    let resp = send_text(&client, &base, "smoke-success-1", "count 3").await;
    let text =
        first_artifact_text(&resp).unwrap_or_else(|| panic!("no artifact for count: {resp:#}"));
    let payload: Value = serde_json::from_str(text).expect("count artifact is JSON");
    assert_eq!(
        payload["squared"], 9,
        "expected squared=9, got: {payload:#}"
    );

    // metrics → success >= 1
    let m = metrics(&client, &base, "smoke-success-2").await;
    let success = m["success"].as_u64().expect("success counter");
    assert!(success >= 1, "expected success >= 1, got metrics: {m:#}");
}

#[tokio::test]
async fn hook_fires_on_failure() {
    let port = TEST_PORT_BASE + 1; // 38015
    let _guard = spawn_agent(port);
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    wait_for_card(&client, &base).await;

    // "count three" → planner forwards non-number → InvalidRequest → hook fires Failure.
    // The HTTP response itself is an A2A error envelope; what matters is that
    // metrics records the failure.
    let _ = send_text(&client, &base, "smoke-failure-1", "count three").await;

    let m = metrics(&client, &base, "smoke-failure-2").await;
    let failure = m["failure"].as_u64().expect("failure counter");
    assert!(failure >= 1, "expected failure >= 1, got metrics: {m:#}");
}

#[tokio::test]
async fn hook_fires_once_per_call() {
    let port = TEST_PORT_BASE + 2; // 38016
    let _guard = spawn_agent(port);
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    wait_for_card(&client, &base).await;

    for i in 0..5 {
        let id = format!("smoke-hot-{i}");
        let _ = send_text(&client, &base, &id, "count 2").await;
    }

    let m = metrics(&client, &base, "smoke-hot-metrics").await;
    let success = m["success"].as_u64().expect("success counter");
    assert!(
        success >= 5,
        "expected success >= 5 after 5 calls, got metrics: {m:#}"
    );
}
