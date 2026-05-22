//! Offline smoke test for the planner-router example agent.
//!
//! Spins up the agent binary on an isolated port and verifies the two
//! end-to-end planner paths produce the expected structured artifacts.
//! No network egress — the agent has no external deps.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const TEST_PORT: u16 = 38012;

struct AgentGuard(Child);

impl Drop for AgentGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_agent() -> AgentGuard {
    let child = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "-p",
            "agent-role-planner-router-agent",
            "--bin",
            "agent-role-planner-router-agent",
        ])
        .env("A2A_PORT", TEST_PORT.to_string())
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

#[tokio::test]
async fn planner_router_smoke() {
    let _guard = spawn_agent();
    let base = format!("http://127.0.0.1:{TEST_PORT}");
    let client = reqwest::Client::new();

    wait_for_card(&client, &base).await;

    // 1. AgentCard must advertise both registered skills.
    let card: Value = client
        .get(format!("{base}/.well-known/agent-card.json"))
        .send()
        .await
        .expect("card req")
        .json()
        .await
        .expect("card json");
    let skills = card["skills"].as_array().expect("skills array");
    let ids: Vec<&str> = skills.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        ids.contains(&"add") && ids.contains(&"concat"),
        "agent card must advertise add + concat, got: {ids:?}"
    );

    // 2. "add 3 5" → add skill → {"result": 8}
    let resp = send_text(&client, &base, "smoke-add", "add 3 5").await;
    let text = first_artifact_text(&resp);
    let payload: Value = serde_json::from_str(text).expect("add artifact is JSON");
    assert_eq!(
        payload["result"], 8,
        "add result should be 8, got payload: {payload:#}"
    );

    // 3. "concat: foo bar baz" → concat skill → {"joined": "foo bar baz"}
    let resp = send_text(&client, &base, "smoke-concat", "concat: foo bar baz").await;
    let text = first_artifact_text(&resp);
    let payload: Value = serde_json::from_str(text).expect("concat artifact is JSON");
    assert_eq!(
        payload["joined"].as_str(),
        Some("foo bar baz"),
        "concat joined should be 'foo bar baz', got payload: {payload:#}"
    );
}
