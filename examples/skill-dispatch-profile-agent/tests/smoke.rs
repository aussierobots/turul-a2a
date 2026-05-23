//! Offline smoke test for the skill-dispatch profile example agent.
//!
//! Spins up the agent binary on an isolated port and verifies the
//! end-to-end dispatch paths produce the expected structured artifacts.
//! The third test exercises the failure mode when the profile is not
//! activated (no `a2a.skillId` metadata key).
//!
//! No network egress.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const PROFILE_URI: &str = "https://turul.dev/a2a/extensions/skill-invocation/v1";

struct AgentGuard {
    child: Child,
    port: u16,
}

impl Drop for AgentGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Each test gets a unique port so the four `#[tokio::test]` cases — which
/// `cargo test` runs in parallel within one binary — do not collide on a
/// shared listener. 38015 is the base; the offset is the caller-provided
/// nonce.
fn spawn_agent(port_offset: u16) -> AgentGuard {
    let port = 38015 + port_offset;
    let child = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "-p",
            "skill-dispatch-profile-agent",
            "--bin",
            "skill-dispatch-profile-agent",
        ])
        .env("A2A_PORT", port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent");
    AgentGuard { child, port }
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

async fn send_message(
    client: &reqwest::Client,
    base: &str,
    body: Value,
    activate_profile: bool,
) -> reqwest::Response {
    let mut req = client
        .post(format!("{base}/message:send"))
        .header("a2a-version", "1.0")
        .json(&body);
    if activate_profile {
        req = req.header("A2A-Extensions", PROFILE_URI);
    }
    req.send().await.expect("send req")
}

fn first_artifact_text(resp: &Value) -> &str {
    let artifacts = resp["artifacts"]
        .as_array()
        .or_else(|| resp["task"]["artifacts"].as_array())
        .unwrap_or_else(|| panic!("no artifacts in response: {resp:#}"));
    assert!(
        !artifacts.is_empty(),
        "artifacts must be non-empty: {resp:#}"
    );
    artifacts[0]["parts"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("missing text part: {resp:#}"))
}

fn dispatch_body(message_id: &str, skill_id: &str, params: Value) -> Value {
    json!({
        "message": {
            "messageId": message_id,
            "role": "ROLE_USER",
            "parts": [ { "text": format!("dispatch:{skill_id}") } ],
            "metadata": {
                "a2a.skillId": skill_id,
                "a2a.skillParams": params,
            }
        }
    })
}

#[tokio::test]
async fn agent_card_advertises_profile_extension() {
    // see spawn_agent — unique port per test for parallel-safe execution.
    let guard = spawn_agent(0);
    let base = format!("http://127.0.0.1:{}", guard.port);
    let client = reqwest::Client::new();

    wait_for_card(&client, &base).await;

    let card: Value = client
        .get(format!("{base}/.well-known/agent-card.json"))
        .send()
        .await
        .expect("card req")
        .json()
        .await
        .expect("card json");

    let extensions = card["capabilities"]["extensions"]
        .as_array()
        .expect("capabilities.extensions array");
    let uris: Vec<&str> = extensions
        .iter()
        .filter_map(|e| e["uri"].as_str())
        .collect();
    assert!(
        uris.contains(&PROFILE_URI),
        "card must advertise profile URI, got: {uris:?}"
    );

    let skills = card["skills"].as_array().expect("skills array");
    let ids: Vec<&str> = skills.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        ids.contains(&"echo_loud") && ids.contains(&"reverse"),
        "agent card must advertise echo_loud + reverse, got: {ids:?}"
    );
}

#[tokio::test]
async fn dispatches_to_echo_loud() {
    // see spawn_agent — unique port per test for parallel-safe execution.
    let guard = spawn_agent(1);
    let base = format!("http://127.0.0.1:{}", guard.port);
    let client = reqwest::Client::new();

    wait_for_card(&client, &base).await;

    let body = dispatch_body("smoke-echo", "echo_loud", json!({ "text": "hello" }));
    let resp = send_message(&client, &base, body, true).await;

    // Echo header must be present on the response.
    let echo = resp
        .headers()
        .get("a2a-extensions")
        .expect("response must echo A2A-Extensions");
    assert_eq!(echo.to_str().unwrap(), PROFILE_URI);

    let payload: Value = resp.json().await.expect("send json");
    let text = first_artifact_text(&payload);
    let artifact: Value = serde_json::from_str(text).expect("artifact is JSON");
    assert_eq!(
        artifact["shouted"].as_str(),
        Some("HELLO"),
        "echo_loud should uppercase input; got payload: {artifact:#}"
    );
}

#[tokio::test]
async fn dispatches_to_reverse() {
    // see spawn_agent — unique port per test for parallel-safe execution.
    let guard = spawn_agent(2);
    let base = format!("http://127.0.0.1:{}", guard.port);
    let client = reqwest::Client::new();

    wait_for_card(&client, &base).await;

    let body = dispatch_body("smoke-reverse", "reverse", json!({ "text": "abc" }));
    let resp = send_message(&client, &base, body, true).await;

    let payload: Value = resp.json().await.expect("send json");
    let text = first_artifact_text(&payload);
    let artifact: Value = serde_json::from_str(text).expect("artifact is JSON");
    assert_eq!(
        artifact["reversed"].as_str(),
        Some("cba"),
        "reverse should reverse characters; got payload: {artifact:#}"
    );
}

#[tokio::test]
async fn missing_skill_id_fails_task() {
    // see spawn_agent — unique port per test for parallel-safe execution.
    let guard = spawn_agent(3);
    let base = format!("http://127.0.0.1:{}", guard.port);
    let client = reqwest::Client::new();

    wait_for_card(&client, &base).await;

    // No metadata, no activation header — the executor falls into its
    // failure path because there is no `a2a.skillId` to dispatch on.
    let body = json!({
        "message": {
            "messageId": "smoke-missing",
            "role": "ROLE_USER",
            "parts": [ { "text": "no dispatch intent here" } ]
        }
    });
    let resp = send_message(&client, &base, body, false).await;
    assert!(
        resp.status().is_success(),
        "task-level failure is reported via TaskStatus, not HTTP error"
    );
    let payload: Value = resp.json().await.expect("send json");
    let state = payload
        .pointer("/status/state")
        .or_else(|| payload.pointer("/task/status/state"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("missing task status.state: {payload:#}"));
    assert_eq!(
        state, "TASK_STATE_FAILED",
        "missing a2a.skillId must produce a Failed task; got payload: {payload:#}"
    );
    let message_text = payload
        .pointer("/status/message/parts/0/text")
        .or_else(|| payload.pointer("/task/status/message/parts/0/text"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        message_text.contains("a2a.skillId"),
        "failure message should cite the missing metadata key; got: {message_text}"
    );
}
