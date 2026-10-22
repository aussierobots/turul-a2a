//! The one stream behind `SendStreamingMessage` and `SubscribeToTask`
//! on every transport.
//!
//! Transports only encode [`StreamItem`]s into their wire format; the
//! item order, the replay boundary and the close decision live here so
//! HTTP+SSE, JSON-RPC+SSE and gRPC cannot drift apart.
//!
//! Contract (A2A v1.0.1):
//! - a fresh stream opens with a `Task` snapshot (§3.1.2, §3.1.6),
//!   followed only by events with a sequence above the one the snapshot
//!   reflects;
//! - the stream closes after a terminal state or INPUT_REQUIRED (§11.7)
//!   and stays open across AUTH_REQUIRED, so an out-of-band credential
//!   can resume work on the same stream (§7.6.1).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;
use turul_a2a_types::{Task, TaskState};

use super::StreamEvent;
use crate::error::A2aError;
use crate::storage::A2aEventStore;

/// Cross-instance backstop poll. A same-instance broker wake-up makes
/// delivery immediate; the periodic re-read covers events committed on
/// another instance.
const STORE_POLL_INTERVAL: Duration = Duration::from_secs(2);

const CHANNEL_DEPTH: usize = 64;

/// One item of a task stream.
pub(crate) enum StreamItem {
    /// Task snapshot. `sequence` is the latest event it reflects, so a
    /// client resuming from it receives only later events.
    Task { task: Box<Task>, sequence: u64 },
    /// A persisted event with its store sequence.
    Event { sequence: u64, event: StreamEvent },
}

/// Where a task stream starts.
pub(crate) enum StreamStart {
    /// Open with `task`, then deliver events after `sequence`.
    Snapshot { task: Box<Task>, sequence: u64 },
    /// Reconnection: the client already has the snapshot; deliver
    /// events after `after_sequence`. `current_state` and
    /// `current_sequence` describe the task as it is now, so a client
    /// that is already caught up on a state that closes streams gets a
    /// stream that ends at once rather than one that idles.
    Resume {
        after_sequence: u64,
        current_state: Option<TaskState>,
        current_sequence: u64,
    },
}

pub(crate) struct TaskEventStream {
    pub(crate) task_id: String,
    pub(crate) items: ReceiverStream<Result<StreamItem, A2aError>>,
}

fn closes_stream(state: TaskState) -> bool {
    turul_a2a_types::state_machine::is_terminal(state) || state == TaskState::InputRequired
}

/// Read events for `task_id` from the durable store, starting at
/// `start`, until the task reaches a state that closes the stream or
/// the consumer goes away. The broker receiver is only a wake-up
/// signal; the store is the source of every item.
pub(crate) fn task_event_stream(
    event_store: Arc<dyn A2aEventStore>,
    tenant: String,
    task_id: String,
    start: StreamStart,
    mut wake_rx: broadcast::Receiver<()>,
) -> TaskEventStream {
    let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_DEPTH);
    let stream_task_id = task_id.clone();

    tokio::spawn(async move {
        let mut last_seq = match start {
            StreamStart::Snapshot { task, sequence } => {
                let closes = task
                    .status()
                    .and_then(|s| s.state().ok())
                    .is_some_and(closes_stream);
                if tx
                    .send(Ok(StreamItem::Task { task, sequence }))
                    .await
                    .is_err()
                    || closes
                {
                    return;
                }
                sequence
            }
            StreamStart::Resume {
                after_sequence,
                current_state,
                current_sequence,
            } => {
                if after_sequence >= current_sequence && current_state.is_some_and(closes_stream) {
                    return;
                }
                after_sequence
            }
        };

        loop {
            let events = match event_store
                .get_events_after(&tenant, &task_id, last_seq)
                .await
            {
                Ok(events) => events,
                Err(e) => {
                    let _ = tx.send(Err(A2aError::from(e))).await;
                    return;
                }
            };

            for (sequence, event) in events {
                last_seq = sequence;
                let closes = event.task_state().is_some_and(closes_stream);
                if tx
                    .send(Ok(StreamItem::Event { sequence, event }))
                    .await
                    .is_err()
                    || closes
                {
                    return;
                }
            }

            tokio::select! {
                result = wake_rx.recv() => {
                    if let Err(broadcast::error::RecvError::Closed) = result {
                        return;
                    }
                }
                _ = tokio::time::sleep(STORE_POLL_INTERVAL) => {}
            }
        }
    });

    TaskEventStream {
        task_id: stream_task_id,
        items: ReceiverStream::new(rx),
    }
}
