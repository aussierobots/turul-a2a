//! Error type for this crate.

use crate::TaskState;

/// Errors from the A2A types crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum A2aTypeError {
    /// Returned when a task is asked to move to a state the state machine does not allow from its current state.
    #[error("Invalid state transition: {current:?} -> {requested:?}")]
    InvalidTransition {
        /// The state the task is in.
        current: TaskState,
        /// The state the caller tried to move the task to.
        requested: TaskState,
    },
    /// Returned when a transition is requested from a terminal state, which accepts no further changes; carries that state.
    #[error("Task is in terminal state: {0:?}")]
    TerminalState(TaskState),
    /// Returned when a task state or message role is `UNSPECIFIED` or not a known enum value.
    #[error("Invalid state: UNSPECIFIED is not a valid application state")]
    InvalidState,
    /// Returned when a wrapper is built from a proto value whose required field is empty, absent or unspecified; carries the field name.
    #[error("Missing required field: {0}")]
    MissingField(&'static str),
    /// Returned when a data part's JSON cannot be deserialized into the requested type; carries the serde error message.
    #[error("Deserialization error: {0}")]
    Deserialization(String),
}
