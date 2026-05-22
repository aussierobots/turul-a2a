//! ADR-021 §2.2 item 1 — `SkillHandler` is an async, object-safe trait
//! that takes structured input + a `&dyn SkillProgressSink` and returns
//! a typed result.

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde_json::json;
use static_assertions::assert_obj_safe;

use turul_a2a_patterns::{ProgressState, SinkError, SkillError, SkillHandler, SkillProgressSink};
use turul_a2a_types::{Artifact, Message};

assert_obj_safe!(SkillHandler);

struct StubSink;

#[async_trait]
impl SkillProgressSink for StubSink {
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
}

/// Positive: a hand-written `SkillHandler` is dispatchable through
/// `&dyn SkillHandler` and reaches the sink. Phase A: this fails
/// because no first-party handler has been built yet, so we drive a
/// stub through the framework-supplied glue once it lands. Today the
/// glue does not exist, so we explicitly assert presence.
#[tokio::test]
async fn skill_handler_trait_is_dyn_dispatchable() {
    struct Echo {
        called: AtomicBool,
    }

    #[async_trait]
    impl SkillHandler for Echo {
        async fn run(
            &self,
            params: serde_json::Value,
            _sink: &dyn SkillProgressSink,
        ) -> Result<serde_json::Value, SkillError> {
            self.called.store(true, Ordering::SeqCst);
            Ok(params)
        }
    }

    let handler: Box<dyn SkillHandler> = Box::new(Echo {
        called: AtomicBool::new(false),
    });
    let sink = StubSink;
    let out = handler.run(json!({"msg": "hello"}), &sink).await.unwrap();
    assert_eq!(out, json!({"msg": "hello"}));
}

/// Negative: `SkillHandler::run` returning `Err(SkillError::InvalidRequest)`
/// propagates as the typed error variant (§2.2 item 5 maps to A2A
/// InvalidRequest).
#[tokio::test]
async fn skill_handler_invalid_request_propagates_as_typed_error() {
    struct Rejecting;

    #[async_trait]
    impl SkillHandler for Rejecting {
        async fn run(
            &self,
            _params: serde_json::Value,
            _sink: &dyn SkillProgressSink,
        ) -> Result<serde_json::Value, SkillError> {
            Err(SkillError::InvalidRequest("missing field".into()))
        }
    }

    let handler: Box<dyn SkillHandler> = Box::new(Rejecting);
    let sink = StubSink;
    let err = handler.run(json!({}), &sink).await.unwrap_err();
    match err {
        SkillError::InvalidRequest(msg) => assert_eq!(msg, "missing field"),
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}
