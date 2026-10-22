//! Offline smoke test. Runs the already-built agent binary
//! (`CARGO_BIN_EXE_*`) on a free loopback port, hits
//! `/.well-known/agent-card.json` to verify the manifest-derived skill is
//! advertised, then sends a JSON-shaped greeting via `POST /message:send`
//! and asserts the offline stub artifact comes back in a spec
//! `SendMessageResponse`.
//!
//! Live-Ollama path is NOT exercised here — see README.md for the manual
//! probe.

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
    let child = Command::new(env!("CARGO_BIN_EXE_skill-manifest-ollama-agent"))
        .env("A2A_PORT", port.to_string())
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

#[tokio::test]
async fn offline_smoke_end_to_end() {
    let agent = spawn_agent().await;
    let client = reqwest::Client::new();

    // 1. AgentCard advertises the manifest-derived skill.
    let card: Value = client
        .get(format!("{}/.well-known/agent-card.json", agent.base))
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

    let resp = client
        .post(format!("{}/message:send", agent.base))
        .header("a2a-version", "1.0")
        .json(&send_body)
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
    let part_text = resp
        .pointer("/task/artifacts/0/parts/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected SendMessageResponse.task with an artifact: {resp:#}"));
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
