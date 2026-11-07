//! `SkillCard` projection of an Agent Skills `SKILL.md`.
//!
//! Covers the projection (`from_skill`, `TryFrom<&Skill>`) and the four
//! helpers (AgentSkill generation, prompt rendering, schema validation).
//! Parsing and validating the `SKILL.md` itself belongs to
//! `turul-agent-skills`. Errors are asserted on their structured form
//! (variant + location), never on message strings.

use std::collections::HashMap;
use std::path::Path;

use serde_json::json;
use turul_agent_skills::{Skill, load, load_str};

use turul_a2a_patterns::{ManifestError, RenderError, SkillCard, ValidationError};

const VALID_MANIFEST: &str = r#"---
name: greet
description: Say hello to a named user
metadata:
  a2a-examples: '["hello, world"]'
  a2a-input-modes: '["text/plain"]'
  a2a-input-schema: assets/input.schema.json
  a2a-name: Greet
  a2a-output-modes: '["text/plain"]'
  a2a-output-schema: assets/output.schema.json
  a2a-security-requirements: '["bearer","apiKey"]'
  a2a-tags: '["demo"]'
---
Say hello to {{ user.name }}.
"#;

const INPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "user": {
      "type": "object",
      "properties": { "name": { "type": "string" } },
      "required": ["name"]
    }
  },
  "required": ["user"]
}"#;

const OUTPUT_SCHEMA: &str = r#"{"type": "object", "properties": {"greeting": {"type": "string"}}, "required": ["greeting"]}"#;

fn skill(text: &str) -> Skill {
    let loaded = load_str(skill_name(text), text).expect("SKILL.md must load");
    assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    loaded.skill
}

fn skill_name(text: &str) -> &str {
    text.lines()
        .find_map(|line| line.strip_prefix("name: "))
        .expect("name line")
}

/// Project `text`, serving assets from `files` by relative path.
fn card_with(text: &str, files: &[(&str, &str)]) -> Result<SkillCard, ManifestError> {
    let files: HashMap<&str, &str> = files.iter().copied().collect();
    SkillCard::from_skill(&skill(text), |path| files.get(path).map(|s| s.to_string()))
}

fn valid_card() -> SkillCard {
    card_with(
        VALID_MANIFEST,
        &[
            ("assets/input.schema.json", INPUT_SCHEMA),
            ("assets/output.schema.json", OUTPUT_SCHEMA),
        ],
    )
    .expect("valid manifest must project")
}

/// A skill `s` with the given metadata lines and body.
fn manifest(metadata: &[(&str, &str)], body: &str) -> String {
    let mut text = String::from("---\nname: s\ndescription: d\n");
    if !metadata.is_empty() {
        text.push_str("metadata:\n");
        for (key, value) in metadata {
            text.push_str(&format!("  {key}: '{value}'\n"));
        }
    }
    text.push_str("---\n");
    text.push_str(body);
    text
}

