//! Pre-terminal gates: checks that run after a handler returns its output
//! and before the executor makes the task terminal, and that can turn a
//! successful output into a failure.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::SkillError;
use crate::handler::SkillHandler;
use crate::sink::SkillProgressSink;

/// What a gate sees: the skill id, the invocation params and the output
/// the handler produced.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct GateContext<'a> {
    pub skill_id: &'a str,
    pub params: &'a Value,
    pub output: &'a Value,
}

/// A check that can veto a successful skill output.
///
/// `Ok(())` lets the output through. `Err` replaces it: the executor
/// records the error as it would any handler failure, and a
/// `TerminalHook` observes it as `SkillOutcome::Failure`. A veto is
/// `SkillError::Rejected` naming the gate; `Internal` is for the gate's
/// own failure (for example an unreachable dependency), so a refusal is
/// never mistaken for a fault.
///
/// A gate sees only the output the handler returns, not artifacts the
/// handler streamed through its sink. A handler whose result must pass a
/// gate returns what the gate checks. Gates receive no sink: they
/// decide, they do not emit events.
#[async_trait]
pub trait PreTerminalGate: Send + Sync {
    async fn check(&self, ctx: GateContext<'_>) -> Result<(), SkillError>;
}

/// A `SkillHandler` that runs its gates on the inner handler's output.
///
/// The inner handler runs first; its error is returned unchanged and no
/// gate runs. On success the gates run in the order they were added, and
/// the first error stops the rest and becomes the result. Register it
/// like any other handler.
pub struct GatedHandler {
    skill_id: String,
    inner: Arc<dyn SkillHandler>,
    gates: Vec<Arc<dyn PreTerminalGate>>,
}

impl GatedHandler {
    pub fn new(skill_id: impl Into<String>, inner: Arc<dyn SkillHandler>) -> Self {
        Self {
            skill_id: skill_id.into(),
            inner,
            gates: Vec::new(),
        }
    }

    /// Adds a gate after those already added.
    pub fn gate(mut self, gate: Arc<dyn PreTerminalGate>) -> Self {
        self.gates.push(gate);
        self
    }
}

#[async_trait]
impl SkillHandler for GatedHandler {
    async fn run(&self, params: Value, sink: &dyn SkillProgressSink) -> Result<Value, SkillError> {
        let output = self.inner.run(params.clone(), sink).await?;
        for gate in &self.gates {
            gate.check(GateContext {
                skill_id: &self.skill_id,
                params: &params,
                output: &output,
            })
            .await?;
        }
        Ok(output)
    }
}

const _: fn() = || {
    fn assert<T: ?Sized + PreTerminalGate>() {}
    assert::<dyn PreTerminalGate>();
};
