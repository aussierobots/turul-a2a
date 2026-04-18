use serde::{Deserialize, Serialize};
use turul_a2a_proto as pb;

use crate::artifact::Artifact;
use crate::error::A2aTypeError;
use crate::message::Message;

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

/// Ergonomic wrapper over proto `TaskStatus`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TaskStatus {
    pub(crate) inner: pb::TaskStatus,
}

impl TaskStatus {
    pub fn new(state: TaskState) -> Self {
        Self {
            inner: pb::TaskStatus {
                state: pb::TaskState::from(state).into(),
                message: None,
                timestamp: None,
            },
        }
    }

    pub fn with_message(mut self, message: Message) -> Self {
        self.inner.message = Some(message.into_proto());
        self
    }

    pub fn state(&self) -> Result<TaskState, A2aTypeError> {
        let proto_state = pb::TaskState::try_from(self.inner.state)
            .map_err(|_| A2aTypeError::InvalidState)?;
        TaskState::try_from(proto_state)
    }

    pub fn as_proto(&self) -> &pb::TaskStatus {
        &self.inner
    }

    pub fn into_proto(self) -> pb::TaskStatus {
        self.inner
    }
}

impl TryFrom<pb::TaskStatus> for TaskStatus {
    type Error = A2aTypeError;

    fn try_from(inner: pb::TaskStatus) -> Result<Self, Self::Error> {
        // Validate state is not UNSPECIFIED
        let proto_state = pb::TaskState::try_from(inner.state)
            .map_err(|_| A2aTypeError::InvalidState)?;
        if proto_state == pb::TaskState::Unspecified {
            return Err(A2aTypeError::InvalidState);
        }
        Ok(Self { inner })
    }
}

impl From<TaskStatus> for pb::TaskStatus {
    fn from(status: TaskStatus) -> Self {
        status.inner
    }
}

impl Serialize for TaskStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.inner.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TaskStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let proto = pb::TaskStatus::deserialize(deserializer)?;
        TaskStatus::try_from(proto).map_err(serde::de::Error::custom)
    }
}

/// Ergonomic wrapper over proto `Task`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Task {
    pub(crate) inner: pb::Task,
}

impl Task {
    pub fn new(id: impl Into<String>, status: TaskStatus) -> Self {
        Self {
            inner: pb::Task {
                id: id.into(),
                context_id: String::new(),
                status: Some(status.into_proto()),
                artifacts: vec![],
                history: vec![],
                metadata: None,
            },
        }
    }

    pub fn with_context_id(mut self, context_id: impl Into<String>) -> Self {
        self.inner.context_id = context_id.into();
        self
    }

    pub fn id(&self) -> &str {
        &self.inner.id
    }

    pub fn context_id(&self) -> &str {
        &self.inner.context_id
    }

    pub fn status(&self) -> Option<TaskStatus> {
        self.inner.status.clone().and_then(|s| TaskStatus::try_from(s).ok())
    }

    pub fn history(&self) -> &[pb::Message] {
        &self.inner.history
    }

    pub fn artifacts(&self) -> &[pb::Artifact] {
        &self.inner.artifacts
    }

    pub fn append_message(&mut self, message: Message) {
        self.inner.history.push(message.into_proto());
    }

    pub fn append_artifact(&mut self, artifact: Artifact) {
        self.inner.artifacts.push(artifact.into_proto());
    }

    /// Merge an artifact using A2A streaming append semantics.
    ///
    /// - `append = true`: if an artifact with the same `artifactId` is
    ///   already on the task, extend its `parts` with the incoming parts
    ///   (same as a streaming-chunk continuation). Otherwise, append as
    ///   a new entry.
    /// - `append = false`: append unconditionally (new entry or caller
    ///   tolerates duplicate ids).
    ///
    /// The `last_chunk` flag is transport-only metadata (ADR-006 / ADR-009)
    /// — it is not persisted on the task. It is accepted here so callers
    /// can keep the signature symmetric with the streaming wire event
    /// payload and not drop the parameter separately.
    ///
    /// This mirrors the server storage append-artifact semantics so that
    /// in-memory task mutations and the storage layer converge on the
    /// same view. The storage trait lives in the server crate; this
    /// helper is the dependency-free equivalent for wrapper callers.
    pub fn merge_artifact(&mut self, artifact: Artifact, append: bool, _last_chunk: bool) {
        if append {
            let target_id = artifact.as_proto().artifact_id.clone();
            if let Some(existing) = self
                .inner
                .artifacts
                .iter_mut()
                .find(|a| a.artifact_id == target_id)
            {
                existing.parts.extend(artifact.into_proto().parts);
                return;
            }
        }
        self.append_artifact(artifact);
    }

