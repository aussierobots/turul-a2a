/// A2A TaskState transition validation.
///
/// Validates transitions per the A2A v1.0 task lifecycle. `REJECTED`
/// may be emitted from any non-terminal state, reflecting the proto
/// spec language that an agent "may be done during initial task
/// creation or later once an agent has determined it can't or won't
/// proceed" (a2a.proto TASK_STATE_REJECTED doc comment).
///
/// ```text
/// Submitted     -> Working | Rejected | Failed | Canceled
/// Working       -> Completed | Failed | Canceled | Rejected
///                  | InputRequired | AuthRequired
/// InputRequired -> Working | Completed | Failed | Canceled | Rejected
/// AuthRequired  -> Working | Failed | Canceled | Rejected
/// Completed/Failed/Canceled/Rejected -> ERROR (terminal)
/// Unspecified -> ERROR (not a valid application state)
/// ```
use crate::error::A2aTypeError;
use crate::TaskState;

/// Validate a task state transition per A2A v1.0 lifecycle rules.
pub fn validate_transition(from: TaskState, to: TaskState) -> Result<(), A2aTypeError> {
    match from {
        TaskState::Submitted => match to {
            TaskState::Working | TaskState::Rejected | TaskState::Failed | TaskState::Canceled => {
                Ok(())
            }
            _ => Err(A2aTypeError::InvalidTransition {
                current: from,
                requested: to,
            }),
        },
        TaskState::Working => match to {
            TaskState::Completed
            | TaskState::Failed
            | TaskState::Canceled
            | TaskState::Rejected
            | TaskState::InputRequired
            | TaskState::AuthRequired => Ok(()),
            _ => Err(A2aTypeError::InvalidTransition {
                current: from,
                requested: to,
            }),
        },
        TaskState::InputRequired => match to {
            TaskState::Working
            | TaskState::Completed
            | TaskState::Failed
            | TaskState::Canceled
            | TaskState::Rejected => Ok(()),
            _ => Err(A2aTypeError::InvalidTransition {
                current: from,
                requested: to,
            }),
        },
        TaskState::AuthRequired => match to {
            TaskState::Working
            | TaskState::Failed
            | TaskState::Canceled
            | TaskState::Rejected => Ok(()),
            _ => Err(A2aTypeError::InvalidTransition {
                current: from,
                requested: to,
            }),
        },
        TaskState::Completed
        | TaskState::Failed
        | TaskState::Canceled
        | TaskState::Rejected => Err(A2aTypeError::TerminalState(from)),
    }
}

/// Returns `true` if the state is terminal (no further transitions allowed).
pub fn is_terminal(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================
    // State machine tests — from A2A v1.0 spec state transitions
    // =========================================================

    #[test]
    fn valid_submitted_transitions() {
        assert!(validate_transition(TaskState::Submitted, TaskState::Working).is_ok());
        assert!(validate_transition(TaskState::Submitted, TaskState::Rejected).is_ok());
        assert!(validate_transition(TaskState::Submitted, TaskState::Failed).is_ok());
        assert!(validate_transition(TaskState::Submitted, TaskState::Canceled).is_ok());
    }

    #[test]
    fn invalid_submitted_transitions() {
        assert!(validate_transition(TaskState::Submitted, TaskState::Completed).is_err());
        assert!(validate_transition(TaskState::Submitted, TaskState::InputRequired).is_err());
        assert!(validate_transition(TaskState::Submitted, TaskState::AuthRequired).is_err());
        assert!(validate_transition(TaskState::Submitted, TaskState::Submitted).is_err());
    }

    #[test]
    fn valid_working_transitions() {
        // REJECTED is now valid from any non-terminal state per the
        // proto enum doc: "This may be done during initial task
        // creation or later once an agent has determined it can't or
        // won't proceed."
        assert!(validate_transition(TaskState::Working, TaskState::Completed).is_ok());
        assert!(validate_transition(TaskState::Working, TaskState::Failed).is_ok());
        assert!(validate_transition(TaskState::Working, TaskState::Canceled).is_ok());
        assert!(validate_transition(TaskState::Working, TaskState::Rejected).is_ok());
        assert!(validate_transition(TaskState::Working, TaskState::InputRequired).is_ok());
        assert!(validate_transition(TaskState::Working, TaskState::AuthRequired).is_ok());
    }

    #[test]
    fn invalid_working_transitions() {
        assert!(validate_transition(TaskState::Working, TaskState::Working).is_err());
        assert!(validate_transition(TaskState::Working, TaskState::Submitted).is_err());
    }

    #[test]
    fn valid_input_required_transitions() {
        assert!(validate_transition(TaskState::InputRequired, TaskState::Working).is_ok());
        assert!(validate_transition(TaskState::InputRequired, TaskState::Completed).is_ok());
        assert!(validate_transition(TaskState::InputRequired, TaskState::Failed).is_ok());
        assert!(validate_transition(TaskState::InputRequired, TaskState::Canceled).is_ok());
        assert!(validate_transition(TaskState::InputRequired, TaskState::Rejected).is_ok());
    }

    #[test]
    fn invalid_input_required_transitions() {
        assert!(
            validate_transition(TaskState::InputRequired, TaskState::InputRequired).is_err()
        );
        assert!(validate_transition(TaskState::InputRequired, TaskState::AuthRequired).is_err());
        assert!(validate_transition(TaskState::InputRequired, TaskState::Submitted).is_err());
    }

    #[test]
    fn valid_auth_required_transitions() {
        assert!(validate_transition(TaskState::AuthRequired, TaskState::Working).is_ok());
        assert!(validate_transition(TaskState::AuthRequired, TaskState::Failed).is_ok());
        assert!(validate_transition(TaskState::AuthRequired, TaskState::Canceled).is_ok());
        assert!(validate_transition(TaskState::AuthRequired, TaskState::Rejected).is_ok());
    }

    #[test]
    fn invalid_auth_required_transitions() {
        // AUTH_REQUIRED -> COMPLETED is NOT valid per spec
        assert!(validate_transition(TaskState::AuthRequired, TaskState::Completed).is_err());
        assert!(validate_transition(TaskState::AuthRequired, TaskState::AuthRequired).is_err());
        assert!(validate_transition(TaskState::AuthRequired, TaskState::InputRequired).is_err());
        assert!(validate_transition(TaskState::AuthRequired, TaskState::Submitted).is_err());
    }

    #[test]
    fn terminal_states_reject_all_transitions() {
        for terminal in [
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::Rejected,
        ] {
            for target in [
                TaskState::Submitted,
                TaskState::Working,
                TaskState::Completed,
                TaskState::Failed,
                TaskState::Canceled,
                TaskState::InputRequired,
                TaskState::AuthRequired,
                TaskState::Rejected,
            ] {
                let result = validate_transition(terminal, target);
                assert!(
                    result.is_err(),
                    "Expected error for {terminal:?} -> {target:?}"
                );
                match result.unwrap_err() {
                    A2aTypeError::TerminalState(s) => assert_eq!(s, terminal),
                    other => panic!("Expected TerminalState, got: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn is_terminal_correct() {
        assert!(!is_terminal(TaskState::Submitted));
        assert!(!is_terminal(TaskState::Working));
        assert!(!is_terminal(TaskState::InputRequired));
        assert!(!is_terminal(TaskState::AuthRequired));
        assert!(is_terminal(TaskState::Completed));
        assert!(is_terminal(TaskState::Failed));
        assert!(is_terminal(TaskState::Canceled));
        assert!(is_terminal(TaskState::Rejected));
    }
}
