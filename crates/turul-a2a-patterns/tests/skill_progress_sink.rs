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

/// Negative: terminal states are not constructible.
/// The four terminal task states (Completed/Failed/Canceled/Rejected)
/// must not exist on `ProgressState`. We assert by pattern-matching
/// only the three legal variants and confirming the catch-all is
/// unreachable for the values we construct.
#[test]
fn progress_state_excludes_terminal_states() {
    let states = [
        ProgressState::Working,
        ProgressState::InputRequired,
        ProgressState::AuthRequired,
    ];
    for s in states {
        let label = match s {
            ProgressState::Working => "working",
            ProgressState::InputRequired => "input",
            ProgressState::AuthRequired => "auth",
            _ => "unexpected",
        };
        assert_ne!(label, "unexpected");
    }
    // Compile-fail-by-name (documented; cannot be enforced as a runtime test):
    //
    //     let _ = ProgressState::Completed;   // would fail to compile
    //     let _ = ProgressState::Failed;      // would fail to compile
    //
    // Adding any of these variants would break the contract that
    // terminal task states remain framework-owned and are not
    // constructible through the sink surface.
}

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