    /// Set the task's status. This is the low-level escape hatch —
    /// prefer `complete()`, `fail()`, etc. for common transitions.
    pub fn set_status(&mut self, status: TaskStatus) {
        self.inner.status = Some(status.into_proto());
    }

    /// Mark the task as completed.
    pub fn complete(&mut self) {
        self.set_status(TaskStatus::new(TaskState::Completed));
    }

    /// Mark the task as failed with an optional message.
    pub fn fail(&mut self, message: impl Into<String>) {
        let msg = Message::new(
            uuid::Uuid::now_v7().to_string(),
            crate::Role::Agent,
            vec![crate::Part::text(message)],
        );
        self.set_status(TaskStatus::new(TaskState::Failed).with_message(msg));
    }

    /// Add a text artifact to the task.
    pub fn push_text_artifact(
        &mut self,
        artifact_id: impl Into<String>,
        name: impl Into<String>,
        text: impl Into<String>,
    ) {
        let artifact = Artifact::new(artifact_id, vec![crate::Part::text(text)])
            .with_name(name);
        self.append_artifact(artifact);
    }

    pub fn as_proto(&self) -> &pb::Task {
        &self.inner
    }

    pub fn as_proto_mut(&mut self) -> &mut pb::Task {
        &mut self.inner
    }

    pub fn into_proto(self) -> pb::Task {
        self.inner
    }
}

impl TryFrom<pb::Task> for Task {
    type Error = A2aTypeError;

    fn try_from(inner: pb::Task) -> Result<Self, Self::Error> {
        if inner.id.is_empty() {
            return Err(A2aTypeError::MissingField("id"));
        }
        // Status is REQUIRED per proto field_behavior
        let status = inner
            .status
            .as_ref()
            .ok_or(A2aTypeError::MissingField("status"))?;
        let proto_state = pb::TaskState::try_from(status.state)
            .map_err(|_| A2aTypeError::InvalidState)?;
        if proto_state == pb::TaskState::Unspecified {
            return Err(A2aTypeError::InvalidState);
        }
        Ok(Self { inner })
    }
}

impl From<Task> for pb::Task {
    fn from(task: Task) -> Self {
        task.inner
    }
}

impl Serialize for Task {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.inner.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Task {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let proto = pb::Task::deserialize(deserializer)?;
        Task::try_from(proto).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Part, Role};

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

    // TaskStatus tests

    #[test]
    fn task_status_constructor() {
        let status = TaskStatus::new(TaskState::Working);
        assert_eq!(status.state().unwrap(), TaskState::Working);
    }

    #[test]
    fn task_status_with_message() {
        let msg = crate::Message::new("s-msg", Role::Agent, vec![Part::text("working")]);
        let status = TaskStatus::new(TaskState::Working).with_message(msg);
        assert!(status.as_proto().message.is_some());
    }

    #[test]
    fn task_status_try_from_proto_rejects_unspecified() {
        let proto = pb::TaskStatus {
            state: pb::TaskState::Unspecified.into(),
            message: None,
            timestamp: None,
        };
        assert!(TaskStatus::try_from(proto).is_err());
    }

