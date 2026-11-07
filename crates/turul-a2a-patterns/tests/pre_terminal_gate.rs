//! `GatedHandler` contract: gates run after a successful inner handler,
//! in the order added; the first veto becomes the result and stops the
//! rest; an inner error skips every gate; each gate sees the skill id,
//! params and output.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use static_assertions::assert_obj_safe;

use turul_a2a_patterns::{
    GateContext, GatedHandler, PreTerminalGate, ProgressState, SinkError, SkillError, SkillHandler,
    SkillProgressSink,
};
use turul_a2a_types::{Artifact, Message};

assert_obj_safe!(PreTerminalGate);

struct NullSink;

#[async_trait]
impl SkillProgressSink for NullSink {
    async fn set_status(&self, _: ProgressState, _: Option<Message>) -> Result<(), SinkError> {
        Ok(())
    }
    async fn emit_artifact(&self, _: Artifact, _: bool, _: bool) -> Result<(), SinkError> {
        Ok(())
    }
}

struct Echo;

#[async_trait]
impl SkillHandler for Echo {
    async fn run(&self, params: Value, _: &dyn SkillProgressSink) -> Result<Value, SkillError> {
        Ok(json!({ "echo": params }))
    }
}

struct Fails;

#[async_trait]
impl SkillHandler for Fails {
    async fn run(&self, _: Value, _: &dyn SkillProgressSink) -> Result<Value, SkillError> {
        Err(SkillError::InvalidRequest("bad input".into()))
    }
}

type Log = Arc<Mutex<Vec<(String, String, Value, Value)>>>;

struct Recording {
    name: &'static str,
    veto: bool,
    log: Log,
}

#[async_trait]
impl PreTerminalGate for Recording {
    async fn check(&self, ctx: GateContext<'_>) -> Result<(), SkillError> {
        self.log.lock().unwrap().push((
            self.name.to_string(),
            ctx.skill_id.to_string(),
            ctx.params.clone(),
            ctx.output.clone(),
        ));
        if self.veto {
            Err(SkillError::Rejected {
                gate: self.name.to_string(),
                reason: "vetoed".into(),
            })
        } else {
            Ok(())
        }
    }
}

fn gate(name: &'static str, veto: bool, log: &Log) -> Arc<dyn PreTerminalGate> {
    Arc::new(Recording {
        name,
        veto,
        log: log.clone(),
    })
}

fn names(log: &Log) -> Vec<String> {
    log.lock().unwrap().iter().map(|e| e.0.clone()).collect()
}

#[tokio::test]
async fn passing_gates_run_in_order_and_keep_output() {
    let log = Log::default();
    let handler = GatedHandler::new("echo", Arc::new(Echo))
        .gate(gate("a", false, &log))
        .gate(gate("b", false, &log));
    let out = handler.run(json!({"x": 1}), &NullSink).await.unwrap();
    assert_eq!(out, json!({"echo": {"x": 1}}));
    assert_eq!(names(&log), ["a", "b"]);
}

#[tokio::test]
async fn first_veto_is_the_result_and_stops_later_gates() {
    let log = Log::default();
    let handler = GatedHandler::new("echo", Arc::new(Echo))
        .gate(gate("a", false, &log))
        .gate(gate("b", true, &log))
        .gate(gate("c", true, &log));
    let err = handler.run(json!({}), &NullSink).await.unwrap_err();
    assert!(
        matches!(&err, SkillError::Rejected { gate, reason } if gate == "b" && reason == "vetoed"),
        "{err:?}"
    );
    assert_eq!(names(&log), ["a", "b"]);
}

#[tokio::test]
async fn inner_error_skips_every_gate() {
    let log = Log::default();
    let handler = GatedHandler::new("fails", Arc::new(Fails)).gate(gate("a", true, &log));
    let err = handler.run(json!({}), &NullSink).await.unwrap_err();
    assert!(
        matches!(&err, SkillError::InvalidRequest(m) if m == "bad input"),
        "{err:?}"
    );
    assert!(names(&log).is_empty());
}

#[tokio::test]
async fn gate_context_carries_skill_id_params_and_output() {
    let log = Log::default();
    let handler = GatedHandler::new("echo", Arc::new(Echo)).gate(gate("a", false, &log));
    handler
        .run(json!({"name": "Ada"}), &NullSink)
        .await
        .unwrap();
    let entry = log.lock().unwrap()[0].clone();
    assert_eq!(entry.1, "echo");
    assert_eq!(entry.2, json!({"name": "Ada"}));
    assert_eq!(entry.3, json!({"echo": {"name": "Ada"}}));
}

#[tokio::test]
async fn handler_without_gates_is_transparent() {
    let handler: Arc<dyn SkillHandler> = Arc::new(GatedHandler::new("echo", Arc::new(Echo)));
    let out = handler.run(json!(1), &NullSink).await.unwrap();
    assert_eq!(out, json!({"echo": 1}));
}
