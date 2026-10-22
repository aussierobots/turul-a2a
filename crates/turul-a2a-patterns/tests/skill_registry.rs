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
    assert_eq!(h.run(json!({}), &StubSink).await.unwrap(), json!({}));
}

/// Returns a fixed tag so a test can tell which handler is registered.
struct Tagged(&'static str);

#[async_trait]
impl SkillHandler for Tagged {
    async fn run(
        &self,
        _params: Value,
        _sink: &dyn SkillProgressSink,
    ) -> Result<Value, SkillError> {
        Ok(json!(self.0))
    }
}

/// An empty skill id is rejected as `InvalidRequest` and nothing is
/// registered.
#[tokio::test]
async fn empty_skill_id_is_rejected() {
    let reg = InMemorySkillRegistry::new();
    let err = reg
        .register_programmatic(make_agent_skill(""), None, Arc::new(Noop))
        .await
        .expect_err("empty id must be rejected");
    assert!(matches!(err, SkillError::InvalidRequest(_)), "got {err:?}");
    assert!(reg.list().await.is_empty());
}

/// Registering an id that is already present replaces both the
/// descriptor and the handler (last registration wins).
#[tokio::test]
async fn duplicate_skill_id_overwrites_previous_registration() {
    let reg = InMemorySkillRegistry::new();
    reg.register_programmatic(
        make_agent_skill("dup"),
        Some(json!({"type": "string"})),
        Arc::new(Tagged("first")),
    )
    .await
    .unwrap();
    reg.register_programmatic(
        make_agent_skill("dup"),
        Some(json!({"type": "object"})),
        Arc::new(Tagged("second")),
    )
    .await
    .unwrap();

    assert_eq!(reg.list().await.len(), 1);
    let descriptor = reg.describe("dup").await.unwrap();
    assert_eq!(descriptor.params_schema, Some(json!({"type": "object"})));
    let handler = reg.handler("dup").await.unwrap();
    assert_eq!(
        handler.run(json!({}), &StubSink).await.unwrap(),
        json!("second")
    );
}

/// `list` returns every registered descriptor, ordered by id.
#[tokio::test]
async fn list_returns_all_descriptors_ordered_by_id() {
    let reg = InMemorySkillRegistry::new();
    assert!(reg.list().await.is_empty());
    for id in ["beta", "alpha", "gamma"] {
        reg.register_programmatic(make_agent_skill(id), None, Arc::new(Noop))
            .await
            .unwrap();
    }
    let ids: Vec<String> = reg.list().await.into_iter().map(|d| d.id).collect();
    assert_eq!(ids, ["alpha", "beta", "gamma"]);
}
