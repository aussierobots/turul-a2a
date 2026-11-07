//! `SkillError` has exactly three variants: `InvalidRequest(String)`,
//! `Internal(String)` and `Rejected { gate, reason }`.
//!
//! The enum is `#[non_exhaustive]`, so a consumer crate such as this test
//! binary must carry a wildcard arm and cannot detect an added variant at
//! compile time. The fixed-variant-set guard is the wildcard-free match in
//! the crate's own `error` module tests; this file checks the consumer
//! view only.

use turul_a2a_patterns::SkillError;

/// Every variant is constructible and carries its payload through
/// `Display`.
#[test]
fn skill_error_constructs_every_variant() {
    let invalid = SkillError::InvalidRequest("bad".into());
    let internal = SkillError::Internal("boom".into());
    assert_eq!(invalid.to_string(), "invalid request: bad");
    assert_eq!(internal.to_string(), "internal: boom");
    let rejected = SkillError::Rejected {
        gate: "output-contract".into(),
        reason: "missing field".into(),
    };
    assert_eq!(
        rejected.to_string(),
        "rejected by output-contract: missing field"
    );
}
