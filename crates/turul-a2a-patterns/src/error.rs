//! Structured error surfaces shared across the patterns crate.
//!
//! `SkillError` is the handler-facing error returned by `SkillHandler::run`;
//! `SinkError` is the sink-side counterpart used by `SkillProgressSink`;
//! and `ManifestError` / `RenderError` / `ValidationError` carry structured
//! diagnostics from SKILL.md parsing, prompt rendering, and JSON Schema
//! validation respectively.

use thiserror::Error;

/// Skill error surface. Exactly three variants; richer error shapes are
/// the adopter's concern at the call site.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum SkillError {
    /// Maps to A2A InvalidRequest.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Maps to A2A Internal.
    #[error("internal: {0}")]
    Internal(String),
    /// A pre-terminal gate refused a successful output. Maps to A2A
    /// Internal, like `Internal`, but names the gate so a veto is
    /// distinguishable from a handler fault.
    #[error("rejected by {gate}: {reason}")]
    Rejected {
        /// Name of the gate that vetoed the output.
        gate: String,
        /// Why the gate refused the output.
        reason: String,
    },
}

/// Sink-side error variants.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum SinkError {
    /// The sink's underlying task is closed.
    #[error("sink closed")]
    Closed,
    /// Transient backend failure.
    #[error("sink backend: {0}")]
    Backend(String),
}

/// Structured manifest parse error.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ManifestError {
    /// The manifest is malformed or violates its metadata rules.
    #[error("manifest parse error at {location}: {reason}")]
    Parse {
        /// `metadata.<key>` of the offending entry, or `SKILL.md` when the skill fails to load.
        location: String,
        /// What is wrong at that location.
        reason: String,
    },
    /// A declared input or output schema is unsupported or does not compile.
    #[error("schema validation error at {location}: {reason}")]
    Schema {
        /// `metadata.<key>` naming the offending schema asset.
        location: String,
        /// Why the schema was refused.
        reason: String,
    },
}

/// Structured template render error.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum RenderError {
    /// A placeholder names a parameter that the params do not contain.
    #[error("missing template variable `{path}` at offset {offset}")]
    MissingVariable {
        /// Dotted path of the missing variable.
        path: String,
        /// Byte offset of the placeholder in the template.
        offset: usize,
    },
    /// The template has a malformed placeholder: an unclosed `{{` or an empty expression.
    #[error("template syntax error at offset {offset}: {reason}")]
    Syntax {
        /// Byte offset of the malformed placeholder in the template.
        offset: usize,
        /// What is malformed.
        reason: String,
    },
}

/// Structured I/O schema validation error.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ValidationError {
    /// A schema does not compile, or a value does not satisfy its schema.
    #[error("schema validation failed at {location}: {reason}")]
    Invalid {
        /// JSON pointer into the instance rooted at `#`; `#` when the schema itself does not compile.
        location: String,
        /// Why validation failed.
        reason: String,
    },
    /// A schema uses a keyword outside the supported JSON Schema subset.
    #[error("unsupported JSON Schema keyword `{keyword}`")]
    UnsupportedKeyword {
        /// The unsupported keyword.
        keyword: String,
    },
}

#[cfg(test)]
mod tests {
    use super::SkillError;

    /// `SkillError` has exactly three variants. Inside the defining crate
    /// `#[non_exhaustive]` does not force a wildcard arm, so this match
    /// has none: adding a variant fails to compile here until the
    /// binding contract is deliberately amended.
    #[test]
    fn skill_error_variant_set_is_fixed() {
        fn label(e: &SkillError) -> &'static str {
            match e {
                SkillError::InvalidRequest(_) => "invalid-request",
                SkillError::Internal(_) => "internal",
                SkillError::Rejected { .. } => "rejected",
            }
        }
        assert_eq!(
            label(&SkillError::InvalidRequest("x".into())),
            "invalid-request"
        );
        assert_eq!(label(&SkillError::Internal("x".into())), "internal");
        assert_eq!(
            label(&SkillError::Rejected {
                gate: "g".into(),
                reason: "r".into()
            }),
            "rejected"
        );
    }
}
