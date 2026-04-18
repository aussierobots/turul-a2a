use turul_a2a_types::{A2aTypeError, TaskState};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum A2aStorageError {
    #[error("Task not found: {0}")]
    TaskNotFound(String),

    #[error("Invalid state transition: {current:?} -> {requested:?}")]
    InvalidTransition {
        current: TaskState,
        requested: TaskState,
    },

    #[error("Task is in terminal state: {0:?}")]
    TerminalState(TaskState),

    /// The atomic store's single-terminal-writer invariant rejected a
    /// terminal write because the task was already in a terminal state
    /// at the time of the CAS. Distinct from [`Self::TerminalState`]
    /// which is the raw state-machine signal: `TerminalStateAlreadySet`
    /// is specifically the "you lost the race" signal emitted by
    /// [`crate::storage::A2aAtomicStore::update_task_status_with_events`].
    /// Callers typically translate this to HTTP 409 `TaskNotCancelable` /
    /// JSON-RPC `-32002` on the wire, and to `EventSink::closed` semantics
    /// when the caller is an executor sink.
    ///
    /// The `current_state` field carries the wire enum name (e.g.
    /// `"TASK_STATE_COMPLETED"`) so log/telemetry consumers can see
    /// exactly which terminal won the race.
    #[error("task {task_id} already in terminal state {current_state} (CAS loser)")]
    TerminalStateAlreadySet {
        task_id: String,
        current_state: String,
    },

    #[error("Owner mismatch for task: {task_id}")]
    OwnerMismatch { task_id: String },

    #[error("Tenant mismatch for task: {task_id}")]
    TenantMismatch { task_id: String },

    #[error("Concurrent modification: {0}")]
    ConcurrentModification(String),

    #[error("Push notification config not found: {0}")]
    PushConfigNotFound(String),

    #[error("Database error: {0}")]
    DatabaseError(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),

    #[error("Type error: {0}")]
    TypeError(#[from] A2aTypeError),

    #[error("Generic storage error: {0}")]
    Generic(String),
}
