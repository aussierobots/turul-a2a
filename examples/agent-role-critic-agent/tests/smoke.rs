//! Offline smoke test for the critic example agent.
//!
//! Runs the already-built agent binary (`CARGO_BIN_EXE_*`) on a free
//! loopback port (parallel-safe, no nested `cargo run`) and verifies the
//! four end-to-end critic paths produce the expected structured
//! artifacts. No network egress — the agent has no external deps.

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
    let child = Command::new(env!("CARGO_BIN_EXE_agent-role-critic-agent"))
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
