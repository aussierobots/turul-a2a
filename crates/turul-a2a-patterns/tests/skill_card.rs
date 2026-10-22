//! SKILL.md manifest support.
//!
//! Covers the four helpers (parsing, AgentSkill generation, prompt
//! rendering, schema validation). Errors are asserted on their
//! structured form (variant + location), never on message strings.

use serde_json::json;

use turul_a2a_patterns::{ManifestError, RenderError, SkillCard, ValidationError};

const VALID_MANIFEST: &str = r#"---
id: greet
name: Greet
description: Say hello to a named user
tags: [demo]
examples: ["hello, world"]
inputModes: [text/plain]
outputModes: [text/plain]
securityRequirements: [bearer, apiKey]
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

/// Build a manifest from a frontmatter tail and a body.
fn manifest(extra_frontmatter: &str, body: &str) -> String {
    format!("---\nid: s\nname: S\ndescription: d\n{extra_frontmatter}---\n{body}")
}

fn assert_parse_error(err: ManifestError, expected_location: &str) {
    match err {
        ManifestError::Parse { location, .. } => assert_eq!(location, expected_location),
        other => panic!("expected ManifestError::Parse at {expected_location}, got {other:?}"),
    }
}

fn assert_schema_error(err: ManifestError, expected_location: &str) {
    match err {
        ManifestError::Schema { location, .. } => assert_eq!(location, expected_location),
        other => panic!("expected ManifestError::Schema at {expected_location}, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------

#[test]
fn parse_valid_manifest_yields_typed_card() {
    let card = SkillCard::parse(VALID_MANIFEST).expect("manifest must parse");
    assert_eq!(card.id, "greet");
    assert_eq!(card.name, "Greet");
    assert_eq!(card.input_modes, vec!["text/plain".to_string()]);
    assert_eq!(card.body, "Say hello to {{ user.name }}.\n");
    assert!(card.execution_hints.is_none());
    assert!(card.provider_config.is_none());
}

/// Snake_case frontmatter must NOT be silently accepted (camelCase only);
/// the rejection is a structured parse error located at the frontmatter.
#[test]
fn parse_rejects_snake_case_frontmatter() {
    let snake = manifest("input_modes: [text/plain]\n", "body\n");
    let err = SkillCard::parse(&snake).expect_err("snake_case must be rejected");
    assert_parse_error(err, "frontmatter");
}

#[test]
fn parse_rejects_document_without_frontmatter_delimiters() {
    let err = SkillCard::parse("id: greet\nname: Greet\n").expect_err("no delimiters");
    assert_parse_error(err, "document");
}

#[test]
fn parse_rejects_unclosed_frontmatter() {
    let err = SkillCard::parse("---\nid: greet\nname: Greet\n").expect_err("no closing ---");
    assert_parse_error(err, "document");
}

#[test]
fn parse_reads_execution_hints_and_opaque_provider_config() {
    let text = manifest(
        "executionHints:\n  maxTokens: 256\n  temperature: 0.5\n  topP: 0.25\n\
         providerConfig:\n  model: some-model\n  vendorKnob: [1, 2]\n",
        "body\n",
    );
    let card = SkillCard::parse(&text).expect("must parse");
    let hints = card.execution_hints.expect("hints present");
    assert_eq!(hints.max_tokens, Some(256));
    assert_eq!(hints.temperature, Some(0.5));
    assert_eq!(hints.top_p, Some(0.25));
    assert_eq!(
        card.provider_config,
        Some(json!({"model": "some-model", "vendorKnob": [1, 2]}))
    );
}

#[test]
fn parse_rejects_unknown_execution_hint() {
    let text = manifest("executionHints:\n  maxTokens: 1\n  seed: 7\n", "body\n");
    let err = SkillCard::parse(&text).expect_err("unknown hint must be rejected");
    assert_parse_error(err, "frontmatter");
}

#[test]
fn parse_rejects_unsupported_keyword_in_input_schema() {
    let text = manifest(
        "inputSchema:\n  type: object\n  customExt: true\n",
        "body\n",
    );
    let err = SkillCard::parse(&text).expect_err("unknown keyword must be rejected");
    assert_schema_error(err, "/inputSchema");
}

#[test]
fn parse_rejects_uncompilable_output_schema() {
    let text = manifest("outputSchema:\n  type: banana\n", "body\n");
    let err = SkillCard::parse(&text).expect_err("invalid schema must be rejected");
    assert_schema_error(err, "/outputSchema");
}

// ---------------------------------------------------------------------
// AgentSkill projection
// ---------------------------------------------------------------------

/// Every `AgentSkill` field is projected; each `securityRequirements`
/// scheme name becomes one `SecurityRequirement` naming that scheme with
/// an empty scope list.
#[test]
fn to_agent_skill_projects_all_eight_discovery_fields() {
    let card = SkillCard::parse(VALID_MANIFEST).unwrap();
    let skill = card.to_agent_skill();
    assert_eq!(skill.id, "greet");
    assert_eq!(skill.name, "Greet");
    assert_eq!(skill.description, "Say hello to a named user");
    assert_eq!(skill.tags, vec!["demo".to_string()]);
    assert_eq!(skill.examples, vec!["hello, world".to_string()]);
    assert_eq!(skill.input_modes, vec!["text/plain".to_string()]);
    assert_eq!(skill.output_modes, vec!["text/plain".to_string()]);

    assert_eq!(skill.security_requirements.len(), 2);
    for (req, scheme) in skill.security_requirements.iter().zip(["bearer", "apiKey"]) {
        assert_eq!(req.schemes.len(), 1, "one scheme per requirement");
        let scopes = req
            .schemes
            .get(scheme)
            .unwrap_or_else(|| panic!("requirement must name `{scheme}`"));
        assert!(scopes.list.is_empty(), "scopes default to empty");
    }
}

#[test]
fn to_agent_skill_with_no_security_requirements_projects_empty() {
    let card = SkillCard::parse(&manifest("", "body\n")).unwrap();
    assert!(card.to_agent_skill().security_requirements.is_empty());
}

// ---------------------------------------------------------------------
// Prompt rendering
// ---------------------------------------------------------------------

#[test]
fn render_prompt_substitutes_dotted_path() {
    let card = SkillCard::parse(VALID_MANIFEST).unwrap();
    let rendered = card
        .render_prompt(&json!({"user": {"name": "Ada"}}))
        .expect("render must succeed");
    assert_eq!(rendered, "Say hello to Ada.\n");
}

/// Missing variables produce a structured `MissingVariable` error — never
/// a silent empty substitution.
#[test]
fn render_prompt_missing_variable_is_structured_error() {
    let card = SkillCard::parse(VALID_MANIFEST).unwrap();
    let err = card
        .render_prompt(&json!({}))
        .expect_err("missing var must fail");
    match err {
        RenderError::MissingVariable { path, offset } => {
            assert_eq!(path, "user.name");
            assert_eq!(offset, "Say hello to ".len());
        }
        other => panic!("expected MissingVariable, got {other:?}"),
    }
}

/// `\{{` renders as a literal `{{` and the text after it is not
/// interpreted as an expression.
#[test]
fn render_prompt_escaped_double_brace_is_literal() {
    let card = SkillCard::parse(&manifest("", r"Use \{{ name }} for {{ name }}.")).unwrap();
    let rendered = card.render_prompt(&json!({"name": "Ada"})).unwrap();
    assert_eq!(rendered, "Use {{ name }} for Ada.");
}

#[test]
fn render_prompt_unterminated_expression_is_syntax_error() {
    let card = SkillCard::parse(&manifest("", "Hi {{ name")).unwrap();
    match card.render_prompt(&json!({"name": "Ada"})) {
        Err(RenderError::Syntax { offset, .. }) => assert_eq!(offset, 3),
        other => panic!("expected Syntax, got {other:?}"),
    }
}

#[test]
fn render_prompt_empty_expression_is_syntax_error() {
    let card = SkillCard::parse(&manifest("", "Hi {{  }}")).unwrap();
    match card.render_prompt(&json!({})) {
        Err(RenderError::Syntax { offset, .. }) => assert_eq!(offset, 3),
        other => panic!("expected Syntax, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// Schema validation
// ---------------------------------------------------------------------

#[test]
fn validate_input_accepts_conforming_payload() {
    let card = SkillCard::parse(VALID_MANIFEST).unwrap();
    card.validate_input(&json!({"user": {"name": "Ada"}}))
        .expect("conforming input must validate");
}

/// Violations are located by a JSON pointer into the instance rooted at
/// `#` — the same convention as `validate_json`.
#[test]
fn validate_input_rejects_nonconforming_payload_with_hash_rooted_location() {
    let card = SkillCard::parse(VALID_MANIFEST).unwrap();
    let err = card
        .validate_input(&json!({"user": {"name": 42}}))
        .expect_err("must reject");
    match err {
        ValidationError::Invalid { location, .. } => assert_eq!(location, "#/user/name"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn validate_output_accepts_conforming_payload() {
    let card = SkillCard::parse(VALID_MANIFEST).unwrap();
    card.validate_output(&json!({"greeting": "Hello, Ada"}))
        .expect("conforming output must validate");
}

#[test]
fn validate_output_rejects_missing_required_field() {
    let card = SkillCard::parse(VALID_MANIFEST).unwrap();
    let err = card.validate_output(&json!({})).expect_err("must reject");
    match err {
        ValidationError::Invalid { location, .. } => assert_eq!(location, "#"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn card_without_schemas_accepts_any_payload() {
    let card = SkillCard::parse(&manifest("", "body\n")).unwrap();
    card.validate_input(&json!({"anything": [1, 2]})).unwrap();
    card.validate_output(&json!("free text")).unwrap();
}
