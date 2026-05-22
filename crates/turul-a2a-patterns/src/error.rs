//! Structured error surfaces shared across the patterns crate.
//!
//! `SkillError` is the handler-facing error returned by `SkillHandler::run`;
//! `SinkError` is the sink-side counterpart used by `SkillProgressSink`;
//! and `ManifestError` / `RenderError` / `ValidationError` carry structured
//! diagnostics from SKILL.md parsing, prompt rendering, and JSON Schema
//! validation respectively.

use thiserror::Error;

/// Skill error surface. Exactly two variants per ADR-021 §2.2 item 5.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum SkillError {
    /// Maps to A2A InvalidRequest.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Maps to A2A Internal.
    #[error("internal: {0}")]
    Internal(String),
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
    #[error("manifest parse error at {location}: {reason}")]
    Parse { location: String, reason: String },
    #[error("schema validation error at {location}: {reason}")]
    Schema { location: String, reason: String },
}

/// Structured template render error (§2.2 item 3).
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum RenderError {
    #[error("missing template variable `{path}` at offset {offset}")]
    MissingVariable { path: String, offset: usize },
    #[error("template syntax error at offset {offset}: {reason}")]
    Syntax { offset: usize, reason: String },
}

/// Structured I/O schema validation error (§2.2 item 3).
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("schema validation failed at {location}: {reason}")]
    Invalid { location: String, reason: String },
    #[error("unsupported JSON Schema keyword `{keyword}`")]
    UnsupportedKeyword { keyword: String },
}
