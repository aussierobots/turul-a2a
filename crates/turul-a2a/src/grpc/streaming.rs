//! gRPC streaming adapters for `SendStreamingMessage` and
//! `SubscribeToTask` (ADR-014 §2.3).
//!
//! Both streams are fed from the durable event store + broker wake-up
//! signal — the same single-source-of-truth path that SSE consumes
//! (ADR-009). The gRPC adapter does **not** open a parallel event
//! pipeline, subscribe to the broker for data, or assign its own event
//! sequences. Ordering and `(task_id, event_sequence)` invariants are
//! inherited verbatim.
//!
//! Each event from the store is serialized as a `pb::StreamResponse`
//! via the pbjson-generated `serde` impls, so the wire shape matches
//! what the HTTP+SSE encoder emits. `last_chunk` on
//! `TaskArtifactUpdateEvent` is read back from
//! `ArtifactUpdatePayload.last_chunk` in the stored event (ADR-014
//! §2.3 two-layer persistence) — the adapter never invents it.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;
use turul_a2a_proto as pb;
use turul_a2a_types::Task;

use crate::error::A2aError;
use crate::grpc::error::a2a_to_status;
use crate::grpc::service::BoxedStreamResponseStream;
use crate::router::{self, AppState};
use crate::storage::A2aEventStore;
use crate::streaming::replay;

/// Cross-instance backstop poll. Same value the SSE path uses
/// (`router.rs` `STORE_POLL_INTERVAL`). A local wake notification makes
/// this timer effectively zero-latency on the active instance; the
/// periodic tick covers the multi-instance case where writes land on
/// another instance's broker (ADR-009 §§3-4).
const STORE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Channel depth for the spawned producer task. Matches the SSE path.
const STREAM_CHANNEL_DEPTH: usize = 64;

/// gRPC metadata key carrying the SSE `Last-Event-ID` equivalent
/// (ADR-014 §2.3). ASCII; value is `"{task_id}:{sequence}"`.
pub const LAST_EVENT_ID_METADATA: &str = "a2a-last-event-id";

/// Build the streaming response for `SendStreamingMessage`.
///
/// Reuses [`router::setup_streaming_send`] to create the task + emit
/// SUBMITTED/WORKING + spawn the executor, then wraps the broker
/// wake-up receiver in the shared replay-then-live loop.
pub async fn handle_send_streaming_message(
    state: AppState,
    tenant: String,
    owner: String,
    body: String,
) -> Result<BoxedStreamResponseStream, Status> {
    let (task_id, wake_rx) = router::setup_streaming_send(state.clone(), &tenant, &owner, body)
        .await
        .map_err(a2a_to_status)?;
    Ok(make_store_grpc_stream(
        state.event_store,
        tenant,
        task_id,
        /* after_sequence */ 0,
        wake_rx,
        /* initial_task */ None,
    ))
}

/// Build the streaming response for `SubscribeToTask`.
///
/// Implements the spec §3.1.6 first-event-is-Task rule, terminal-task
/// rejection as `UnsupportedOperationError` (mapped to
/// `FAILED_PRECONDITION` per ADR-014 §2.5), and `a2a-last-event-id`
/// resume.
pub async fn handle_subscribe_to_task(
    state: AppState,
    tenant: String,
    owner: String,
    task_id: String,
    last_event_id_meta: Option<String>,
) -> Result<BoxedStreamResponseStream, Status> {
    // Ownership + existence check — same as core_subscribe_to_task.
    let task = state
        .task_storage
        .get_task(&tenant, &task_id, &owner, None)
        .await
        .map_err(|e| a2a_to_status(A2aError::from(e)))?
        .ok_or_else(|| {
            a2a_to_status(A2aError::TaskNotFound {
                task_id: task_id.clone(),
            })
        })?;

    // Terminal-state rejection: shared core raises
    // UnsupportedOperationError (ADR-014 §2.3); the mapping table in
    // §2.5 turns that into FAILED_PRECONDITION + ErrorInfo.
    if let Some(status) = task.status() {
        if let Ok(s) = status.state() {
            if s.is_terminal() {
                return Err(a2a_to_status(A2aError::UnsupportedOperation {
                    message: format!("Task {task_id} is already in terminal state {s:?}"),
                }));
            }
        }
    }

    let after_sequence = last_event_id_meta
        .as_deref()
        .and_then(replay::parse_last_event_id)
        .filter(|parsed| parsed.task_id == task_id)
        .map(|parsed| parsed.sequence)
        .unwrap_or(0);

    // Spec §3.1.6 + ADR-014 §2.11: the first event on a fresh attach is
    // the `Task` snapshot. On resume the client already has it — skip.
    let initial_task = if after_sequence == 0 { Some(task) } else { None };

    let wake_rx = state.event_broker.subscribe(&task_id).await;
    Ok(make_store_grpc_stream(
        state.event_store,
        tenant,
        task_id,
        after_sequence,
        wake_rx,
        initial_task,
    ))
}

/// Replay-then-live producer. The shape mirrors `make_store_sse_response`
/// in `router.rs` — only the output encoding differs (`pb::StreamResponse`
/// for gRPC vs. SSE for HTTP). All sequence + terminal invariants of
/// ADR-009 are inherited.
fn make_store_grpc_stream(
    event_store: Arc<dyn A2aEventStore>,
    tenant: String,
    task_id: String,
    after_sequence: u64,
    mut wake_rx: broadcast::Receiver<()>,
    initial_task: Option<Task>,
) -> BoxedStreamResponseStream {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<pb::StreamResponse, Status>>(
        STREAM_CHANNEL_DEPTH,
    );

    tokio::spawn(async move {
        // Spec §3.1.6: emit the Task snapshot as the first event on a
        // fresh attach. On resume (`after_sequence > 0`) the caller
        // already has the snapshot.
        if let Some(task) = initial_task {
            let response = pb::StreamResponse {
                payload: Some(pb::stream_response::Payload::Task(task.as_proto().clone())),
            };
            if tx.send(Ok(response)).await.is_err() {
                return;
            }
        }

        let mut last_seq = after_sequence;

        loop {
            let events = match event_store.get_events_after(&tenant, &task_id, last_seq).await {
                Ok(events) => events,
                Err(err) => {
                    let _ = tx
                        .send(Err(a2a_to_status(A2aError::from(err))))
                        .await;
                    return;
                }
            };

            let mut saw_terminal = false;
            for (seq, event) in events {
                last_seq = seq;
                // pbjson serde makes StreamEvent JSON compatible with
                // the `StreamResponse` oneof. last_chunk is preserved
                // because it's a field on `ArtifactUpdatePayload`,
                // which is persisted as-is in the event store.
                let value = serde_json::to_value(&event).unwrap_or_default();
                let response = match serde_json::from_value::<pb::StreamResponse>(value) {
                    Ok(r) => r,
                    Err(err) => {
                        let _ = tx
                            .send(Err(Status::internal(format!(
                                "grpc adapter: failed to encode event seq {seq}: {err}"
                            ))))
                            .await;
                        return;
                    }
                };
                if tx.send(Ok(response)).await.is_err() {
                    return; // subscriber disconnected
                }
                if event.is_terminal() {
                    saw_terminal = true;
                }
            }

            if saw_terminal {
                return; // close stream after terminal event
            }

            tokio::select! {
                result = wake_rx.recv() => {
                    match result {
                        Ok(()) => {}
                        Err(broadcast::error::RecvError::Closed) => return,
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                    }
                }
                _ = tokio::time::sleep(STORE_POLL_INTERVAL) => {}
            }
        }
    });

    Box::pin(ReceiverStream::new(rx))
}
