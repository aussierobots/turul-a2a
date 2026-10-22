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
//!   can resume work on the same stream (§7.6.1);
//! - a storage error is delivered as the last item, after which the
//!   stream ends; each transport encodes it as that binding's error.

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
    /// The task as it stands when the stream opens.
    Task(Box<Task>),
    /// A persisted event committed after the snapshot.
    Event(StreamEvent),
}

pub(crate) struct TaskEventStream {
    pub(crate) items: ReceiverStream<Result<StreamItem, A2aError>>,
}

fn closes_stream(state: TaskState) -> bool {
    turul_a2a_types::state_machine::is_terminal(state) || state == TaskState::InputRequired
}

/// Open with `snapshot`, then deliver events for `task_id` committed
/// after `sequence` (the latest event the snapshot reflects) until the
/// task reaches a state that closes the stream or the consumer goes
/// away. The broker receiver is only a wake-up signal; the store is the
/// source of every event.
pub(crate) fn task_event_stream(
    event_store: Arc<dyn A2aEventStore>,
    tenant: String,
    task_id: String,
    snapshot: Task,
    sequence: u64,
    mut wake_rx: broadcast::Receiver<()>,
) -> TaskEventStream {
    let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_DEPTH);

    tokio::spawn(async move {
        let closes = snapshot
            .status()
            .and_then(|s| s.state().ok())
            .is_some_and(closes_stream);
        if tx
            .send(Ok(StreamItem::Task(Box::new(snapshot))))
            .await
            .is_err()
            || closes
        {
            return;
        }
        let mut last_seq = sequence;

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

            for (seq, event) in events {
                last_seq = seq;
                let closes = event.task_state().is_some_and(closes_stream);
                if tx.send(Ok(StreamItem::Event(event))).await.is_err() || closes {
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
        items: ReceiverStream::new(rx),
    }
}

#[cfg(test)]
mod tests {
    use tokio_stream::StreamExt;
    use turul_a2a_types::TaskStatus;

    use super::*;
    use crate::storage::InMemoryA2aStorage;
    use crate::streaming::StatusUpdatePayload;

    /// States after which no further event can follow on the stream:
    /// the terminal states, and INPUT_REQUIRED, which ends the turn.
    const CLOSING: [TaskState; 5] = [
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Canceled,
        TaskState::Rejected,
        TaskState::InputRequired,
    ];
    /// States the task moves on from by itself; AUTH_REQUIRED resumes
    /// on the same stream once a credential arrives out of band.
    const OPEN: [TaskState; 3] = [
        TaskState::Submitted,
        TaskState::Working,
        TaskState::AuthRequired,
    ];

    const STILL_OPEN_WAIT: Duration = Duration::from_millis(300);

    #[test]
    fn terminal_states_and_input_required_close_the_stream() {
        for state in CLOSING {
            assert!(closes_stream(state), "{state:?} must close the stream");
        }
        for state in OPEN {
            assert!(!closes_stream(state), "{state:?} must keep the stream open");
        }
    }

    fn status_event(state: TaskState) -> StreamEvent {
        StreamEvent::StatusUpdate {
            status_update: StatusUpdatePayload {
                task_id: "t".into(),
                context_id: "c".into(),
                status: serde_json::to_value(TaskStatus::new(state)).unwrap(),
            },
        }
    }

    /// Open a stream over `store` with a snapshot in `state`. The
    /// returned sender keeps the wake-up channel alive, as a running
    /// server does.
    fn open(
        store: &InMemoryA2aStorage,
        state: TaskState,
    ) -> (TaskEventStream, broadcast::Sender<()>) {
        let (wake_tx, wake_rx) = broadcast::channel(4);
        let snapshot = Task::new("t", TaskStatus::new(state)).with_context_id("c");
        let stream = task_event_stream(
            Arc::new(store.clone()),
            String::new(),
            "t".into(),
            snapshot,
            0,
            wake_rx,
        );
        (stream, wake_tx)
    }

    /// The state the next item carries, or `None` once the stream ended.
    async fn next_state(stream: &mut TaskEventStream) -> Option<TaskState> {
        match tokio::time::timeout(Duration::from_secs(5), stream.items.next())
            .await
            .expect("an item or the end of the stream within the wait")?
            .expect("no storage error")
        {
            StreamItem::Task(task) => task.status().and_then(|s| s.state().ok()),
            StreamItem::Event(event) => event.task_state(),
        }
    }

    async fn assert_still_open(stream: &mut TaskEventStream, ctx: &str) {
        assert!(
            tokio::time::timeout(STILL_OPEN_WAIT, stream.items.next())
                .await
                .is_err(),
            "{ctx}: the stream must stay open"
        );
    }

    #[tokio::test]
    async fn a_snapshot_in_a_closing_state_ends_the_stream_after_it() {
        for state in CLOSING {
            let store = InMemoryA2aStorage::new();
            let (mut stream, _wake) = open(&store, state);
            assert_eq!(next_state(&mut stream).await, Some(state));
            assert!(
                next_state(&mut stream).await.is_none(),
                "{state:?}: nothing follows the snapshot"
            );
        }
    }

    #[tokio::test]
    async fn a_snapshot_in_an_open_state_keeps_the_stream_open() {
        for state in OPEN {
            let store = InMemoryA2aStorage::new();
            let (mut stream, _wake) = open(&store, state);
            assert_eq!(next_state(&mut stream).await, Some(state));
            assert_still_open(&mut stream, &format!("{state:?} snapshot")).await;
        }
    }

    #[tokio::test]
    async fn open_state_events_pass_and_a_closing_event_ends_the_stream() {
        for closing in CLOSING {
            let store = InMemoryA2aStorage::new();
            for state in [
                TaskState::AuthRequired,
                TaskState::Working,
                closing,
                TaskState::Working,
            ] {
                store
                    .append_event("", "t", status_event(state))
                    .await
                    .unwrap();
            }
            let (mut stream, _wake) = open(&store, TaskState::Submitted);
            let mut states = Vec::new();
            while let Some(state) = next_state(&mut stream).await {
                states.push(state);
            }
            assert_eq!(
                states,
                [
                    TaskState::Submitted,
                    TaskState::AuthRequired,
                    TaskState::Working,
                    closing
                ],
                "the stream ends at {closing:?}; nothing after it is delivered"
            );
        }
    }

    #[tokio::test]
    async fn an_auth_required_event_keeps_the_stream_open() {
        let store = InMemoryA2aStorage::new();
        store
            .append_event("", "t", status_event(TaskState::AuthRequired))
            .await
            .unwrap();
        let (mut stream, _wake) = open(&store, TaskState::Working);
        assert_eq!(next_state(&mut stream).await, Some(TaskState::Working));
        assert_eq!(next_state(&mut stream).await, Some(TaskState::AuthRequired));
        assert_still_open(&mut stream, "after AUTH_REQUIRED").await;
    }
}
