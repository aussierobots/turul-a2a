//! `SkillRegistry` trait + `InMemorySkillRegistry` default impl maps
//! `AgentSkill.id` → registered `SkillHandler`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use turul_a2a_patterns::{
    InMemorySkillRegistry, ProgressState, SinkError, SkillError, SkillHandler, SkillProgressSink,
    SkillRegistry,
};
use turul_a2a_proto::AgentSkill;
use turul_a2a_types::{Artifact, Message};

struct Noop;

#[async_trait]
impl SkillHandler for Noop {
    async fn run(
        &self,
        _params: Value,
        _sink: &dyn SkillProgressSink,
    ) -> Result<Value, SkillError> {
        Ok(json!({}))
    }
}

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

fn make_agent_skill(id: &str) -> AgentSkill {
    AgentSkill {
        id: id.to_string(),
        name: "test".to_string(),
        description: "test skill".to_string(),
        tags: vec![],
        examples: vec![],
        input_modes: vec!["text/plain".to_string()],
        output_modes: vec!["text/plain".to_string()],
        security_requirements: vec![],
    }
}

/// Positive: register a programmatic skill and look it up.
#[tokio::test]
async fn register_and_describe_programmatic_skill() {
    let reg = InMemorySkillRegistry::new();
    let handler: Arc<dyn SkillHandler> = Arc::new(Noop);
    reg.register_programmatic(
        make_agent_skill("greet"),
        Some(json!({"type": "object"})),
        handler,
    )
    .await
    .unwrap();
    let descriptor = reg
        .describe("greet")
        .await
        .expect("greet should be registered");
    assert_eq!(descriptor.id, "greet");
    assert_eq!(descriptor.params_schema, Some(json!({"type": "object"})));
}

/// Negative: looking up an unregistered id returns `None`.
#[tokio::test]
async fn describe_unknown_returns_none() {
    let reg = InMemorySkillRegistry::new();
    let descriptor = reg.describe("missing").await;
    assert!(descriptor.is_none());
}

/// Surface check: dispatch a handler through the registry's lookup.
#[tokio::test]
async fn handler_lookup_dispatches_through_dyn() {
    let reg = InMemorySkillRegistry::new();
    let handler: Arc<dyn SkillHandler> = Arc::new(Noop);
    reg.register_programmatic(make_agent_skill("ping"), None, handler)
        .await
        .unwrap();
    let h = reg
        .handler("ping")
        .await
        .expect("ping should be registered");
    let _ = h.run(json!({}), &StubSink).await;
}
