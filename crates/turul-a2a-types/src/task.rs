use crate::error::A2aTypeError;
use turul_a2a_proto as pb;

/// A2A task states — type-safe wrapper over proto TaskState.
///
/// Excludes `UNSPECIFIED` which is not a valid application state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TaskState {
    Submitted,
    Working,
    Completed,
    Failed,
    Canceled,
    InputRequired,
    Rejected,
    AuthRequired,
}

impl TaskState {
    /// Returns whether transitioning from `self` to `next` is valid per the A2A spec.
    pub fn can_transition_to(&self, next: TaskState) -> bool {
        crate::state_machine::validate_transition(*self, next).is_ok()
    }

    /// Returns `true` if this is a terminal state (no further transitions allowed).
    pub fn is_terminal(&self) -> bool {
        crate::state_machine::is_terminal(*self)
    }
}

impl TryFrom<pb::TaskState> for TaskState {
    type Error = A2aTypeError;

    fn try_from(value: pb::TaskState) -> Result<Self, Self::Error> {
        match value {
            pb::TaskState::Submitted => Ok(Self::Submitted),
            pb::TaskState::Working => Ok(Self::Working),
            pb::TaskState::Completed => Ok(Self::Completed),
            pb::TaskState::Failed => Ok(Self::Failed),
            pb::TaskState::Canceled => Ok(Self::Canceled),
            pb::TaskState::InputRequired => Ok(Self::InputRequired),
            pb::TaskState::Rejected => Ok(Self::Rejected),
            pb::TaskState::AuthRequired => Ok(Self::AuthRequired),
            pb::TaskState::Unspecified => Err(A2aTypeError::InvalidState),
        }
    }
}

impl From<TaskState> for pb::TaskState {
    fn from(value: TaskState) -> Self {
        match value {
            TaskState::Submitted => pb::TaskState::Submitted,
            TaskState::Working => pb::TaskState::Working,
            TaskState::Completed => pb::TaskState::Completed,
            TaskState::Failed => pb::TaskState::Failed,
            TaskState::Canceled => pb::TaskState::Canceled,
            TaskState::InputRequired => pb::TaskState::InputRequired,
            TaskState::Rejected => pb::TaskState::Rejected,
            TaskState::AuthRequired => pb::TaskState::AuthRequired,
        }
    }
}

impl TryFrom<i32> for TaskState {
    type Error = A2aTypeError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        let proto_state = pb::TaskState::try_from(value)
            .map_err(|_| A2aTypeError::InvalidState)?;
        Self::try_from(proto_state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_from_proto_all_valid_states() {
        assert_eq!(TaskState::try_from(pb::TaskState::Submitted).unwrap(), TaskState::Submitted);
        assert_eq!(TaskState::try_from(pb::TaskState::Working).unwrap(), TaskState::Working);
        assert_eq!(TaskState::try_from(pb::TaskState::Completed).unwrap(), TaskState::Completed);
        assert_eq!(TaskState::try_from(pb::TaskState::Failed).unwrap(), TaskState::Failed);
        assert_eq!(TaskState::try_from(pb::TaskState::Canceled).unwrap(), TaskState::Canceled);
        assert_eq!(TaskState::try_from(pb::TaskState::InputRequired).unwrap(), TaskState::InputRequired);
        assert_eq!(TaskState::try_from(pb::TaskState::Rejected).unwrap(), TaskState::Rejected);
        assert_eq!(TaskState::try_from(pb::TaskState::AuthRequired).unwrap(), TaskState::AuthRequired);
    }

    #[test]
    fn try_from_proto_unspecified_is_error() {
        assert!(TaskState::try_from(pb::TaskState::Unspecified).is_err());
    }

    #[test]
    fn into_proto_round_trip() {
        for state in [
            TaskState::Submitted,
            TaskState::Working,
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::InputRequired,
            TaskState::Rejected,
            TaskState::AuthRequired,
        ] {
            let proto: pb::TaskState = state.into();
            let back = TaskState::try_from(proto).unwrap();
            assert_eq!(back, state);
        }
    }

    #[test]
    fn try_from_i32() {
        assert_eq!(TaskState::try_from(1i32).unwrap(), TaskState::Submitted);
        assert_eq!(TaskState::try_from(2i32).unwrap(), TaskState::Working);
        assert!(TaskState::try_from(0i32).is_err()); // UNSPECIFIED
        assert!(TaskState::try_from(99i32).is_err()); // unknown
    }

    #[test]
    fn can_transition_to_delegates_to_state_machine() {
        assert!(TaskState::Submitted.can_transition_to(TaskState::Working));
        assert!(!TaskState::Submitted.can_transition_to(TaskState::Completed));
        assert!(!TaskState::Completed.can_transition_to(TaskState::Working));
    }

    #[test]
    fn is_terminal_delegates() {
        assert!(!TaskState::Working.is_terminal());
        assert!(TaskState::Completed.is_terminal());
    }
}
