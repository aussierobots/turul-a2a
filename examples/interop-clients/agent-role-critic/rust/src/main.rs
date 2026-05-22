//! Rust interop client for `agent-role-critic-agent`.
//!
//! Sends two sequential JSON-shaped messages to exercise both critic skills:
//! 1. `validate_against_schema` — value=42 against `{"type":"integer"}`.
//!    Expected: `{"valid":true,"errors":[]}`.
//! 2. `check_invariants`        — value="hello world" with two invariants
//!    (`non_empty`, `contains "world"`).
//!    Expected: `{"verdict":"pass","failures":[]}`.
//!
//! Run:
//!   # terminal 1
//!   cargo run -p agent-role-critic-agent
//!   # terminal 2
//!   cargo run -p interop-agent-role-critic-rust
use std::process::ExitCode;

use serde_json::json;
use turul_a2a_client::prelude::*;

const DEFAULT_AGENT_BASE_URL: &str = "http://localhost:3013";

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

    // 1. Schema validation: integer value against integer schema.
    let validate_payload = json!({
        "kind": "validate_against_schema",
        "value": 42,
        "schema": {"type": "integer"}
    });

    // 2. Invariant check: non-empty + contains "world".
    let invariants_payload = json!({
        "kind": "check_invariants",
        "value": "hello world",
        "invariants": [
            {"name": "ne", "check": "non_empty"},
            {"name": "has_world", "check": "contains", "args": {"needle": "world"}}
        ]
    });

    for payload in [validate_payload, invariants_payload] {
        let text = payload.to_string();
        println!("--- SendMessage request ---");
        println!("text={text}");
        let response = client
            .send_message(MessageBuilder::new().text(text))
            .await?;
        print_response(response);
    }

    Ok(())
}

fn print_response(response: SendResponse) {
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
}
