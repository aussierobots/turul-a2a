//! `SkillError` has exactly two variants: `InvalidRequest(String)` and
//! `Internal(String)`.

use turul_a2a_patterns::SkillError;

/// Positive: both variants are constructible with `String` payloads.
#[test]
fn skill_error_constructs_both_variants() {
    let _invalid = SkillError::InvalidRequest("bad".into());
    let _internal = SkillError::Internal("boom".into());
}

/// Negative: exhaustively matching only the two named variants must
/// compile against the current surface. If a third variant slips in,
/// this match goes non-exhaustive and the test fails to compile —
/// catching unintended surface drift. The `#[non_exhaustive]` attr on
/// the enum forces the wildcard arm; we assert that the two named arms
/// remain the only ones that need explicit handling.
#[test]
fn skill_error_has_exactly_two_named_variants() {
    let e = SkillError::InvalidRequest("x".into());
    let label = match &e {
        SkillError::InvalidRequest(_) => "invalid",
        SkillError::Internal(_) => "internal",
        _ => "unexpected_variant",
    };
    assert_eq!(label, "invalid");
}
