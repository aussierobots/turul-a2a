//! Offline smoke test for the critic example agent.
//!
//! Spins up the agent binary on an isolated port and verifies the four
//! end-to-end critic paths produce the expected structured artifacts.
//! No network egress — the agent has no external deps.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const TEST_PORT: u16 = 38013;

struct AgentGuard(Child);

impl Drop for AgentGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_agent() -> AgentGuard {
    // Explicitly set-to-empty rather than `env_remove`. The binary's
    // dotenvy support (if any sibling example added it) treats empty
    // strings as "no opt-in", which is what we want for hermetic smoke.
    let child = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "-p",
            "agent-role-critic-agent",
            "--bin",
            "agent-role-critic-agent",
        ])
        .env("A2A_PORT", TEST_PORT.to_string())
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

fn first_artifact_payload(resp: &Value) -> Value {
    let artifacts = resp["artifacts"]
        .as_array()
        .or_else(|| resp["task"]["artifacts"].as_array())
        .unwrap_or_else(|| panic!("no artifacts in response: {resp:#}"));
    assert!(
        !artifacts.is_empty(),
        "artifacts must be non-empty: {resp:#}"
    );
    let text = artifacts[0]["parts"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("missing text part: {resp:#}"));
    serde_json::from_str(text).expect("artifact payload is JSON")
}

#[tokio::test]
async fn critic_smoke() {
    let _guard = spawn_agent();
    let base = format!("http://127.0.0.1:{TEST_PORT}");
    let client = reqwest::Client::new();

    wait_for_card(&client, &base).await;

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
