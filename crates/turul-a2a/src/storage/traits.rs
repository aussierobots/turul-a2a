use async_trait::async_trait;
use turul_a2a_types::{Artifact, Message, Task, TaskStatus};

use super::error::A2aStorageError;
use super::filter::{PushConfigListPage, TaskFilter, TaskListPage};

/// Core trait for A2A task storage backends.
///
/// All public methods use wrapper types from `turul_a2a_types` — never raw proto types.
#[async_trait]
pub trait A2aTaskStorage: Send + Sync {
    fn backend_name(&self) -> &'static str;

    /// Create a new task. Server generates `id` if empty on the task.
    async fn create_task(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
    ) -> Result<Task, A2aStorageError>;

    /// Get a task by ID. Returns `None` if not found or not accessible.
    /// Enforces tenant + owner isolation.
    /// `history_length`: Some(0)=omit, None=no limit, Some(n)=last n.
    async fn get_task(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        history_length: Option<i32>,
    ) -> Result<Option<Task>, A2aStorageError>;

    /// Full replacement update of a task. Enforces tenant + owner match.
    async fn update_task(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
    ) -> Result<(), A2aStorageError>;

    /// Delete a task. Returns `true` if deleted, `false` if not found.
    async fn delete_task(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
    ) -> Result<bool, A2aStorageError>;

    /// List tasks with filtering and pagination.
    async fn list_tasks(&self, filter: TaskFilter) -> Result<TaskListPage, A2aStorageError>;

    /// Update a task's status with state machine validation.
    /// Rejects invalid transitions with `InvalidTransition` or `TerminalState`.
    async fn update_task_status(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        new_status: TaskStatus,
    ) -> Result<Task, A2aStorageError>;

    /// Append a message to a task's history. Enforces tenant + owner isolation.
    async fn append_message(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        message: Message,
    ) -> Result<(), A2aStorageError>;

    /// Append an artifact to a task. Enforces tenant + owner isolation.
    /// `append`: if true and artifact_id matches existing, append parts to it.
    /// `last_chunk`: transport-level signal for SSE streaming (Phase 3).
    ///   Storage passes it through but does not model completion state in v0.1.
    async fn append_artifact(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        artifact: Artifact,
        append: bool,
        last_chunk: bool,
    ) -> Result<(), A2aStorageError>;

    /// Total number of tasks across all tenants.
    async fn task_count(&self) -> Result<usize, A2aStorageError>;

    /// Periodic maintenance (cleanup, compaction).
    async fn maintenance(&self) -> Result<(), A2aStorageError>;
}

/// Storage trait for push notification configurations.
///
/// **Design note**: This trait uses `turul_a2a_proto::TaskPushNotificationConfig` directly
/// rather than a wrapper type. Push configs are simple CRUD resources with no state machine
/// or invariant enforcement, so a wrapper adds no safety value. If a wrapper is needed later
/// (e.g., for validation), it can be added without breaking the storage contract since the
/// proto type's serde behavior is stable via pbjson.
#[async_trait]
pub trait A2aPushNotificationStorage: Send + Sync {
    fn backend_name(&self) -> &'static str;

    /// Create a push notification config. Server generates `id` if empty.
    async fn create_config(
        &self,
        tenant: &str,
        config: turul_a2a_proto::TaskPushNotificationConfig,
    ) -> Result<turul_a2a_proto::TaskPushNotificationConfig, A2aStorageError>;

    /// Get a push notification config. Returns `None` if not found.
    async fn get_config(
        &self,
        tenant: &str,
        task_id: &str,
        config_id: &str,
    ) -> Result<Option<turul_a2a_proto::TaskPushNotificationConfig>, A2aStorageError>;

    /// List push notification configs for a task with pagination.
    async fn list_configs(
        &self,
        tenant: &str,
        task_id: &str,
        page_token: Option<&str>,
        page_size: Option<i32>,
    ) -> Result<PushConfigListPage, A2aStorageError>;

    /// Delete a push notification config. Idempotent — no error if not found.
    async fn delete_config(
        &self,
        tenant: &str,
        task_id: &str,
        config_id: &str,
    ) -> Result<(), A2aStorageError>;
}
