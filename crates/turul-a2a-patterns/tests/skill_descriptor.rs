//! ADR-021 §2.2 item 4 — `SkillDescriptor.params_schema` is the single
//! source of truth: derived from the manifest's input schema for
//! manifest-backed skills, supplied once at registration for
//! programmatic skills. There is no second authoritative surface.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use turul_a2a_patterns::{
    InMemorySkillRegistry, SkillCard, SkillError, SkillHandler, SkillProgressSink, SkillRegistry,
};

const MANIFEST_WITH_SCHEMA: &str = r#"---
id: greet
name: Greet
description: hi
inputModes: [text/plain]
outputModes: [text/plain]
inputSchema:
  type: object
  properties:
    name: { type: string }
  required: [name]
---
hello {{ name }}
"#;

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

/// Positive: a manifest-backed skill's `params_schema` is exactly the
/// manifest's input schema — derived, not duplicated.
#[tokio::test]
async fn manifest_backed_descriptor_uses_manifest_input_schema() {
    let card = SkillCard::parse(MANIFEST_WITH_SCHEMA).expect("manifest must parse");
    let expected_schema = card.input_schema.clone();

    let reg = InMemorySkillRegistry::new();
    reg.register_manifest(card, Arc::new(Noop)).await.unwrap();

    let descriptor = reg.describe("greet").await.expect("registered");
    assert_eq!(
        descriptor.params_schema, expected_schema,
        "params_schema must equal the manifest input schema (single source of truth)"
    );
}

/// Negative: a programmatic skill's `params_schema` is the one supplied
/// at registration; the registry exposes it verbatim, with no second
/// override surface.
#[tokio::test]
async fn programmatic_descriptor_carries_registration_schema() {
    let reg = InMemorySkillRegistry::new();
    let supplied = json!({"type": "object", "properties": {"q": {"type": "string"}}});
    let agent_skill = turul_a2a_proto::AgentSkill {
        id: "echo".to_string(),
        name: "Echo".to_string(),
        description: "echoes the input".to_string(),
        tags: vec![],
        examples: vec![],
        input_modes: vec!["text/plain".to_string()],
        output_modes: vec!["text/plain".to_string()],
        security_requirements: vec![],
    };
    reg.register_programmatic(agent_skill, Some(supplied.clone()), Arc::new(Noop))
        .await
        .unwrap();

    let descriptor = reg.describe("echo").await.expect("registered");
    assert_eq!(descriptor.params_schema, Some(supplied));
}
