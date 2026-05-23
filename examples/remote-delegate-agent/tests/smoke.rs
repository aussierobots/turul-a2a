//! End-to-end smoke for the remote-delegate-agent.
//!
//! Spawns two A2A servers on fixed test ports:
//!   1. `skill-manifest-ollama-agent` as the *upstream*, in its offline
//!      stub mode (no Ollama required).
//!   2. `remote-delegate-agent` as the *delegate*, pointed at the upstream.
//!
//! Sends a JSON-shaped greeting to the delegate and asserts the
//! artifact body came from the upstream offline stub (it includes the
//! marker string "offline stub").
//!
//! Tests both agents shut down cleanly via the `AgentGuard` Drop impl.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const UPSTREAM_PORT: u16 = 38110;
const DELEGATE_PORT: u16 = 38116;

struct AgentGuard(Child);

impl Drop for AgentGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_upstream() -> AgentGuard {
    let child = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "-p",
            "skill-manifest-ollama-agent",
            "--bin",
            "skill-manifest-ollama-agent",
        ])
        .env("A2A_PORT", UPSTREAM_PORT.to_string())
        // Keep upstream firmly in offline mode. Empty-not-unset so
        // dotenvy autoload cannot repopulate a live URL from a developer's
        // local .env.
        .env("OLLAMA_BASE_URL", "")
        .env("RUN_OLLAMA_SMOKE", "")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn upstream");
    AgentGuard(child)
}

fn spawn_delegate() -> AgentGuard {
    let child = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "-p",
            "remote-delegate-agent",
            "--bin",
            "remote-delegate-agent",
        ])
        .env("A2A_PORT", DELEGATE_PORT.to_string())
        .env(
            "REMOTE_AGENT_URL",
            format!("http://127.0.0.1:{UPSTREAM_PORT}"),
        )
        // Short-circuit any local .env so the delegate's auth/timeout
        // settings are deterministic across machines.
        .env("REMOTE_AGENT_BEARER", "")
        .env("REMOTE_TIMEOUT_SECS", "10")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn delegate");
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
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("agent never came up at {url}");
}

#[tokio::test(flavor = "multi_thread")]
async fn delegate_forwards_to_upstream_and_returns_offline_stub_artifact() {
    let http = reqwest::Client::new();
    let upstream_base = format!("http://127.0.0.1:{UPSTREAM_PORT}");
    let delegate_base = format!("http://127.0.0.1:{DELEGATE_PORT}");

    // The delegate runs upstream discovery at startup, so the upstream
    // must be answering on its port BEFORE the delegate spawns —
    // otherwise the delegate fails fast and never binds.
    let _upstream = spawn_upstream();
    wait_for_card(&http, &upstream_base).await;
    let _delegate = spawn_delegate();
    wait_for_card(&http, &delegate_base).await;

    // Send through the delegate. The payload is the same shape the
    // upstream's `greet` skill expects.
    let body = json!({
        "message": {
            "messageId": "smoke-1",
            "role": "ROLE_USER",
            "parts": [{
                "text": r#"{"user":{"name":"Ada"},"style":"formal"}"#
            }]
        }
    });

    let resp = http
        .post(format!("{delegate_base}/message:send"))
        .header("Content-Type", "application/json")
        .header("a2a-version", "1.0")
        .json(&body)
        .send()
        .await
        .expect("send to delegate");
    assert!(
        resp.status().is_success(),
        "delegate returned {status}: {body}",
        status = resp.status(),
        body = resp.text().await.unwrap_or_default()
    );

    let payload: Value = resp.json().await.expect("delegate response is JSON");

    // Walk the response Task → artifacts[0] → parts → text and look for
    // the upstream offline-stub marker. The delegate may have rewrapped
    // artifact_id / artifact_name, but the part body must be intact.
    let artifacts = payload
        .pointer("/task/artifacts")
        .or_else(|| payload.pointer("/artifacts"))
        .and_then(Value::as_array)
        .expect("response carries artifacts array");
    assert!(
        !artifacts.is_empty(),
        "delegate emitted no artifacts: {payload}"
    );

    let combined = artifacts
        .iter()
        .filter_map(|a| a.pointer("/parts"))
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|p| p.pointer("/text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        combined.contains("offline stub"),
        "expected upstream offline-stub marker in delegate artifact; got: {combined}"
    );
    assert!(
        combined.contains("Ada"),
        "expected upstream greeting to include caller name; got: {combined}"
    );
}
