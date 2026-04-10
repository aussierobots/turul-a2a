//! SSE streaming infrastructure for A2A task events.
//!
//! Provides multi-subscriber fan-out for task status and artifact updates.
//! Used by `POST /message:stream` and `GET /tasks/{id}:subscribe`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, RwLock};
use turul_a2a_types::{Artifact, TaskStatus};

/// A single streaming event.
#[derive(Debug, Clone, serde::Serialize)]
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct StatusUpdatePayload {
    #[serde(rename = "taskId")]
    pub task_id: String,
    #[serde(rename = "contextId")]
    pub context_id: String,
    pub status: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize)]
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

/// Manages broadcast channels for streaming task events.
///
/// Each task gets its own broadcast channel. Multiple subscribers
/// can listen to the same task's events.
#[derive(Clone)]
pub struct TaskEventBroker {
    channels: Arc<RwLock<HashMap<String, broadcast::Sender<StreamEvent>>>>,
}

impl TaskEventBroker {
    pub fn new() -> Self {
        Self {
            channels: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get or create a broadcast sender for a task.
    pub async fn get_or_create_sender(
        &self,
        task_id: &str,
    ) -> broadcast::Sender<StreamEvent> {
        let mut channels = self.channels.write().await;
        channels
            .entry(task_id.to_string())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .clone()
    }

    /// Subscribe to events for a task. Returns a receiver.
    pub async fn subscribe(
        &self,
        task_id: &str,
    ) -> broadcast::Receiver<StreamEvent> {
        let sender = self.get_or_create_sender(task_id).await;
        sender.subscribe()
    }

    /// Publish a status update event.
    pub async fn publish_status_update(
        &self,
        task_id: &str,
        context_id: &str,
        status: &TaskStatus,
    ) {
        let sender = self.get_or_create_sender(task_id).await;
        let event = StreamEvent::StatusUpdate {
            status_update: StatusUpdatePayload {
                task_id: task_id.to_string(),
                context_id: context_id.to_string(),
                status: serde_json::to_value(status).unwrap_or_default(),
            },
        };
        // Ignore send errors (no subscribers is fine)
        let _ = sender.send(event);
    }

    /// Publish an artifact update event.
    pub async fn publish_artifact_update(
        &self,
        task_id: &str,
        context_id: &str,
        artifact: &Artifact,
        append: bool,
        last_chunk: bool,
    ) {
        let sender = self.get_or_create_sender(task_id).await;
        let event = StreamEvent::ArtifactUpdate {
            artifact_update: ArtifactUpdatePayload {
                task_id: task_id.to_string(),
                context_id: context_id.to_string(),
                artifact: serde_json::to_value(artifact).unwrap_or_default(),
                append,
                last_chunk,
            },
        };
        let _ = sender.send(event);
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
    use turul_a2a_types::{Part, TaskState};

    // =========================================================
    // Streaming event tests — contract before implementation
    // =========================================================

    #[tokio::test]
    async fn subscribe_receives_published_status_update() {
        let broker = TaskEventBroker::new();
        let mut rx = broker.subscribe("task-1").await;

        let status = TaskStatus::new(TaskState::Working);
        broker.publish_status_update("task-1", "ctx-1", &status).await;

        let event = rx.recv().await.unwrap();
        match event {
            StreamEvent::StatusUpdate { status_update } => {
                assert_eq!(status_update.task_id, "task-1");
                assert_eq!(status_update.context_id, "ctx-1");
            }
            _ => panic!("Expected StatusUpdate"),
        }
    }

    #[tokio::test]
    async fn subscribe_receives_published_artifact_update() {
        let broker = TaskEventBroker::new();
        let mut rx = broker.subscribe("task-2").await;

        let artifact = Artifact::new("art-1", vec![Part::text("chunk")]);
        broker
            .publish_artifact_update("task-2", "ctx-2", &artifact, true, false)
            .await;

        let event = rx.recv().await.unwrap();
        match event {
            StreamEvent::ArtifactUpdate { artifact_update } => {
                assert_eq!(artifact_update.task_id, "task-2");
                assert!(artifact_update.append);
                assert!(!artifact_update.last_chunk);
            }
            _ => panic!("Expected ArtifactUpdate"),
        }
    }

    #[tokio::test]
    async fn events_arrive_in_order() {
        let broker = TaskEventBroker::new();
        let mut rx = broker.subscribe("task-3").await;

        // Publish 3 status updates with different states
        for state in [TaskState::Working, TaskState::InputRequired, TaskState::Completed] {
            broker
                .publish_status_update("task-3", "ctx-3", &TaskStatus::new(state))
                .await;
        }

        // Must arrive in order
        let e1 = rx.recv().await.unwrap();
        let e2 = rx.recv().await.unwrap();
        let e3 = rx.recv().await.unwrap();

        match (&e1, &e2, &e3) {
            (
                StreamEvent::StatusUpdate { status_update: s1 },
                StreamEvent::StatusUpdate { status_update: s2 },
                StreamEvent::StatusUpdate { status_update: s3 },
            ) => {
                // Verify ordering via state values
                assert!(s1.status["state"].as_str().unwrap().contains("WORKING"));
                assert!(s2.status["state"].as_str().unwrap().contains("INPUT_REQUIRED"));
                assert!(s3.status["state"].as_str().unwrap().contains("COMPLETED"));
            }
            _ => panic!("Expected 3 StatusUpdate events"),
        }
    }

    #[tokio::test]
    async fn multiple_subscribers_receive_same_events() {
        let broker = TaskEventBroker::new();
        let mut rx1 = broker.subscribe("task-4").await;
        let mut rx2 = broker.subscribe("task-4").await;

        broker
            .publish_status_update("task-4", "ctx-4", &TaskStatus::new(TaskState::Working))
            .await;

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();

        // Both should receive the same event
        match (&e1, &e2) {
            (
                StreamEvent::StatusUpdate { status_update: s1 },
                StreamEvent::StatusUpdate { status_update: s2 },
            ) => {
                assert_eq!(s1.task_id, s2.task_id);
                assert_eq!(s1.task_id, "task-4");
            }
            _ => panic!("Both subscribers should get StatusUpdate"),
        }
    }

    #[tokio::test]
    async fn publish_without_subscribers_does_not_error() {
        let broker = TaskEventBroker::new();
        // Publishing with no subscribers should not panic or error
        broker
            .publish_status_update("task-5", "ctx-5", &TaskStatus::new(TaskState::Working))
            .await;
    }

    #[tokio::test]
    async fn remove_cleans_up_channel() {
        let broker = TaskEventBroker::new();
        let _rx = broker.subscribe("task-6").await;
        assert!(broker.has_channel("task-6").await);

        broker.remove("task-6").await;
        assert!(!broker.has_channel("task-6").await);
    }

    #[tokio::test]
    async fn stream_event_serialization_matches_proto() {
        // StatusUpdate serializes with correct camelCase field names
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

        // ArtifactUpdate serializes with append/lastChunk
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
}
