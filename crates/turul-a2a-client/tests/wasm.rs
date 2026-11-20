//! The client compiled to wasm32-unknown-unknown, run under Node against a
//! live A2A server: card discovery, a blocking send, and an SSE stream.
//!
//! The server URL is read at compile time from `TURUL_WASM_E2E_URL`, since a
//! wasm test has no process environment. The fixture is
//! `cargo run -p post-task-hook-agent`, whose `count <n>` skill answers
//! `{"squared": n*n}`.
//!
//! ```sh
//! TURUL_WASM_E2E_URL=http://127.0.0.1:3014 \
//! CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
//! cargo test -p turul-a2a-client --target wasm32-unknown-unknown --test wasm
//! ```
#![cfg(target_arch = "wasm32")]

use futures::StreamExt;
use turul_a2a_client::prelude::*;
use turul_a2a_types::TaskState;
use wasm_bindgen_test::wasm_bindgen_test;

// An unset URL must fail the test, not skip it.
#[allow(clippy::option_env_unwrap)]
fn server_url() -> &'static str {
    option_env!("TURUL_WASM_E2E_URL")
        .expect("set TURUL_WASM_E2E_URL to a running post-task-hook-agent")
}

#[wasm_bindgen_test]
async fn discovers_the_agent_card() {
    let client = A2aClient::discover(server_url()).await.expect("discover");
    let card = client.agent_card().expect("card cached by discover");
    assert!(
        card.capabilities
            .as_ref()
            .is_some_and(|c| c.streaming == Some(true))
    );
}

#[wasm_bindgen_test]
async fn blocking_send_returns_the_completed_task() {
    let client = A2aClient::new(server_url());
    let reply = client
        .send_message(MessageBuilder::new().text("count 3"))
        .await
        .expect("send");
    let task = reply.task().expect("task reply");
    assert_eq!(
        task.status().and_then(|s| s.state().ok()),
        Some(TaskState::Completed)
    );
    assert_eq!(artifact_text(task), r#"{"squared":9}"#);
}

#[wasm_bindgen_test]
async fn streaming_send_ends_in_a_completed_status() {
    let client = A2aClient::new(server_url());
    let mut stream = client
        .send_streaming_message(MessageBuilder::new().text("count 4"))
        .await
        .expect("stream");

    let mut first_is_task = None;
    let mut artifact = String::new();
    let mut last_state = None;
    while let Some(item) = stream.next().await {
        let event = item.expect("stream item").event;
        first_is_task.get_or_insert(matches!(event, StreamEvent::Task(_)));
        match event {
            StreamEvent::ArtifactUpdate { artifact: a, .. } => {
                artifact.push_str(a["parts"][0]["text"].as_str().unwrap_or_default())
            }
            StreamEvent::StatusUpdate { status, .. } => {
                last_state = status["state"].as_str().map(str::to_owned)
            }
            _ => {}
        }
    }
    assert_eq!(
        first_is_task,
        Some(true),
        "first event is the task snapshot"
    );
    assert_eq!(artifact, r#"{"squared":16}"#);
    assert_eq!(last_state.as_deref(), Some("TASK_STATE_COMPLETED"));
}
