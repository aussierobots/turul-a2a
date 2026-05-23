//! Rust interop client for `remote-delegate-agent`.
//!
//! Sends a greeting payload to the delegate, which forwards it to the
//! upstream `skill-manifest-ollama-agent`. The "offline stub" marker
//! in the returned artifact body proves the two-hop chain completed:
//! this client only knows about the delegate; the upstream is
//! invisible at the wire boundary.
//!
//! Run:
//!   cargo run -p skill-manifest-ollama-agent      # upstream :3010
//!   cargo run -p remote-delegate-agent            # delegate :3016
//!   cargo run -p interop-remote-delegate-rust     # this client
use std::process::ExitCode;

use serde_json::json;
use turul_a2a_client::prelude::*;

const DEFAULT_AGENT_BASE_URL: &str = "http://localhost:3016";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let base_url =
        std::env::var("A2A_BASE_URL").unwrap_or_else(|_| DEFAULT_AGENT_BASE_URL.to_string());
    println!("target: {base_url}");

    let client = A2aClient::discover(&base_url).await?;
    if let Some(card) = client.agent_card() {
        println!(
            "delegate AgentCard: {} v{} (skills={:?})",
            card.name,
            card.version,
            card.skills
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>()
        );
    }

    let payload = json!({
        "user": { "name": "Ada" },
        "style": "formal",
    });
    let payload_text = payload.to_string();
    println!("--- Send (chain: client → delegate → upstream) ---");
    println!("payload: {payload_text}");

    let request = MessageBuilder::new().text(payload_text);
    let response = client.send_message(request).await?;

    let artifact_body = match response {
        SendResponse::Task(task) => {
            let state = task
                .status()
                .and_then(|s| s.state().ok())
                .map(|s| format!("{s:?}"))
                .unwrap_or_else(|| "<unknown>".to_string());
            println!(
                "task: id={} state={} artifacts={}",
                task.id(),
                state,
                task.artifacts().len()
            );
            artifact_text(&task)
        }
        SendResponse::Message(msg) => msg.text_parts().join(" "),
        other => {
            return Err(format!("unexpected response variant: {other:?}").into());
        }
    };

    println!("artifact: {artifact_body}");

    // The "offline stub" marker in the artifact proves the chain
    // reached the upstream: this client never spoke to it directly,
    // but the artifact body is the upstream agent's offline-mode
    // greeting handler output.
    if !artifact_body.contains("offline stub") {
        return Err(format!(
            "artifact missing 'offline stub' marker — chain did not reach the upstream offline-mode path: {artifact_body}"
        )
        .into());
    }
    if !artifact_body.contains("Ada") {
        return Err(format!("artifact missing caller name 'Ada': {artifact_body}").into());
    }

    println!("=== OK: two-hop chain returned the upstream's artifact ===");
    Ok(())
}
