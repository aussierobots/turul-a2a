//! Progress sink trait used by handlers to emit non-terminal status updates
//! and streaming artifacts. Terminal task states remain framework-owned and
//! are deliberately not constructible through this surface (§2.3).

use async_trait::async_trait;

use turul_a2a_types::{Artifact, Message};

use crate::error::SinkError;

/// Non-terminal task states a skill may emit during execution.
/// Terminal states are framework-owned; not constructible here.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    Working,
    InputRequired,
    AuthRequired,
}

/// Object-safe progress emission trait used by `SkillHandler::run`.
///
/// Implemented with `#[async_trait]` so adopters write standard
/// `async fn` rather than hand-rolling `Pin<Box<dyn Future>>`. The
/// macro produces an object-safe trait, verified by the const-fn
/// assertion at the bottom of this module.
#[async_trait]
pub trait SkillProgressSink: Send + Sync {
    async fn set_status(
        &self,
        state: ProgressState,
        message: Option<Message>,
    ) -> Result<(), SinkError>;

    async fn emit_artifact(
        &self,
        artifact: Artifact,
        append: bool,
        last_chunk: bool,
    ) -> Result<(), SinkError>;

    fn is_closed(&self) -> bool {
        false
    }
}

// Compile-time object-safety assertion (§2.3 contract).
const _: fn() = || {
    fn assert<T: ?Sized + SkillProgressSink>() {}
    assert::<dyn SkillProgressSink>();
};
