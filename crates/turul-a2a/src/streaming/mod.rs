//! SSE streaming infrastructure for A2A task events.
//!
//! The durable event store (`A2aEventStore`) is the source of truth for events.
//! The `TaskEventBroker` is a wake-up signal only — it notifies same-instance
//! subscribers that new data is available in the store. Subscribers always
//! re-read from the store; the broker carries no event data or sequences.
//!
//! Used by `POST /message:stream` and `GET /tasks/{id}:subscribe`.

pub mod replay;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, RwLock};

/// A single streaming event (persisted in the durable event store).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum StreamEvent {
    /// Task status changed.
    StatusUpdate {
        #[serde(rename = "statusUpdate")]
        status_update: StatusUpdatePayload,
    },
    /// Artifact produced or updated.
    ArtifactUpdate {
        #[serde(rename = "artifactUpdate")]
        artifact_update: ArtifactUpdatePayload,
    },
}

impl StreamEvent {
    /// Check if this event represents a terminal task state.
    pub fn is_terminal(&self) -> bool {
        match self {
            StreamEvent::StatusUpdate { status_update } => {
                status_update.status.get("state")
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| matches!(s,
                        "TASK_STATE_COMPLETED" | "TASK_STATE_FAILED" |
                        "TASK_STATE_CANCELED" | "TASK_STATE_REJECTED"
                    ))
            }
            StreamEvent::ArtifactUpdate { .. } => false,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StatusUpdatePayload {
    #[serde(rename = "taskId")]
    pub task_id: String,
    #[serde(rename = "contextId")]
    pub context_id: String,
    pub status: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArtifactUpdatePayload {
    #[serde(rename = "taskId")]
    pub task_id: String,
    #[serde(rename = "contextId")]
    pub context_id: String,
    pub artifact: serde_json::Value,
    pub append: bool,
    #[serde(rename = "lastChunk")]
    pub last_chunk: bool,
}

/// Channel capacity per task.
const CHANNEL_CAPACITY: usize = 64;

/// Wake-up signal broker for streaming subscribers.
///
/// Each task gets its own broadcast channel carrying `()` signals.
/// The broker does NOT carry event data or sequence numbers — it only
/// tells subscribers "new data is available in the store, re-read now."
///
/// This is a same-instance latency optimization. Cross-instance subscribers
/// fall back to periodic store polling.
#[derive(Clone)]
pub struct TaskEventBroker {
    channels: Arc<RwLock<HashMap<String, broadcast::Sender<()>>>>,
}

impl TaskEventBroker {
    pub fn new() -> Self {
        Self {
            channels: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get or create a wake-up sender for a task.
    async fn get_or_create_sender(
        &self,
        task_id: &str,
    ) -> broadcast::Sender<()> {
        let mut channels = self.channels.write().await;
        channels
            .entry(task_id.to_string())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .clone()
    }

    /// Subscribe to wake-up notifications for a task.
    /// Returns a receiver that yields `()` when new events are available in the store.
    pub async fn subscribe(
        &self,
        task_id: &str,
    ) -> broadcast::Receiver<()> {
        let sender = self.get_or_create_sender(task_id).await;
        sender.subscribe()
    }

    /// Notify subscribers that new event data is available in the store for this task.
    /// Fire-and-forget — no error if there are no subscribers.
    pub async fn notify(&self, task_id: &str) {
        let sender = self.get_or_create_sender(task_id).await;
        let _ = sender.send(());
    }

    /// Remove a task's channel (cleanup after terminal state).
    pub async fn remove(&self, task_id: &str) {
        self.channels.write().await.remove(task_id);
    }

    /// Check if a task has an active channel.
    pub async fn has_channel(&self, task_id: &str) -> bool {
        self.channels.read().await.contains_key(task_id)
    }
}

impl Default for TaskEventBroker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================
    // Broker tests — wake-up signal semantics
    // =========================================================

    #[tokio::test]
    async fn subscriber_receives_notification() {
        let broker = TaskEventBroker::new();
        let mut rx = broker.subscribe("task-1").await;

        broker.notify("task-1").await;

        let result = rx.recv().await;
        assert!(result.is_ok(), "Subscriber should receive () notification");
    }

    #[tokio::test]
    async fn multiple_notifications_arrive_in_order() {
        let broker = TaskEventBroker::new();
        let mut rx = broker.subscribe("task-2").await;

        broker.notify("task-2").await;
        broker.notify("task-2").await;
        broker.notify("task-2").await;

        // All three should arrive
        assert!(rx.recv().await.is_ok());
        assert!(rx.recv().await.is_ok());
        assert!(rx.recv().await.is_ok());
    }

    #[tokio::test]
    async fn multiple_subscribers_all_notified() {
        let broker = TaskEventBroker::new();
        let mut rx1 = broker.subscribe("task-3").await;
        let mut rx2 = broker.subscribe("task-3").await;

        broker.notify("task-3").await;

        assert!(rx1.recv().await.is_ok(), "Subscriber 1 should be notified");
        assert!(rx2.recv().await.is_ok(), "Subscriber 2 should be notified");
    }

    #[tokio::test]
    async fn notify_without_subscribers_does_not_error() {
        let broker = TaskEventBroker::new();
        // Should not panic or error
        broker.notify("task-4").await;
    }

    #[tokio::test]
    async fn remove_cleans_up_channel() {
        let broker = TaskEventBroker::new();
        let _rx = broker.subscribe("task-5").await;
        assert!(broker.has_channel("task-5").await);

        broker.remove("task-5").await;
        assert!(!broker.has_channel("task-5").await);
    }

    #[tokio::test]
    async fn cross_task_isolation() {
        let broker = TaskEventBroker::new();
        let mut rx_a = broker.subscribe("task-a").await;
        let mut rx_b = broker.subscribe("task-b").await;

        // Notify only task-a
        broker.notify("task-a").await;

        assert!(rx_a.recv().await.is_ok(), "task-a subscriber should be notified");

        // task-b should have nothing — use try_recv to avoid blocking
        // (broadcast recv would block, so we check via a timeout)
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            rx_b.recv(),
        ).await;
        assert!(result.is_err(), "task-b should NOT be notified");
    }

    // =========================================================
    // StreamEvent serialization tests (event type, not broker)
    // =========================================================

    #[test]
    fn stream_event_serialization_matches_proto() {
        let event = StreamEvent::StatusUpdate {
            status_update: StatusUpdatePayload {
                task_id: "t-1".to_string(),
                context_id: "c-1".to_string(),
                status: serde_json::json!({"state": "TASK_STATE_WORKING"}),
            },
        };
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("statusUpdate").is_some());
        assert_eq!(json["statusUpdate"]["taskId"], "t-1");
        assert_eq!(json["statusUpdate"]["contextId"], "c-1");

        let event = StreamEvent::ArtifactUpdate {
            artifact_update: ArtifactUpdatePayload {
                task_id: "t-2".to_string(),
                context_id: "c-2".to_string(),
                artifact: serde_json::json!({"artifactId": "a-1"}),
                append: true,
                last_chunk: true,
            },
        };
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("artifactUpdate").is_some());
        assert_eq!(json["artifactUpdate"]["append"], true);
        assert_eq!(json["artifactUpdate"]["lastChunk"], true);
    }

    #[test]
    fn is_terminal_detects_terminal_states() {
        for state in ["TASK_STATE_COMPLETED", "TASK_STATE_FAILED", "TASK_STATE_CANCELED", "TASK_STATE_REJECTED"] {
            let event = StreamEvent::StatusUpdate {
                status_update: StatusUpdatePayload {
                    task_id: "t".into(), context_id: "c".into(),
                    status: serde_json::json!({"state": state}),
                },
            };
            assert!(event.is_terminal(), "{state} should be terminal");
        }
    }

    #[test]
    fn is_terminal_rejects_non_terminal_states() {
        for state in ["TASK_STATE_SUBMITTED", "TASK_STATE_WORKING", "TASK_STATE_INPUT_REQUIRED"] {
            let event = StreamEvent::StatusUpdate {
                status_update: StatusUpdatePayload {
                    task_id: "t".into(), context_id: "c".into(),
                    status: serde_json::json!({"state": state}),
                },
            };
            assert!(!event.is_terminal(), "{state} should NOT be terminal");
        }
    }

    #[test]
    fn artifact_update_is_never_terminal() {
        let event = StreamEvent::ArtifactUpdate {
            artifact_update: ArtifactUpdatePayload {
                task_id: "t".into(), context_id: "c".into(),
                artifact: serde_json::json!({}),
                append: false, last_chunk: true,
            },
        };
        assert!(!event.is_terminal());
    }
}
