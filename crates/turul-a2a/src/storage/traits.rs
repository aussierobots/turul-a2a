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
    /// `last_chunk`: transport metadata carried in the SSE
    ///   `ArtifactUpdate` event. Storage does not persist it as task
    ///   state; the server layer forwards it to streaming
    ///   subscribers.
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

    /// Set the `cancel_requested` marker on a non-terminal task.
    ///
    /// Owner-scoped (unlike the supervisor-only reads on
    /// [`A2aCancellationSupervisor`]): wrong owner or missing task returns
    /// `TaskNotFound` (anti-enumeration, same pattern as other
    /// owner-scoped methods). A terminal task returns `TerminalState` —
    /// callers map to HTTP 409 `TaskNotCancelable` at the wire layer.
    ///
    /// The marker is idempotent: setting it on a task where it is already
    /// `true` is a successful no-op. Storage-internal only; the marker
    /// never appears on the wire (ADR-012 §1).
    ///
    /// Consumers: the `CancelTask` handler and direct cancel paths in
    /// cross-instance deployments. Once set, the in-flight supervisor
    /// on the instance running the executor discovers the marker via
    /// [`A2aCancellationSupervisor::supervisor_list_cancel_requested`]
    /// and trips the executor's cancellation token.
    async fn set_cancel_requested(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
    ) -> Result<(), A2aStorageError>;
}

/// Supervisor-only cancel-marker reads.
///
/// **NOT for request handlers.** Methods on this trait deliberately omit
/// the `owner` parameter. Authorization invariant: callers MUST operate
/// only on `task_id`s already present in their own
/// [`crate::server::in_flight::InFlightRegistry`], whose entries were
/// owner-validated at spawn time. Re-checking owner on every poll tick
/// would force the supervisor to carry per-task owner strings through its
/// registry without producing any security benefit — the upstream
/// validation already covered it.
///
/// External backend implementers implement this trait alongside
/// [`A2aTaskStorage`], [`super::A2aEventStore`], and
/// [`super::A2aAtomicStore`]. `AppState` carries
/// `Arc<dyn A2aCancellationSupervisor>` on a dedicated field so handler
/// code paths cannot reach the unscoped reads by construction.
///
/// # Structural trait-split invariant (compile-time enforced)
///
/// Supervisor-only reads MUST NOT be reachable via `Arc<dyn A2aTaskStorage>`.
/// The following attempts such a call and fails to compile:
///
/// ```compile_fail
/// # use std::sync::Arc;
/// # use turul_a2a::storage::A2aTaskStorage;
/// async fn leak(ts: Arc<dyn A2aTaskStorage>) {
///     let _ = ts.supervisor_get_cancel_requested("tenant", "task").await;
/// }
/// ```
///
/// Handler code holds `Arc<dyn A2aTaskStorage>` and can call the
/// owner-scoped [`A2aTaskStorage::set_cancel_requested`], but the type
/// system prevents it from calling the unscoped reads.
#[async_trait]
pub trait A2aCancellationSupervisor: Send + Sync {
    fn backend_name(&self) -> &'static str;

    /// Read the cancel-requested marker for a single task.
    ///
    /// Returns `false` if the marker is unset OR the task is already
    /// terminal (supervisor treats these identically — no token trip
    /// needed in either case). Owner is intentionally not checked; see
    /// trait-level docs for the authorization invariant.
    async fn supervisor_get_cancel_requested(
        &self,
        tenant: &str,
        task_id: &str,
    ) -> Result<bool, A2aStorageError>;

    /// Batch-read: return the subset of `task_ids` in `tenant` that have
    /// the cancel-requested marker set AND are not terminal.
    ///
    /// One database round-trip per supervisor poll tick, bounded by the
    /// in-flight registry size. Task IDs absent from storage are silently
    /// skipped — they aren't returned, and the caller should clean up
    /// its registry independently.
    async fn supervisor_list_cancel_requested(
        &self,
        tenant: &str,
        task_ids: &[String],
    ) -> Result<Vec<String>, A2aStorageError>;
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

    /// List configs eligible for fan-out of a specific terminal event
    /// (ADR-013 §4.5 / §6.2).
    ///
    /// Returns configs whose `registered_after_event_sequence <
    /// event_sequence`. Strictly less-than: a config recorded AT
    /// sequence N is not eligible for event sequence N, because the
    /// event was already in flight when the config's create-time CAS
    /// succeeded. The unfiltered `list_configs` remains for CRUD
    /// endpoints; this method is for the dispatcher's fan-out path
    /// and the Lambda recovery workers.
    async fn list_configs_eligible_at_event(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
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
