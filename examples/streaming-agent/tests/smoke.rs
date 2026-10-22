//! End-to-end smoke test for the streaming executor.
//!
//! Spawns the A2A server on a random port, drives it through
//! `turul-a2a-client::send_streaming_message`, and asserts the
//! chunked-artifact + terminal invariants that the example exists to
//! teach:
//!
//! 1. The stream opens with the `Task` snapshot (A2A §3.1.2).
//! 2. The artifact arrives in chunks: the first starts the artifact
//!    (`append=false`), every later one appends to it (`append=true`,
//!    a2a.proto `TaskArtifactUpdateEvent.append`), and only the final
//!    one carries `last_chunk=true`.
//! 3. The concatenated text payloads equal the executor's canned
//!    suffix (prefixed with an echo segment for the test input).
//! 4. `TASK_STATE_COMPLETED` is the only status update and the last
//!    event, and the server closes the stream after it.

use std::net::TcpListener;
use std::time::Duration;

use futures::StreamExt;
use streaming_agent::{ARTIFACT_ID, CANNED_SUFFIX, StreamingExecutor};
use turul_a2a::A2aServer;
use turul_a2a_client::{A2aClient, MessageBuilder, StreamEvent};

fn reserve_port() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.set_nonblocking(true).expect("set_nonblocking");
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

#[tokio::test]
async fn streaming_agent_emits_chunks_then_completes() {
    let (std_listener, port) = reserve_port();
    let tokio_listener =
        tokio::net::TcpListener::from_std(std_listener).expect("std -> tokio listener");

    let server = A2aServer::builder()
        .executor(StreamingExecutor { delay_ms: 0 })
        .bind(([127, 0, 0, 1], port))
        .build()
        .expect("build server");
    let router = server.into_router();

    // Keep the JoinHandle so we can abort at test end and avoid
    // leaking a background task into the test harness.
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(tokio_listener, router).await;
    });

    let client = A2aClient::new(format!("http://127.0.0.1:{port}"));

    let request = MessageBuilder::new().text("hi").build();
    let stream_future = client.send_streaming_message(request);
    let mut stream = tokio::time::timeout(Duration::from_secs(5), stream_future)
        .await
        .expect("connection opens promptly")
        .expect("send_streaming_message ok");

    // Read until the server ends the stream; a stream still open when
    // the wait elapses fails the test.
    let mut events = Vec::new();
    while let Some(next) = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the server closes the stream after the terminal event")
    {
        events.push(next.expect("parse stream event").event);
    }

    // Invariant 1: the Task snapshot opens the stream.
    assert!(
        matches!(events.first(), Some(StreamEvent::Task(_))),
        "the first event must be the Task snapshot, got {:?}",
        events.first()
    );

    // Invariant 2: append=false on the first chunk only, last_chunk on
    // the final chunk only.
    let chunks: Vec<(bool, bool, &serde_json::Value)> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ArtifactUpdate {
                append,
                last_chunk,
                artifact,
                ..
            } => Some((*append, *last_chunk, artifact)),
            _ => None,
        })
        .collect();
    assert!(chunks.len() > 1, "expected several chunks, got {chunks:?}");
    let last = chunks.len() - 1;
    for (i, (append, last_chunk, artifact)) in chunks.iter().enumerate() {
        assert_eq!(artifact["artifactId"], ARTIFACT_ID, "chunk {i}");
        assert_eq!(*append, i > 0, "chunk {i}: only later chunks append");
        assert_eq!(
            *last_chunk,
            i == last,
            "chunk {i}: only the final chunk is last"
        );
    }

    // Invariant 3: concatenated text reassembles the echo and the
    // canned suffix.
    let assembled: String = chunks
        .iter()
        .flat_map(|(_, _, artifact)| artifact["parts"].as_array().cloned().unwrap_or_default())
        .filter_map(|p| p["text"].as_str().map(str::to_string))
        .collect();
    assert_eq!(assembled, format!("[echo: hi] {CANNED_SUFFIX}"));

    // Invariant 4: COMPLETED is the only status update after the
    // snapshot, and the last event.
    let status_states: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::StatusUpdate { status, .. } => status["state"].as_str(),
            _ => None,
        })
        .collect();
    assert_eq!(status_states, ["TASK_STATE_COMPLETED"]);
    assert!(
        matches!(events.last(), Some(StreamEvent::StatusUpdate { .. })),
        "the terminal status update is the last event"
    );

    // Clean up the spawned server task so the test harness doesn't
    // leak a listening socket into subsequent tests.
    server_handle.abort();
    let _ = server_handle.await;
}
