//! End-to-end smoke test for `conversation-agent`.
//!
//! Drives the A2A "Life of a Task" refinement flow:
//!
//! 1. Originating request — fresh `task_id` and `contextId` issued by
//!    the framework.
//! 2. Refinement request in the **same** `contextId`, with
//!    `referenceTaskIds = [task1.id]` — produces a **new** task with
//!    the same artifact name and a new `artifactId`.
//!
//! Spec invariants asserted (and printed for visibility):
//!
//! - `task1.id != task2.id` — refinement creates a new task; tasks
//!   are immutable once terminal.
//! - `task1.contextId == task2.contextId` — refinement stays in the
//!   conversation.
//! - Same artifact name across versions, new `artifactId` per task.

use std::net::TcpListener;
use std::time::Duration;

use conversation_agent::{ARTIFACT_ID_V1, ARTIFACT_ID_V2, ARTIFACT_NAME, ConversationExecutor};
use turul_a2a::A2aServer;
use turul_a2a_client::{A2aClient, MessageBuilder};

fn reserve_port() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.set_nonblocking(true).expect("set_nonblocking");
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

#[tokio::test]
async fn refinement_creates_new_task_in_same_context_with_new_artifact_id() {
    let (std_listener, port) = reserve_port();
    let tokio_listener =
        tokio::net::TcpListener::from_std(std_listener).expect("std -> tokio listener");
    let server = A2aServer::builder()
        .executor(ConversationExecutor)
        .bind(([127, 0, 0, 1], port))
        .build()
        .expect("build server");
    let router = server.into_router();
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(tokio_listener, router).await;
    });

    let client = A2aClient::new(format!("http://127.0.0.1:{port}"));

    eprintln!();
    eprintln!("=== TURN 1 — originate the task ===");
    let req1 = MessageBuilder::new()
        .text("a sailboat on the ocean")
        .build();
    let task1 = tokio::time::timeout(Duration::from_secs(5), client.send_message(req1))
        .await
        .expect("turn1 in time")
        .expect("turn1 ok")
        .into_task()
        .expect("turn1 returned a Task");
    let task1_id = task1.id().to_string();
    let context_id = task1.context_id().to_string();
    let task1_state = task1.status().and_then(|s| s.state().ok());
    eprintln!("  task_id   = {task1_id}");
    eprintln!("  contextId = {context_id}");
    eprintln!("  status    = {task1_state:?}");
    let artifacts1 = task1.artifacts();
    eprintln!(
        "  artifact  = name={:?} artifactId={:?}",
        artifacts1[0].name, artifacts1[0].artifact_id
    );
    assert_eq!(artifacts1.len(), 1, "turn1 must emit one artifact");
    assert_eq!(artifacts1[0].name, ARTIFACT_NAME);
    assert_eq!(artifacts1[0].artifact_id, ARTIFACT_ID_V1);

    eprintln!();
    eprintln!("=== TURN 2 — refinement, SAME contextId, referenceTaskIds=[task1] ===");
    let req2 = MessageBuilder::new()
        .text("make it red")
        .context_id(&context_id)
        .reference_task_ids([&task1_id])
        .build();
    let task2 = tokio::time::timeout(Duration::from_secs(5), client.send_message(req2))
        .await
        .expect("turn2 in time")
        .expect("turn2 ok")
        .into_task()
        .expect("turn2 returned a Task");
    let task2_id = task2.id().to_string();
    let task2_ctx = task2.context_id().to_string();
    let task2_state = task2.status().and_then(|s| s.state().ok());
    let artifacts2 = task2.artifacts();
    eprintln!("  task_id   = {task2_id}");
    eprintln!("  contextId = {task2_ctx}");
    eprintln!("  status    = {task2_state:?}");
    eprintln!(
        "  artifact  = name={:?} artifactId={:?}",
        artifacts2[0].name, artifacts2[0].artifact_id
    );

    eprintln!();
    eprintln!("=== task_id progression across the conversation ===");
    eprintln!("  contextId stays:    {context_id}");
    eprintln!("  task1.id (turn1):   {task1_id}");
    eprintln!("  task2.id (turn2):   {task2_id}");

    // Spec invariants
    assert_ne!(
        task1_id, task2_id,
        "refinement must create a NEW task — task immutability"
    );
    assert_eq!(
        context_id, task2_ctx,
        "refinement must stay in the same contextId"
    );
    assert_eq!(artifacts2.len(), 1);
    assert_eq!(artifacts2[0].name, ARTIFACT_NAME, "artifact name reused");
    assert_eq!(
        artifacts2[0].artifact_id, ARTIFACT_ID_V2,
        "artifactId incremented per refinement"
    );
    assert_ne!(
        artifacts1[0].artifact_id, artifacts2[0].artifact_id,
        "artifactIds differ between originating and refined tasks"
    );

    // The executor should have echoed task1.id into the v2 artifact
    // body — proves the executor saw `referenceTaskIds`.
    let v2_body = match artifacts2[0].parts[0].content.as_ref() {
        Some(turul_a2a_proto::part::Content::Text(s)) => s.clone(),
        _ => panic!("expected text part on v2 artifact"),
    };
    assert!(
        v2_body.contains(&task1_id),
        "v2 artifact body must reference task1.id; got: {v2_body}"
    );

    server_handle.abort();
}
