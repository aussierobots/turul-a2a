//! `SkillError` has exactly two variants: `InvalidRequest(String)` and
//! `Internal(String)`.
//!
//! The enum is `#[non_exhaustive]`, so a consumer crate such as this test
//! binary must carry a wildcard arm and cannot detect a third variant at
//! compile time. The fixed-variant-set guard is the wildcard-free match in
//! the crate's own `error` module tests; this file checks the consumer
//! view only.

use turul_a2a_patterns::SkillError;

/// Both variants are constructible with `String` payloads and carry the
/// payload through `Display`.
#[test]
fn skill_error_constructs_both_variants() {
    let invalid = SkillError::InvalidRequest("bad".into());
    let internal = SkillError::Internal("boom".into());
    assert_eq!(invalid.to_string(), "invalid request: bad");
    assert_eq!(internal.to_string(), "internal: boom");
}
