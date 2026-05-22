//! Rust interop client for `skill-manifest-ollama-agent`.
//!
//! Sends the manifest's documented `greet` input (`user.name`, `style`) and
//! prints the structured `greeting` artifact returned by the agent.
//!
//! Run:
//!   # terminal 1
//!   cargo run -p skill-manifest-ollama-agent
//!   # terminal 2
//!   cargo run -p interop-skill-manifest-ollama-rust
use std::process::ExitCode;

use serde_json::json;
use turul_a2a_client::prelude::*;

const DEFAULT_AGENT_BASE_URL: &str = "http://localhost:3010";

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
        println!("agent: {} v{}", card.name, card.version);
    }

    // Manifest input shape: {"user": {"name": ...}, "style": ...}
    // Sent as a single JSON-text part — the agent's `extract_params`
    // parses the text body as JSON before invoking the `greet` skill.
    let payload = json!({
        "user": { "name": "Ada" },
        "style": "formal",
    });
    let payload_text = payload.to_string();
    println!("--- SendMessage request ---");
    println!("text={payload_text}");

    let request = MessageBuilder::new().text(payload_text);
    let response = client.send_message(request).await?;

    println!("--- SendMessage response ---");
    match response {
        SendResponse::Task(task) => {
            let state = task
                .status()
                .and_then(|s| s.state().ok())
                .map(|s| format!("{s:?}"))
                .unwrap_or_else(|| "<unknown>".to_string());
            println!(
                "kind=Task id={} state={} artifacts={}",
                task.id(),
                state,
                task.artifacts().len()
            );
            let text = artifact_text(&task);
            if !text.is_empty() {
                println!("artifact_text={text}");
            }
        }
        SendResponse::Message(msg) => {
            println!("kind=Message id={}", msg.message_id());
            for (i, t) in msg.text_parts().iter().enumerate() {
                println!("  part[{i}].text={t}");
            }
        }
        other => println!("kind=<unrecognized variant {other:?}>"),
    }

    Ok(())
}
