//! Offline smoke test for the planner-router example agent.
//!
//! Runs the already-built agent binary (`CARGO_BIN_EXE_*`) on an OS-assigned
//! loopback port per test (parallel-safe, no nested `cargo run`) and
//! verifies both planner routes plus the no-rule-matched fallback.
//! No network egress — the agent has no external deps.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

/// The card name the spawned binary advertises.
const AGENT_NAME: &str = "Planner-Router Agent";

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

async fn spawn_agent() -> AgentGuard {
    let child = Command::new(env!("CARGO_BIN_EXE_agent-role-planner-router-agent"))
        .env("A2A_PORT", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent");
    let mut guard = AgentGuard {
        child,
        base: String::new(),
    };
    let port = bound_port(&mut guard.child).await;
    guard.base = format!("http://127.0.0.1:{port}");
    wait_until_ready(&mut guard).await;
    guard
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