    #[test]
    fn task_status_serde_round_trip() {
        let status = TaskStatus::new(TaskState::Submitted);
        let json = serde_json::to_string(&status).unwrap();
        let back: TaskStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.state().unwrap(), TaskState::Submitted);
    }

    // Task tests

    #[test]
    fn task_constructor() {
        let task = Task::new("t-1", TaskStatus::new(TaskState::Submitted))
            .with_context_id("ctx-1");
        assert_eq!(task.id(), "t-1");
        assert_eq!(task.context_id(), "ctx-1");
        assert_eq!(task.status().unwrap().state().unwrap(), TaskState::Submitted);
    }

    #[test]
    fn task_append_history_and_artifacts() {
        let mut task = Task::new("t-2", TaskStatus::new(TaskState::Working));
        task.append_message(crate::Message::new("m-1", Role::User, vec![Part::text("hi")]));
        task.append_artifact(crate::Artifact::new("a-1", vec![Part::text("result")]));
        assert_eq!(task.history().len(), 1);
        assert_eq!(task.artifacts().len(), 1);
    }

    #[test]
    fn task_merge_artifact_append_true_extends_existing_by_id() {
        let mut task = Task::new("t-merge-1", TaskStatus::new(TaskState::Working));
        task.append_artifact(crate::Artifact::new("a-1", vec![Part::text("chunk-1")]));

        task.merge_artifact(
            crate::Artifact::new("a-1", vec![Part::text("chunk-2")]),
            true,
            false,
        );

        assert_eq!(task.artifacts().len(), 1, "same-id append must not duplicate");
        assert_eq!(task.artifacts()[0].parts.len(), 2);
    }

    #[test]
    fn task_merge_artifact_append_true_no_match_adds_new() {
        let mut task = Task::new("t-merge-2", TaskStatus::new(TaskState::Working));
        task.append_artifact(crate::Artifact::new("a-1", vec![Part::text("x")]));

        task.merge_artifact(
            crate::Artifact::new("a-2", vec![Part::text("y")]),
            true,
            false,
        );

        assert_eq!(task.artifacts().len(), 2);
    }

    #[test]
    fn task_merge_artifact_append_false_always_appends() {
        let mut task = Task::new("t-merge-3", TaskStatus::new(TaskState::Working));
        task.append_artifact(crate::Artifact::new("a-1", vec![Part::text("x")]));

        task.merge_artifact(
            crate::Artifact::new("a-1", vec![Part::text("y")]),
            false,
            true,
        );

        assert_eq!(
            task.artifacts().len(),
            2,
            "append=false does not merge by id"
        );
    }

    #[test]
    fn task_try_from_proto_rejects_empty_id() {
        let proto = pb::Task {
            id: String::new(),
            context_id: String::new(),
            status: Some(pb::TaskStatus {
                state: pb::TaskState::Submitted.into(),
                message: None,
                timestamp: None,
            }),
            artifacts: vec![],
            history: vec![],
            metadata: None,
        };
        assert!(Task::try_from(proto).is_err());
    }

    #[test]
    fn task_try_from_proto_rejects_missing_status() {
        // Status is REQUIRED per proto field_behavior
        let proto = pb::Task {
            id: "t-no-status".to_string(),
            context_id: String::new(),
            status: None,
            artifacts: vec![],
            history: vec![],
            metadata: None,
        };
        assert!(Task::try_from(proto).is_err());
    }

    #[test]
    fn task_try_from_proto_rejects_unspecified_state() {
        let proto = pb::Task {
            id: "t-bad".to_string(),
            context_id: String::new(),
            status: Some(pb::TaskStatus {
                state: pb::TaskState::Unspecified.into(),
                message: None,
                timestamp: None,
            }),
            artifacts: vec![],
            history: vec![],
            metadata: None,
        };
        assert!(Task::try_from(proto).is_err());
    }

    #[test]
    fn task_serde_round_trip() {
        let task = Task::new("t-rt", TaskStatus::new(TaskState::Working))
            .with_context_id("ctx-rt");
        let json = serde_json::to_string(&task).unwrap();
        let back: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id(), "t-rt");
        assert_eq!(back.context_id(), "ctx-rt");
    }

    // Task helper tests

    #[test]
    fn task_complete_sets_completed_status() {
        let mut task = Task::new("h-1", TaskStatus::new(TaskState::Submitted));
        task.complete();
        assert_eq!(task.status().unwrap().state().unwrap(), TaskState::Completed);
    }

    #[test]
    fn task_fail_sets_failed_status_with_message() {
        let mut task = Task::new("h-2", TaskStatus::new(TaskState::Submitted));
        task.fail("something went wrong");
        let status = task.status().unwrap();
        assert_eq!(status.state().unwrap(), TaskState::Failed);
        // Status should have a message
        assert!(status.as_proto().message.is_some());
    }

    #[test]
    fn task_set_status_generic() {
        let mut task = Task::new("h-3", TaskStatus::new(TaskState::Submitted));
        task.set_status(TaskStatus::new(TaskState::Working));
        assert_eq!(task.status().unwrap().state().unwrap(), TaskState::Working);
    }

    #[test]
    fn task_push_text_artifact() {
        let mut task = Task::new("h-4", TaskStatus::new(TaskState::Submitted));
        task.push_text_artifact("art-1", "Result", "hello world");
        assert_eq!(task.artifacts().len(), 1);
        assert_eq!(task.artifacts()[0].artifact_id, "art-1");
        assert_eq!(task.artifacts()[0].name, "Result");
        assert_eq!(task.artifacts()[0].parts.len(), 1);
    }
}
