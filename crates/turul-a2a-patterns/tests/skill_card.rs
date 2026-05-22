//! SKILL.md manifest support.
//!
//! Covers all four helpers (parsing, AgentSkill generation, prompt
//! rendering, schema validation) with one positive and one negative
//! case each.

use serde_json::json;

use turul_a2a_patterns::SkillCard;

const VALID_MANIFEST: &str = r#"---
id: greet
name: Greet
description: Say hello to a named user
tags: [demo]
examples: ["hello, world"]
inputModes: [text/plain]
outputModes: [text/plain]
securityRequirements: []
inputSchema:
  type: object
  properties:
    user:
      type: object
      properties:
        name: { type: string }
      required: [name]
  required: [user]
outputSchema:
  type: object
  properties:
    greeting: { type: string }
  required: [greeting]
---
Say hello to {{ user.name }}.
"#;

/// Helper 1 — parsing: positive.
#[test]
fn parse_valid_manifest_yields_typed_card() {
    let card = SkillCard::parse(VALID_MANIFEST).expect("manifest must parse");
    assert_eq!(card.id, "greet");
    assert_eq!(card.name, "Greet");
    assert_eq!(card.input_modes, vec!["text/plain".to_string()]);
    assert!(card.body.contains("{{ user.name }}"));
}

/// Helper 1 — parsing: negative. Snake_case frontmatter must NOT be
/// silently accepted (camelCase only).
#[test]
fn parse_rejects_snake_case_frontmatter() {
    let snake = r#"---
id: greet
name: Greet
description: x
input_modes: [text/plain]
output_modes: [text/plain]
---
body
"#;
    let err = SkillCard::parse(snake).expect_err("snake_case must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("input_modes") || msg.contains("camelCase") || msg.contains("unknown"),
        "expected camelCase-only error, got: {msg}"
    );
}

/// Helper 2 — AgentSkill projection: positive.
#[test]
fn to_agent_skill_carries_all_eight_discovery_fields() {
    let card = SkillCard::parse(VALID_MANIFEST).unwrap();
    let skill = card.to_agent_skill();
    assert_eq!(skill.id, "greet");
    assert_eq!(skill.name, "Greet");
    assert_eq!(skill.description, "Say hello to a named user");
    assert_eq!(skill.tags, vec!["demo".to_string()]);
    assert_eq!(skill.examples, vec!["hello, world".to_string()]);
    assert_eq!(skill.input_modes, vec!["text/plain".to_string()]);
    assert_eq!(skill.output_modes, vec!["text/plain".to_string()]);
    assert!(skill.security_requirements.is_empty());
}

/// Helper 2 — AgentSkill projection: negative. The wire-discoverable
/// projection MUST NOT include `params_schema` (that field is
/// Turul-local). Compile-by-name: the proto's `AgentSkill` has no
/// schema field. This test asserts there's no runtime back-channel
/// either: schemas live on `SkillCard`, never on `AgentSkill`.
#[test]
fn to_agent_skill_does_not_advertise_input_schema() {
    let card = SkillCard::parse(VALID_MANIFEST).unwrap();
    let skill = card.to_agent_skill();
    // Re-serialise to JSON and confirm none of the schema fields leak.
    let as_json = serde_json::to_value(&skill).expect("AgentSkill must serialize");
    assert!(
        as_json.get("inputSchema").is_none(),
        "AgentSkill must not advertise inputSchema"
    );
    assert!(
        as_json.get("outputSchema").is_none(),
        "AgentSkill must not advertise outputSchema"
    );
    assert!(
        as_json.get("paramsSchema").is_none(),
        "AgentSkill must not advertise paramsSchema"
    );
}

/// Helper 3 — prompt rendering: positive (`{{ user.name }}` resolves).
#[test]
fn render_prompt_substitutes_dotted_path() {
    let card = SkillCard::parse(VALID_MANIFEST).unwrap();
    let rendered = card
        .render_prompt(&json!({"user": {"name": "Ada"}}))
        .expect("render must succeed");
    assert_eq!(rendered.trim(), "Say hello to Ada.");
}

/// Helper 3 — prompt rendering: negative. Missing variables produce
/// a structured `MissingVariable` error — never a silent empty
/// substitution.
#[test]
fn render_prompt_missing_variable_is_structured_error() {
    use turul_a2a_patterns::RenderError;
    let card = SkillCard::parse(VALID_MANIFEST).unwrap();
    let err = card
        .render_prompt(&json!({}))
        .expect_err("missing var must fail");
    match err {
        RenderError::MissingVariable { path, .. } => assert_eq!(path, "user.name"),
        other => panic!("expected MissingVariable, got {other:?}"),
    }
}

/// Helper 4 — schema validation (input): positive.
#[test]
fn validate_input_accepts_conforming_payload() {
    let card = SkillCard::parse(VALID_MANIFEST).unwrap();
    card.validate_input(&json!({"user": {"name": "Ada"}}))
        .expect("conforming input must validate");
}

/// Helper 4 — schema validation (output): negative. Missing required
/// field is a structured `ValidationError::Invalid`.
#[test]
fn validate_output_rejects_missing_required_field() {
    use turul_a2a_patterns::ValidationError;
    let card = SkillCard::parse(VALID_MANIFEST).unwrap();
    let err = card.validate_output(&json!({})).expect_err("must reject");
    match err {
        ValidationError::Invalid { .. } => {}
        other => panic!("expected Invalid, got {other:?}"),
    }
}
