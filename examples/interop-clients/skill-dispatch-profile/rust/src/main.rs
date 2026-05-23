//! Rust interop client for `skill-dispatch-profile-agent`.
//!
//! Activates the skill-invocation dispatcher profile by sending the
//! `A2A-Extensions: https://turul.dev/a2a/extensions/skill-invocation/v1`
//! HTTP header and stamping the target skill on `Message.metadata`:
//!
//!   - `a2a.skillId`     → string, the target `AgentSkill.id`
//!   - `a2a.skillParams` → object, the structured input for the skill
//!
//! Runs the two-call sequence (`echo_loud`, then `reverse`) and prints
//! each artifact body so the round-trip matches the Python/Go siblings.

use std::collections::HashMap;
use std::process::ExitCode;

use serde_json::{Value, json};
use turul_a2a_client::prelude::*;

const DEFAULT_AGENT_BASE_URL: &str = "http://localhost:3015";

/// Wire URI for the skill-invocation dispatcher profile. The agent
/// advertises this in `AgentCard.capabilities.extensions[]`; the client
/// activates it by including the URI in the `A2A-Extensions` header.
const SKILL_INVOCATION_PROFILE_V1: &str = "https://turul.dev/a2a/extensions/skill-invocation/v1";

/// Reserved metadata key holding the target `AgentSkill.id`.
const META_SKILL_ID: &str = "a2a.skillId";
/// Reserved metadata key holding the structured parameter object.
const META_SKILL_PARAMS: &str = "a2a.skillParams";

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
    println!("profile: {SKILL_INVOCATION_PROFILE_V1}");

    // `discover` fetches /.well-known/agent-card.json. We then re-create
    // the client with the extensions header activated so every subsequent
    // request advertises the profile.
    let discovered = A2aClient::discover(&base_url).await?;
    if let Some(card) = discovered.agent_card() {
        println!("agent: {} v{}", card.name, card.version);
        let extension_uris: Vec<&str> = card
            .capabilities
            .as_ref()
            .map(|c| c.extensions.iter().map(|e| e.uri.as_str()).collect())
            .unwrap_or_default();
        println!("advertised extensions: {extension_uris:?}");
    }

    let client = A2aClient::new(&base_url).with_extensions([SKILL_INVOCATION_PROFILE_V1]);

    // Call 1: echo_loud → {"shouted": "HELLO"}.
    invoke_skill(
        &client,
        "echo_loud",
        json!({ "text": "hello" }),
        "{\"shouted\":\"HELLO\"}",
    )
    .await?;

    // Call 2: reverse → {"reversed": "cba"}.
    invoke_skill(
        &client,
        "reverse",
        json!({ "text": "abc" }),
        "{\"reversed\":\"cba\"}",
    )
    .await?;

    Ok(())
}

/// Send one dispatch request and print the artifact body. `expected`
/// is the JSON the manifest-backed skill should return; we compare the
/// observed artifact text against it as a self-check (non-fatal).
async fn invoke_skill(
    client: &A2aClient,
    skill_id: &str,
    params: Value,
    expected: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    println!();
    println!("--- dispatch request ---");
    println!("skill_id={skill_id}");
    println!("params={params}");

    let mut metadata: HashMap<String, Value> = HashMap::new();
    metadata.insert(META_SKILL_ID.into(), Value::String(skill_id.into()));
    metadata.insert(META_SKILL_PARAMS.into(), params);

    let request = MessageBuilder::new()
        .text(format!("dispatch:{skill_id}"))
        .metadata_json(metadata);

    let response = client.send_message(request).await?;

    println!("--- dispatch response ---");
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
                if text == expected {
                    println!("match=ok");
                } else {
                    println!("match=mismatch (expected={expected})");
                }
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
