//! Rust interop client for `post-task-hook-agent`.
//!
//! Sends three `count 3` calls (each squares the input) followed by a
//! `metrics` call. The metrics artifact reports the in-process counter
//! that the agent's TerminalHook incremented for every prior call.
//!
//! Run:
//!   # terminal 1
//!   cargo run -p post-task-hook-agent
//!   # terminal 2
//!   cargo run -p interop-post-task-hook-rust
use std::process::ExitCode;

use turul_a2a_client::prelude::*;

const DEFAULT_AGENT_BASE_URL: &str = "http://localhost:3014";

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

    // 1. Three `count` calls. Each squares the input and trips the hook
    //    once on the success path.
    for prompt in ["count 3", "count 3", "count 3"] {
        println!("--- SendMessage request ---");
        println!("text=\"{prompt}\"");
        let response = client
            .send_message(MessageBuilder::new().text(prompt))
            .await?;
        print_response(response);
    }

    // 2. `metrics` reads the hook-recorded counter snapshot.
    println!("--- SendMessage request ---");
    println!("text=\"metrics\"");
    let response = client
        .send_message(MessageBuilder::new().text("metrics"))
        .await?;
    print_response(response);

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
