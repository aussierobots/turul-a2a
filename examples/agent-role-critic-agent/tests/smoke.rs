//! Offline smoke test for the critic example agent.
//!
//! Runs the already-built agent binary (`CARGO_BIN_EXE_*`) on an OS-assigned
//! loopback port (parallel-safe, no nested `cargo run`) and verifies the
//! four end-to-end critic paths produce the expected structured
//! artifacts. No network egress — the agent has no external deps.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

/// The card name the spawned binary advertises.
const AGENT_NAME: &str = "Critic Agent";

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
    let child = Command::new(env!("CARGO_BIN_EXE_agent-role-critic-agent"))
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

async fn send_text(client: &reqwest::Client, base: &str, id: &str, text: &str) -> Value {
    let body = json!({
        "message": {
            "messageId": id,
            "role": "ROLE_USER",
            "parts": [ { "text": text } ]
        }
    });
    let resp = client
        .post(format!("{base}/message:send"))
        .header("a2a-version", "1.0")
        .json(&body)
        .send()
        .await
        .expect("send req");
    assert_eq!(resp.status().as_u16(), 200);
    resp.json::<Value>().await.expect("send json")
}

/// The JSON payload of the first artifact of a spec `SendMessageResponse`
/// (`{"task": {...}}`) for a completed task.
fn first_artifact_payload(resp: &Value) -> Value {
    assert_eq!(
        resp.pointer("/task/status/state").and_then(Value::as_str),
        Some("TASK_STATE_COMPLETED"),
        "{resp:#}"
    );
    let text = resp
        .pointer("/task/artifacts/0/parts/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected SendMessageResponse.task with an artifact: {resp:#}"));
    serde_json::from_str(text).expect("artifact payload is JSON")
}

#[tokio::test]
async fn critic_smoke() {
    let agent = spawn_agent().await;
    let base = agent.base.clone();
    let client = reqwest::Client::new();

    // 0. AgentCard must advertise both registered skills.
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
        ids.contains(&"validate_against_schema") && ids.contains(&"check_invariants"),
        "agent card must advertise both critic skills, got: {ids:?}"
    );

    // 1. validate_against_schema with a value satisfying the schema.
    let inbound = json!({
        "kind": "validate_against_schema",
        "value": {"name": "ok", "count": 3},
        "schema": {
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "count": {"type": "integer"}
            },
            "required": ["name", "count"]
        }
    });
    let resp = send_text(&client, &base, "smoke-valid", &inbound.to_string()).await;
    let payload = first_artifact_payload(&resp);
    assert_eq!(
        payload["valid"],
        json!(true),
        "expected valid=true: {payload:#}"
    );
    assert_eq!(
        payload["errors"],
        json!([]),
        "expected empty errors: {payload:#}"
    );

    // 2. validate_against_schema with a value missing a required field.
    let inbound = json!({
        "kind": "validate_against_schema",
        "value": {"name": "missing-count"},
        "schema": {
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "count": {"type": "integer"}
            },
            "required": ["name", "count"]
        }
    });
    let resp = send_text(&client, &base, "smoke-invalid", &inbound.to_string()).await;
    let payload = first_artifact_payload(&resp);
    assert_eq!(
        payload["valid"],
        json!(false),
        "expected valid=false: {payload:#}"
    );
    let errors = payload["errors"]
        .as_array()
        .expect("errors must be an array");
    assert!(
        !errors.is_empty(),
        "errors must be non-empty for violation: {payload:#}"
    );
    assert!(
        errors[0].as_str().is_some_and(|s| !s.is_empty()),
        "errors[0] must be a non-empty string: {payload:#}"
    );

    // 3. check_invariants where the value passes all four invariant kinds.
    let inbound = json!({
        "kind": "check_invariants",
        "value": ["foo", "bar", "baz"],
        "invariants": [
            {"name": "is_non_empty", "check": "non_empty"},
            {"name": "min_3", "check": "min_length", "args": {"min": 3}},
            {"name": "max_5", "check": "max_length", "args": {"max": 5}},
            {"name": "has_bar", "check": "contains", "args": {"needle": "bar"}}
        ]
    });
    let resp = send_text(&client, &base, "smoke-pass", &inbound.to_string()).await;
    let payload = first_artifact_payload(&resp);
    assert_eq!(
        payload["verdict"], "pass",
        "expected verdict=pass: {payload:#}"
    );
    assert_eq!(
        payload["failures"],
        json!([]),
        "failures must be empty on pass: {payload:#}"
    );

    // 4. check_invariants where one invariant fails (min_length below threshold).
    let inbound = json!({
        "kind": "check_invariants",
        "value": "hi",
        "invariants": [
            {"name": "is_non_empty", "check": "non_empty"},
            {"name": "min_5", "check": "min_length", "args": {"min": 5}}
        ]
    });
    let resp = send_text(&client, &base, "smoke-fail", &inbound.to_string()).await;
    let payload = first_artifact_payload(&resp);
    assert_eq!(
        payload["verdict"], "fail",
        "expected verdict=fail: {payload:#}"
    );
    let failures = payload["failures"]
        .as_array()
        .expect("failures must be an array");
    assert_eq!(
        failures.len(),
        1,
        "expected exactly one failure: {payload:#}"
    );
    assert_eq!(
        failures[0]["name"], "min_5",
        "failing invariant name should be min_5: {payload:#}"
    );
    assert!(
        failures[0]["reason"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "failure reason must be a non-empty string: {payload:#}"
    );
}
