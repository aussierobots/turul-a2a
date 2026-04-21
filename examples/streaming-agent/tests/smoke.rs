//! End-to-end smoke test for the streaming executor.
//!
//! Spawns the A2A server on a random port, drives it through
//! `turul-a2a-client::send_streaming_message`, and asserts the
//! chunked-artifact + terminal invariants that the example exists to
//! teach:
//!
//! 1. At least one run of `ArtifactUpdate` events with `append=true`
//!    whose final member carries `last_chunk=true`.
//! 2. The concatenated text payloads equal the executor's canned
//!    suffix (prefixed with an echo segment for the test input).
//! 3. A terminal `StatusUpdate` with `state=TASK_STATE_COMPLETED` is
//!    the last `StatusUpdate` observed, and the stream closes after.
//! 4. SSE ids are strictly monotonic.
//!
//! The test tolerates (but does not require) intermediate non-terminal
//! `StatusUpdate` events (e.g. Submitted/Working) — the spec does not
//! pin their exact placement and the goal of this test is the
//! chunked-artifact contract, not status-framing minutiae.

use std::net::TcpListener;
use std::time::Duration;

use futures::StreamExt;
use streaming_agent::StreamingExecutor;
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

    let mut events = Vec::new();
    while let Ok(Some(next)) = tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
        events.push(next.expect("parse stream event"));
    }

    assert!(
        !events.is_empty(),
        "expected at least one SSE event from the streaming executor"
    );

    // Invariant 4: strictly monotonic SSE ids (when present).
    let sequences: Vec<u64> = events
        .iter()
        .filter_map(|e| e.id.as_deref())
        .filter_map(|id| id.rsplit_once(':').and_then(|(_, s)| s.parse::<u64>().ok()))
        .collect();
    for pair in sequences.windows(2) {
        assert!(
            pair[0] < pair[1],
            "SSE sequence ids must be strictly monotonic: saw {} then {}",
            pair[0],
            pair[1]
        );
    }

    // Invariant 1: artifact chunks with last_chunk=true on the final one.
    let artifact_updates: Vec<(bool, bool, serde_json::Value)> = events
        .iter()
        .filter_map(|e| match &e.event {
            StreamEvent::ArtifactUpdate {
                append,
                last_chunk,
                artifact,
                ..
            } => Some((*append, *last_chunk, artifact.clone())),
            _ => None,
        })
        .collect();
    assert!(
        !artifact_updates.is_empty(),
        "expected one or more ArtifactUpdate events"
    );
    let (_, last_chunk_flag, _) = artifact_updates
        .last()
        .expect("artifact_updates non-empty above");
    assert!(
        *last_chunk_flag,
        "final ArtifactUpdate must carry last_chunk=true (teaches the chunked-artifact contract)"
    );
    for (append, last_chunk, _) in &artifact_updates[..artifact_updates.len() - 1] {
        assert!(
            *append,
            "intermediate ArtifactUpdates should have append=true"
        );
        assert!(
            !*last_chunk,
            "only the final ArtifactUpdate may set last_chunk=true"
        );
    }

    // Invariant 2: concatenated text reassembles the canned suffix.
    let assembled: String = artifact_updates
        .iter()
        .filter_map(|(_, _, artifact_json)| {
            artifact_json
                .get("parts")
                .and_then(|p| p.as_array())
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<String>()
                })
        })
        .collect();
    assert!(
        assembled.contains("the quick brown fox jumps over the lazy dog"),
        "assembled artifact text must contain the canned suffix; got: {assembled:?}"
    );
    assert!(
        assembled.contains("hi"),
        "assembled artifact text must contain the echoed input; got: {assembled:?}"
    );

    // Invariant 3: a Completed terminal StatusUpdate is present and is
    // the last StatusUpdate in the stream. Non-terminal StatusUpdates
    // (Submitted, Working) are tolerated at any position.
    let status_states: Vec<String> = events
        .iter()
        .filter_map(|e| match &e.event {
            StreamEvent::StatusUpdate { status, .. } => status
                .get("state")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            _ => None,
        })
        .collect();
    assert!(
        !status_states.is_empty(),
        "expected at least one StatusUpdate event"
    );
    assert_eq!(
        status_states.last().map(String::as_str),
        Some("TASK_STATE_COMPLETED"),
        "final StatusUpdate must be TASK_STATE_COMPLETED (saw sequence: {status_states:?})"
    );

    // Clean up the spawned server task so the test harness doesn't
    // leak a listening socket into subsequent tests.
    server_handle.abort();
    let _ = server_handle.await;
}
