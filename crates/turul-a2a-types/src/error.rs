use crate::TaskState;

/// Errors from the A2A types crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum A2aTypeError {
    #[error("Invalid state transition: {current:?} -> {requested:?}")]
    InvalidTransition {
        current: TaskState,
        requested: TaskState,
    },
    #[error("Task is in terminal state: {0:?}")]
    TerminalState(TaskState),
    #[error("Invalid state: UNSPECIFIED is not a valid application state")]
    InvalidState,
    #[error("Missing required field: {0}")]
    MissingField(&'static str),
}
