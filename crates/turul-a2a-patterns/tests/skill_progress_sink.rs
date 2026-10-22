//! `SkillProgressSink` binding contract:
//! - object-safe (`dyn SkillProgressSink` is a valid type),
//! - `ProgressState` enum has Working/InputRequired/AuthRequired and
//!   no terminal states (Completed/Failed/Canceled/Rejected),
//! - `SinkError` has `Closed` and `Backend(String)`,
//! - trait is `Send + Sync`,
//! - shape is `#[async_trait]` so impls are plain `async fn`.

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use static_assertions::{assert_impl_all, assert_obj_safe};

use turul_a2a_patterns::{ProgressState, SinkError, SkillProgressSink};
use turul_a2a_types::{Artifact, Message};

assert_obj_safe!(SkillProgressSink);

/// Stub sink — positive case: a hand-rolled impl compiles and is
/// `Send + Sync` (required by the trait bound).
struct OkSink {
    closed: AtomicBool,
}

#[async_trait]
impl SkillProgressSink for OkSink {
    async fn set_status(
        &self,
        _state: ProgressState,
        _message: Option<Message>,
    ) -> Result<(), SinkError> {
        Ok(())
    }

    async fn emit_artifact(
        &self,
        _artifact: Artifact,
        _append: bool,
        _last_chunk: bool,
    ) -> Result<(), SinkError> {
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

assert_impl_all!(OkSink: Send, Sync);

#[tokio::test]
async fn ok_sink_dispatches_through_dyn_reference() {
    let sink = OkSink {
        closed: AtomicBool::new(false),
    };
    let dyn_ref: &dyn SkillProgressSink = &sink;
    let res = dyn_ref.set_status(ProgressState::Working, None).await;
    assert!(res.is_ok());
    let res = dyn_ref.set_status(ProgressState::InputRequired, None).await;
    assert!(res.is_ok());
    assert!(!dyn_ref.is_closed());
}

// `ProgressState` and `SinkError` are `#[non_exhaustive]`, so matches in
// this consumer crate need a wildcard arm and cannot detect an added
// variant. The guarantee that `ProgressState` holds only the three
// non-terminal states (and that `SinkError` holds only `Closed` and
// `Backend`) is enforced by the wildcard-free matches in the crate's own
// `sink` and `error` module tests, which stop compiling if a variant is
// added.

/// Consumer view: the three non-terminal states are constructible and
/// distinct.
#[test]
fn progress_state_non_terminal_variants_are_distinct() {
    assert_ne!(ProgressState::Working, ProgressState::InputRequired);
    assert_ne!(ProgressState::Working, ProgressState::AuthRequired);
    assert_ne!(ProgressState::InputRequired, ProgressState::AuthRequired);
}

/// Consumer view: both sink error variants are constructible and
/// distinguishable.
#[test]
fn sink_error_has_closed_and_backend_variants() {
    let e1 = SinkError::Closed;
    let e2 = SinkError::Backend("net".into());
    let l1 = match &e1 {
        SinkError::Closed => "closed",
        SinkError::Backend(_) => "backend",
        _ => "unexpected",
    };
    let l2 = match &e2 {
        SinkError::Closed => "closed",
        SinkError::Backend(_) => "backend",
        _ => "unexpected",
    };
    assert_eq!(l1, "closed");
    assert_eq!(l2, "backend");
}
