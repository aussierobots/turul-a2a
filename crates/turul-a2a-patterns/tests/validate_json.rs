//! Public `validate_json` free function.
//!
//! `SkillCard::parse` is the only way to obtain a `SkillCard`, so
//! validating an arbitrary `(schema, instance)` pair through the card
//! would need a SKILL.md document. The free function exposes the same
//! JSON Schema 2020-12 validator, with the same strict-keyword check,
//! that `SkillCard::validate_input` / `validate_output` use.

use serde_json::json;

use turul_a2a_patterns::{ValidationError, validate_json};

#[test]
fn valid_instance_against_valid_schema_returns_ok() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" }
        },
        "required": ["name"]
    });
    let instance = json!({ "name": "Ada" });
    validate_json(&schema, &instance).expect("must validate");
}

#[test]
fn missing_required_field_returns_invalid_with_rooted_location() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" }
        },
        "required": ["name"]
    });
    let instance = json!({});
    let err = validate_json(&schema, &instance).expect_err("must reject");
    match err {
        ValidationError::Invalid { location, .. } => assert_eq!(location, "#"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn nested_violation_location_is_instance_pointer_rooted_at_hash() {
    let schema = json!({
        "type": "object",
        "properties": {
            "user": {
                "type": "object",
                "properties": { "name": { "type": "string" } }
            }
        }
    });
    let instance = json!({ "user": { "name": 7 } });
    let err = validate_json(&schema, &instance).expect_err("must reject");
    match err {
        ValidationError::Invalid { location, .. } => assert_eq!(location, "#/user/name"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn unsupported_keyword_in_schema_is_rejected() {
    let schema = json!({
        "type": "object",
        "customExt": true,
        "properties": {
            "name": { "type": "string" }
        }
    });
    let instance = json!({ "name": "Ada" });
    let err = validate_json(&schema, &instance).expect_err("must reject unsupported keyword");
    match err {
        ValidationError::UnsupportedKeyword { keyword } => {
            assert_eq!(keyword, "customExt");
        }
        other => panic!("expected UnsupportedKeyword, got {other:?}"),
    }
}
