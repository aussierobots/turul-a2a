//! Offline smoke test. Boots the example agent on a free port, hits
//! `/.well-known/agent-card.json` to verify the manifest-derived skill is
//! advertised, then sends a JSON-shaped greeting via `POST /message:send`
//! and asserts the offline stub artifact comes back.
//!
//! Live-Ollama path is NOT exercised here — see README.md for the manual
//! probe.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const TEST_PORT: u16 = 38010;

struct AgentGuard(Child);

impl Drop for AgentGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_agent() -> AgentGuard {
    // `cargo run -p skill-manifest-ollama-agent` reuses the workspace
    // target dir; in CI the binary may already be built. Pass A2A_PORT so
    // the test owns its own port and never collides with `cargo run`.
    let child = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "-p",
            "skill-manifest-ollama-agent",
            "--bin",
            "skill-manifest-ollama-agent",
        ])
        .env("A2A_PORT", TEST_PORT.to_string())
        // Explicitly set-to-empty rather than `env_remove`. The binary's
        // `dotenvy::dotenv()` autoload at startup honours already-set env
        // vars and does NOT override them; empty values are present-but-
        // empty, so dotenvy will not repopulate them from a local `.env`.
        // main()'s `ollama_base_url()` helper treats empty as offline.
        // This keeps the offline smoke hermetic even when a developer has
        // a live-mode `.env` in the example crate's working directory.
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
    for _ in 0..120 {
        if let Ok(r) = client.get(&url).send().await
            && r.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("agent did not become ready at {url} within 30s");
}

#[tokio::test]
async fn offline_smoke_end_to_end() {
    let _guard = spawn_agent();
    let base = format!("http://127.0.0.1:{TEST_PORT}");
    let client = reqwest::Client::new();

    wait_for_card(&client, &base).await;

    // 1. AgentCard advertises the manifest-derived skill.
    let card: Value = client
        .get(format!("{base}/.well-known/agent-card.json"))
        .send()
        .await
        .expect("card req")
        .json()
        .await
        .expect("card json");
    let skills = card["skills"].as_array().expect("skills array");
    assert!(
        skills.iter().any(|s| s["id"] == "greet"),
        "manifest skill `greet` must appear on AgentCard: {card:#}"
    );

    // 2. Send a JSON-shaped greeting; assert the offline stub artifact.
    let send_body = json!({
        "message": {
            "messageId": "smoke-1",
            "role": "ROLE_USER",
            "parts": [
                { "text": r#"{"user":{"name":"Ada"},"style":"formal"}"# }
            ]
        }
    });

    let resp: Value = client
        .post(format!("{base}/message:send"))
        .header("a2a-version", "1.0")
        .json(&send_body)
        .send()
        .await
        .expect("send req")
        .json()
        .await
        .expect("send json");

    // Response shape: a Task wrapper. Drill into artifacts[].parts[].text
    // and parse it as the JSON greeting payload.
    let artifacts = resp["artifacts"]
        .as_array()
        .or_else(|| resp["task"]["artifacts"].as_array())
        .unwrap_or_else(|| panic!("no artifacts in response: {resp:#}"));
    assert!(
        !artifacts.is_empty(),
        "artifacts must be non-empty: {resp:#}"
    );

    let part_text = artifacts[0]["parts"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("missing text part: {resp:#}"));
    let greeting_payload: Value = serde_json::from_str(part_text).expect("artifact text is JSON");
    let greeting = greeting_payload["greeting"]
        .as_str()
        .expect("greeting field");
    assert!(
        greeting.contains("Ada"),
        "greeting should name the user: {greeting:?}"
    );
    assert!(
        greeting.contains("offline stub"),
        "smoke test must hit the offline path: {greeting:?}"
    );
}
