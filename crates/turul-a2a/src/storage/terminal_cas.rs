//! SQL fragment for the single-terminal-writer CAS.
//!
//! The SQL backends persist `status_state` as the proto wire name
//! ([`turul_a2a_types::TaskState::wire_name`]) and guard terminal writes
//! with `WHERE status_state NOT IN (<terminal states>)`. The list is
//! derived from [`turul_a2a_types::state_machine::TERMINAL_STATES`] so it
//! cannot drift from the state machine.

use std::sync::LazyLock;

use turul_a2a_types::state_machine::{EXECUTOR_HELD_STATES, TERMINAL_STATES};

/// `'TASK_STATE_COMPLETED', 'TASK_STATE_FAILED', ...` — the body of a SQL
/// `IN (...)` list over the terminal states. Wire names are fixed
/// uppercase identifiers, so inlining them as literals is injection-safe.
pub(crate) static TERMINAL_STATES_SQL: LazyLock<String> = LazyLock::new(|| {
    TERMINAL_STATES
        .iter()
        .map(|s| format!("'{}'", s.wire_name()))
        .collect::<Vec<_>>()
        .join(", ")
});

/// `'TASK_STATE_SUBMITTED', 'TASK_STATE_WORKING'` — the body of a SQL
/// `IN (...)` list over the states an executor holds.
pub(crate) static EXECUTOR_HELD_STATES_SQL: LazyLock<String> = LazyLock::new(|| {
    EXECUTOR_HELD_STATES
        .iter()
        .map(|s| format!("'{}'", s.wire_name()))
        .collect::<Vec<_>>()
        .join(", ")
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_list_is_the_wire_names_of_the_terminal_states() {
        assert_eq!(
            TERMINAL_STATES_SQL.as_str(),
            "'TASK_STATE_COMPLETED', 'TASK_STATE_FAILED', 'TASK_STATE_CANCELED', 'TASK_STATE_REJECTED'"
        );
    }
}