fn card(metadata: &[(&str, &str)], body: &str) -> Result<SkillCard, ManifestError> {
    card_with(&manifest(metadata, body), &[])
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
// Projection
// ---------------------------------------------------------------------

#[test]
fn valid_skill_projects_to_typed_card() {
    let card = valid_card();
    assert_eq!(card.id, "greet");
    assert_eq!(card.name, "Greet");
    assert_eq!(card.input_modes, vec!["text/plain".to_string()]);
    assert_eq!(card.body, "Say hello to {{ user.name }}.\n");
    assert!(card.input_schema.is_some());
    assert!(card.output_schema.is_some());
    assert!(card.execution_hints.is_none());
    assert!(card.provider_config.is_none());
}

/// The spec `name` is the wire id and display name unless `a2a-id` /
/// `a2a-name` say otherwise; a wire id may hold characters a skill name
/// cannot.
#[test]
fn a2a_id_and_a2a_name_override_the_spec_name() {
    let plain = card(&[], "body\n").unwrap();
    assert_eq!((plain.id.as_str(), plain.name.as_str()), ("s", "s"));

    let named = card(&[("a2a-id", "s_wire"), ("a2a-name", "S Display")], "body\n").unwrap();
    assert_eq!(named.id, "s_wire");
    assert_eq!(named.name, "S Display");
    assert_eq!(named.to_agent_skill().id, "s_wire");
}

/// A misspelt `a2a-*` key fails instead of being ignored; keys other tools
/// own are left alone.
#[test]
fn unknown_a2a_metadata_key_is_rejected_and_foreign_keys_are_ignored() {
    let err = card(&[("a2a-inputModes", "[]")], "body\n").expect_err("unknown key");
    assert_parse_error(err, "metadata.a2a-inputModes");
    card(&[("owner", "docs")], "body\n").expect("foreign key ignored");
}

#[test]
fn list_value_that_is_not_a_json_string_array_is_rejected() {
    let err = card(&[("a2a-tags", "demo, greeting")], "body\n").expect_err("not JSON");
    assert_parse_error(err, "metadata.a2a-tags");
    let err = card(&[("a2a-examples", "[1]")], "body\n").expect_err("not strings");
    assert_parse_error(err, "metadata.a2a-examples");
}

#[test]
fn execution_hints_and_opaque_provider_config_are_read() {
    let text = manifest(
        &[
            ("a2a-max-tokens", "256"),
            ("a2a-temperature", "0.5"),
            ("a2a-top-p", "0.25"),
            ("a2a-provider-config", "assets/provider-config.json"),
        ],
        "body\n",
    );
    let card = card_with(
        &text,
        &[(
            "assets/provider-config.json",
            r#"{"model": "some-model", "vendorKnob": [1, 2]}"#,
        )],
    )
    .expect("must project");
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
fn execution_hint_that_is_not_a_number_is_rejected() {
    let err = card(&[("a2a-max-tokens", "many")], "body\n").expect_err("not a number");
    assert_parse_error(err, "metadata.a2a-max-tokens");
}

#[test]
fn missing_or_non_json_asset_is_rejected_at_its_key() {
    let path = [("a2a-input-schema", "assets/input.schema.json")];
    let err = card(&path, "body\n").expect_err("missing asset");
    assert_parse_error(err, "metadata.a2a-input-schema");

    let err = card_with(
        &manifest(&path, "body\n"),
        &[("assets/input.schema.json", "type: object")],
    )
    .expect_err("asset is not JSON");
    assert_parse_error(err, "metadata.a2a-input-schema");
}

/// Asset paths stay inside the skill directory: the reader is never asked
/// for an absolute path or one that climbs out with `..`.
#[test]
fn asset_path_outside_the_skill_directory_is_rejected_without_reading() {
    for path in [
        "../secret.json",
        "/etc/schema.json",
        "assets/../../x.json",
        "./x.json",
    ] {
        let skill = skill(&manifest(&[("a2a-output-schema", path)], "body\n"));
        let err = SkillCard::from_skill(&skill, |asked| panic!("reader called with {asked}"))
            .expect_err("path outside the skill directory");
        assert_parse_error(err, "metadata.a2a-output-schema");
    }
}

#[test]
fn unsupported_keyword_in_input_schema_is_rejected() {
    let text = manifest(&[("a2a-input-schema", "assets/in.json")], "body\n");
    let err = card_with(
        &text,
        &[("assets/in.json", r#"{"type": "object", "customExt": true}"#)],
    )
    .expect_err("unknown keyword must be rejected");
    assert_schema_error(err, "metadata.a2a-input-schema");
}

#[test]
fn uncompilable_output_schema_is_rejected() {
    let text = manifest(&[("a2a-output-schema", "assets/out.json")], "body\n");
    let err = card_with(&text, &[("assets/out.json", r#"{"type": "banana"}"#)])
        .expect_err("invalid schema must be rejected");
    assert_schema_error(err, "metadata.a2a-output-schema");
}

/// `TryFrom<&Skill>` reads assets from the directory the skill was loaded
/// from.
#[test]
fn try_from_reads_assets_beside_the_loaded_skill() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/greet");
    let loaded = load(&dir).expect("fixture loads");
    assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    let card = SkillCard::try_from(&loaded.skill).expect("fixture projects");
    assert_eq!(card.name, "Greet");
    assert_eq!(card.provider_config, Some(json!({"model": "some-model"})));
    card.validate_input(&json!({"user": {"name": "Ada"}}))
        .unwrap();
}

/// A skill loaded from text has no directory, so `TryFrom<&Skill>` finds no
/// asset file; `from_skill` with a reader is the embedded-skill path.
#[test]
fn try_from_a_skill_without_a_location_finds_no_assets() {
    let with_asset = skill(&manifest(
        &[("a2a-input-schema", "assets/in.json")],
        "body\n",
    ));
    let err = SkillCard::try_from(&with_asset).expect_err("no directory to read from");
    assert_parse_error(err, "metadata.a2a-input-schema");
    SkillCard::try_from(&skill(&manifest(&[], "body\n"))).expect("no assets needed");
}

// ---------------------------------------------------------------------
// AgentSkill projection
// ---------------------------------------------------------------------

/// Every `AgentSkill` field is projected; each `securityRequirements`
/// scheme name becomes one `SecurityRequirement` naming that scheme with
/// an empty scope list.
#[test]
fn to_agent_skill_projects_all_eight_discovery_fields() {
    let card = valid_card();
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
    let card = card(&[], "body\n").unwrap();
    assert!(card.to_agent_skill().security_requirements.is_empty());
}

// ---------------------------------------------------------------------
// Prompt rendering
// ---------------------------------------------------------------------

#[test]
fn render_prompt_substitutes_dotted_path() {
    let card = valid_card();
    let rendered = card
        .render_prompt(&json!({"user": {"name": "Ada"}}))
        .expect("render must succeed");
    assert_eq!(rendered, "Say hello to Ada.\n");
}

/// Missing variables produce a structured `MissingVariable` error — never
/// a silent empty substitution.
#[test]
fn render_prompt_missing_variable_is_structured_error() {
    let card = valid_card();
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
    let card = card(&[], r"Use \{{ name }} for {{ name }}.").unwrap();
    let rendered = card.render_prompt(&json!({"name": "Ada"})).unwrap();
    assert_eq!(rendered, "Use {{ name }} for Ada.");
}

#[test]
fn render_prompt_unterminated_expression_is_syntax_error() {
    let card = card(&[], "Hi {{ name").unwrap();
    match card.render_prompt(&json!({"name": "Ada"})) {
        Err(RenderError::Syntax { offset, .. }) => assert_eq!(offset, 3),
        other => panic!("expected Syntax, got {other:?}"),
    }
}

#[test]
fn render_prompt_empty_expression_is_syntax_error() {
    let card = card(&[], "Hi {{  }}").unwrap();
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
    let card = valid_card();
    card.validate_input(&json!({"user": {"name": "Ada"}}))
        .expect("conforming input must validate");
}

/// Violations are located by a JSON pointer into the instance rooted at
/// `#` — the same convention as `validate_json`.
#[test]
fn validate_input_rejects_nonconforming_payload_with_hash_rooted_location() {
    let card = valid_card();
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
    let card = valid_card();
    card.validate_output(&json!({"greeting": "Hello, Ada"}))
        .expect("conforming output must validate");
}

#[test]
fn validate_output_rejects_missing_required_field() {
    let card = valid_card();
    let err = card.validate_output(&json!({})).expect_err("must reject");
    match err {
        ValidationError::Invalid { location, .. } => assert_eq!(location, "#"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn card_without_schemas_accepts_any_payload() {
    let card = card(&[], "body\n").unwrap();
    card.validate_input(&json!({"anything": [1, 2]})).unwrap();
    card.validate_output(&json!("free text")).unwrap();
}

// ---------------------------------------------------------------------
// Embedded skills
// ---------------------------------------------------------------------

#[test]
fn from_embedded_projects_a_clean_skill_with_its_assets() {
    let card = SkillCard::from_embedded(
        "greet",
        VALID_MANIFEST,
        &[
            ("assets/input.schema.json", INPUT_SCHEMA),
            ("assets/output.schema.json", OUTPUT_SCHEMA),
        ],
    )
    .expect("embedded skill projects");
    assert_eq!(card.id, "greet");
    card.validate_output(&json!({"greeting": "hi"})).unwrap();
}

/// Embedded content is fixed at build time, so what `load` would only
/// report — a name that differs from the directory, an unknown field — is
/// an error.
#[test]
fn from_embedded_rejects_what_the_loader_only_reports() {
    let err = SkillCard::from_embedded("other", &manifest(&[], "body\n"), &[])
        .expect_err("name differs from directory");
    assert_parse_error(err, "SKILL.md");

    let unknown = "---\nname: s\ndescription: d\ninputModes: x\n---\n";
    let err = SkillCard::from_embedded("s", unknown, &[]).expect_err("unknown field");
    assert_parse_error(err, "SKILL.md");

    let err = SkillCard::from_embedded("s", "no frontmatter", &[]).expect_err("not a SKILL.md");
    assert_parse_error(err, "SKILL.md");
}

/// A relative asset path that is a symlink out of the skill directory is
/// refused: the file `TryFrom<&Skill>` reads must resolve inside it.
#[cfg(unix)]
#[test]
fn try_from_refuses_an_asset_symlinked_out_of_the_skill_directory() {
    let root = std::env::temp_dir().join(format!("skill-card-symlink-{}", std::process::id()));
    let dir = root.join("s");
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(root.join("outside.json"), r#"{"type": "object"}"#).unwrap();
    std::os::unix::fs::symlink(root.join("outside.json"), dir.join("assets/in.json")).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        manifest(&[("a2a-input-schema", "assets/in.json")], "body\n"),
    )
    .unwrap();

    let result = load(&dir).map(|loaded| SkillCard::try_from(&loaded.skill));
    std::fs::remove_dir_all(&root).unwrap();
    let err = result
        .expect("skill loads")
        .expect_err("symlink out is refused");
    assert_parse_error(err, "metadata.a2a-input-schema");
}
