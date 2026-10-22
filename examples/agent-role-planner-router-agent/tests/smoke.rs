//! Offline smoke test for the planner-router example agent.
//!
//! Runs the already-built agent binary (`CARGO_BIN_EXE_*`) on a free
//! loopback port per test (parallel-safe, no nested `cargo run`) and
//! verifies both planner routes plus the no-rule-matched fallback.
//! No network egress — the agent has no external deps.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

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
/// the agent binds it.
fn free_port() -> u16 {
    std::net::TcpListener::bind("0.0.0.0:0")
        .and_then(|l| l.local_addr())
        .expect("reserve free port")
        .port()
}

async fn spawn_agent() -> AgentGuard {
    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_agent-role-planner-router-agent"))
        .env("A2A_PORT", port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent");
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
    panic!("agent did not become ready at {url} within 15s");
}

/// Send `text` and return `(skill name on the artifact, artifact JSON)`
/// from the spec `SendMessageResponse` (`{"task": {...}}`).
async fn route(base: &str, id: &str, text: &str) -> (String, Value) {
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
    let resp: Value = resp.json().await.expect("send json");
    assert_eq!(
        resp.pointer("/task/status/state").and_then(Value::as_str),
        Some("TASK_STATE_COMPLETED"),
        "{resp:#}"
    );
    let artifact = resp
        .pointer("/task/artifacts/0")
        .unwrap_or_else(|| panic!("expected SendMessageResponse.task with an artifact: {resp:#}"));
    let skill = artifact["name"].as_str().unwrap_or_default().to_string();
    let text = artifact
        .pointer("/parts/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing text part: {resp:#}"));
    (skill, serde_json::from_str(text).expect("artifact is JSON"))
}

#[tokio::test]
async fn agent_card_advertises_both_skills() {
    let agent = spawn_agent().await;
    let card: Value = reqwest::get(format!("{}/.well-known/agent-card.json", agent.base))
        .await
        .expect("card req")
        .json()
        .await
        .expect("card json");
    let mut ids: Vec<&str> = card["skills"]
        .as_array()
        .expect("skills array")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, ["add", "concat"]);
}

#[tokio::test]
async fn routes_add_keyword_and_infix_to_add() {
    let agent = spawn_agent().await;
    let (skill, payload) = route(&agent.base, "smoke-add", "add 3 5").await;
    assert_eq!(skill, "add");
    assert_eq!(payload["result"], 8, "{payload:#}");

    let (skill, payload) = route(&agent.base, "smoke-infix", "what is 2 + 40").await;
    assert_eq!(skill, "add");
    assert_eq!(payload["result"], 42, "{payload:#}");
}

#[tokio::test]
async fn routes_concat_prefix_to_concat() {
    let agent = spawn_agent().await;
    let (skill, payload) = route(&agent.base, "smoke-concat", "concat: foo bar baz").await;
    assert_eq!(skill, "concat");
    assert_eq!(payload, json!({ "joined": "foo bar baz" }));
}

/// No rule matches — including an `add` with a non-numeric operand — so
/// the planner falls back to a labelled echo instead of misrouting to
/// `add` or failing.
#[tokio::test]
async fn unmatched_input_falls_back_to_labelled_echo() {
    let agent = spawn_agent().await;
    let (skill, payload) = route(&agent.base, "smoke-fallback", "add three 5").await;
    assert_eq!(skill, "concat");
    assert_eq!(payload, json!({ "joined": "unrecognized: add three 5" }));
}
