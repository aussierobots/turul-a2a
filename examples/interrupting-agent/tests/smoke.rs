//! End-to-end smoke test for `interrupting-agent`.
//!
//! Drives the A2A "Life of a Task" interrupted-state flow:
//!
//! 1. Turn 1 — fresh request. Executor pauses the task in
//!    `INPUT_REQUIRED` with a question message. Task is non-terminal.
//! 2. Turn 2 — client sends a message carrying the **same** `taskId`
//!    and `contextId` with the answer. Executor reads the answer,
//!    completes the task, emits the booking artifact.
//!
//! Spec invariants asserted (and printed for visibility):
//!
//! - `task.id` is **stable** across both turns — continuation reuses
//!   the same task; it is not a new task.
//! - `task.contextId` is also stable.
//! - State transitions: turn 1 → `INPUT_REQUIRED`, turn 2 → `COMPLETED`.

use std::net::TcpListener;
use std::time::Duration;

use interrupting_agent::FlightBookingExecutor;
use turul_a2a::A2aServer;
use turul_a2a_client::{A2aClient, MessageBuilder};
use turul_a2a_types::TaskState;

fn reserve_port() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.set_nonblocking(true).expect("set_nonblocking");
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

#[tokio::test]
async fn input_required_then_continuation_with_same_task_id() {
    let (std_listener, port) = reserve_port();
    let tokio_listener =
        tokio::net::TcpListener::from_std(std_listener).expect("std -> tokio listener");
    let server = A2aServer::builder()
        .executor(FlightBookingExecutor)
        .bind(([127, 0, 0, 1], port))
        .build()
        .expect("build server");
    let router = server.into_router();
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(tokio_listener, router).await;
    });

    let client = A2aClient::new(format!("http://127.0.0.1:{port}"));

    eprintln!();
    eprintln!("=== TURN 1 — book a flight, expect INPUT_REQUIRED ===");
    let req1 = MessageBuilder::new()
        .text("I want to book a flight")
        .build();
    let task1 = tokio::time::timeout(Duration::from_secs(5), client.send_message(req1))
        .await
        .expect("turn1 in time")
        .expect("turn1 ok")
        .into_task()
        .expect("turn1 returned a Task");
    let task_id = task1.id().to_string();
    let context_id = task1.context_id().to_string();
    let state1 = task1.status().and_then(|s| s.state().ok());
    let question_text = task1
        .as_proto()
        .status
        .as_ref()
        .and_then(|s| s.message.as_ref())
        .and_then(|m| m.parts.first())
        .and_then(|p| match p.content.as_ref() {
            Some(turul_a2a_proto::part::Content::Text(t)) => Some(t.clone()),
            _ => None,
        })
        .unwrap_or_default();
    eprintln!("  task_id   = {task_id}");
    eprintln!("  contextId = {context_id}");
    eprintln!("  status    = {state1:?}");
    eprintln!("  question  = {question_text:?}");
    assert_eq!(
        state1,
        Some(TaskState::InputRequired),
        "turn1 must yield INPUT_REQUIRED"
    );
    assert!(
        task1.artifacts().is_empty(),
        "no artifact yet — booking deferred"
    );

    eprintln!();
    eprintln!("=== TURN 2 — answer, REUSING the task_id ===");
    let req2 = MessageBuilder::new()
        .text("Helsinki")
        .task_id(&task_id)
        .context_id(&context_id)
        .build();
    let task2 = tokio::time::timeout(Duration::from_secs(5), client.send_message(req2))
        .await
        .expect("turn2 in time")
        .expect("turn2 ok")
        .into_task()
        .expect("turn2 returned a Task");
    let task2_id = task2.id().to_string();
    let task2_ctx = task2.context_id().to_string();
    let state2 = task2.status().and_then(|s| s.state().ok());
    let artifacts2 = task2.artifacts();
    eprintln!("  task_id   = {task2_id} (must equal turn1's task_id)");
    eprintln!("  contextId = {task2_ctx}");
    eprintln!("  status    = {state2:?}");
    eprintln!(
        "  artifact  = name={:?} artifactId={:?}",
        artifacts2
            .first()
            .map(|a| a.name.clone())
            .unwrap_or_default(),
        artifacts2
            .first()
            .map(|a| a.artifact_id.clone())
            .unwrap_or_default()
    );

    eprintln!();
    eprintln!("=== task_id stability across turns ===");
    eprintln!("  contextId    : {context_id}  ({task2_ctx})");
    eprintln!("  turn1 task_id: {task_id}");
    eprintln!("  turn2 task_id: {task2_id}");

    // Spec invariants
    assert_eq!(task_id, task2_id, "continuation must reuse the SAME taskId");
    assert_eq!(context_id, task2_ctx, "contextId stable");
    assert_eq!(
        state2,
        Some(TaskState::Completed),
        "turn2 must complete the task"
    );
    assert_eq!(artifacts2.len(), 1);
    let body = match artifacts2[0].parts[0].content.as_ref() {
        Some(turul_a2a_proto::part::Content::Text(s)) => s.clone(),
        _ => panic!("expected text artifact"),
    };
    assert!(
        body.contains("Helsinki"),
        "booking artifact must contain the answer; got: {body}"
    );
    assert!(
        body.contains("book a flight"),
        "booking artifact should also contain the original request from history; got: {body}"
    );

    server_handle.abort();
}
