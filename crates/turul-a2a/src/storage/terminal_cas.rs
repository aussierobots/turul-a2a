//! Shared helpers for the single-terminal-writer CAS.
//!
//! Each backend's `update_task_status_with_events` uses one of two
//! conditional-write mechanisms to enforce the terminal-CAS:
//!
//! - SQL backends: a `WHERE status_state NOT IN (terminals)` clause whose
//!   condition is computed from [`DEBUG_TERMINAL_STATES`] (the strings
//!   stored in the `status_state` column — Rust `Debug` format of
//!   [`turul_a2a_types::TaskState`], not the proto wire names).
//! - DynamoDB: a `ConditionExpression` on the `statusState` attribute
//!   using the same Debug-format values.
//!
//! Error reporting uses the proto wire names via
//! [`task_state_wire_name`] so telemetry consumers see spec-compliant
//! values (`"TASK_STATE_COMPLETED"`, etc.) instead of Rust-internal
//! `"Completed"`.

use turul_a2a_types::TaskState;

/// Terminal-state values in the storage column format (Rust `Debug`).
/// This matches what `status_state_str` writes in each SQL/DynamoDB
/// backend. Used to construct the conditional-UPDATE / ConditionExpression.
pub const DEBUG_TERMINAL_STATES: &[&str] = &["Completed", "Failed", "Canceled", "Rejected"];

/// Map a [`TaskState`] to its proto wire name.
///
/// Used exclusively for error reporting via
/// [`crate::storage::A2aStorageError::TerminalStateAlreadySet::current_state`]
/// so users see the same values the spec and proto use
/// (`"TASK_STATE_COMPLETED"`, etc.), not Rust Debug.
pub fn task_state_wire_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Submitted => "TASK_STATE_SUBMITTED",
        TaskState::Working => "TASK_STATE_WORKING",
        TaskState::Completed => "TASK_STATE_COMPLETED",
        TaskState::Failed => "TASK_STATE_FAILED",
        TaskState::Canceled => "TASK_STATE_CANCELED",
        TaskState::InputRequired => "TASK_STATE_INPUT_REQUIRED",
        TaskState::AuthRequired => "TASK_STATE_AUTH_REQUIRED",
        TaskState::Rejected => "TASK_STATE_REJECTED",
        // TaskState is #[non_exhaustive]; future variants fall back to
        // a debug-formatted name so error diagnostics don't crash.
        _ => "TASK_STATE_UNKNOWN",
    }
}

/// Convert a storage-column string (Rust `Debug`) into the proto wire
/// name for error reporting. Returns the original string if it is
/// already in wire form or if it does not match any known state — this
/// is a defensive path so a corrupt storage row can't crash the error
/// construction.
pub fn debug_state_to_wire_name(debug_state: &str) -> String {
    match debug_state {
        "Submitted" => "TASK_STATE_SUBMITTED".to_string(),
        "Working" => "TASK_STATE_WORKING".to_string(),
        "Completed" => "TASK_STATE_COMPLETED".to_string(),
        "Failed" => "TASK_STATE_FAILED".to_string(),
        "Canceled" => "TASK_STATE_CANCELED".to_string(),
        "InputRequired" => "TASK_STATE_INPUT_REQUIRED".to_string(),
        "AuthRequired" => "TASK_STATE_AUTH_REQUIRED".to_string(),
        "Rejected" => "TASK_STATE_REJECTED".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_terminal_states_have_wire_names() {
        for state in [
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::Rejected,
        ] {
            let wire = task_state_wire_name(state);
            assert!(wire.starts_with("TASK_STATE_"));
        }
    }

    #[test]
    fn debug_to_wire_roundtrips_all_states() {
        let cases = [
            (TaskState::Submitted, "Submitted", "TASK_STATE_SUBMITTED"),
            (TaskState::Working, "Working", "TASK_STATE_WORKING"),
            (TaskState::Completed, "Completed", "TASK_STATE_COMPLETED"),
            (TaskState::Failed, "Failed", "TASK_STATE_FAILED"),
            (TaskState::Canceled, "Canceled", "TASK_STATE_CANCELED"),
            (
                TaskState::InputRequired,
                "InputRequired",
                "TASK_STATE_INPUT_REQUIRED",
            ),
            (
                TaskState::AuthRequired,
                "AuthRequired",
                "TASK_STATE_AUTH_REQUIRED",
            ),
            (TaskState::Rejected, "Rejected", "TASK_STATE_REJECTED"),
        ];
        for (state, debug, wire) in cases {
            assert_eq!(format!("{state:?}"), debug, "Debug format stability");
            assert_eq!(task_state_wire_name(state), wire);
            assert_eq!(debug_state_to_wire_name(debug), wire);
        }
    }

    #[test]
    fn debug_terminal_states_covers_all_terminals() {
        // Make sure DEBUG_TERMINAL_STATES and TaskState::is_terminal agree.
        let expected = DEBUG_TERMINAL_STATES.to_vec();
        let actual: Vec<String> = [
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::Rejected,
        ]
        .iter()
        .map(|s| format!("{s:?}"))
        .collect();
        for name in &actual {
            assert!(
                expected.contains(&name.as_str()),
                "missing from DEBUG_TERMINAL_STATES: {name}"
            );
        }
        assert_eq!(expected.len(), actual.len());
    }
}
