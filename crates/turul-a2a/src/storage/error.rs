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
