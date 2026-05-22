//! ADR-021 §2.2 item 3 — public `validate_json` free function.
//!
//! `SkillCard` is `#[non_exhaustive]` with no public constructor, so
//! adopters cannot validate arbitrary `(schema, instance)` pairs
//! without round-tripping through `SkillCard::parse`. The free
//! function exposes the same JSON Schema 2020-12 validator with the
//! same strict-keyword check used by manifest parsing.

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
        ValidationError::Invalid { location, .. } => {
            assert!(
                location.starts_with('#'),
                "expected location rooted at `#`, got: {location}"
            );
        }
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
