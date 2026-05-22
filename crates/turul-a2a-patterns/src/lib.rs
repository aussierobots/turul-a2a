//! `turul-a2a-patterns` defines reusable abstractions for A2A skill
//! authoring — the `SkillHandler` trait, `SkillRegistry`, `SkillCard`
//! (SKILL.md) helpers, and the `SkillProgressSink` trait. It depends only
//! on `turul-a2a-proto` and `turul-a2a-types`, never on the server
//! runtime. It does NOT change the A2A wire contract; profile/extension
//! machinery lives elsewhere.
//!
//! The crate is organised into focused modules: errors ([`error`]),
//! progress sink ([`sink`]), handler trait ([`handler`]), terminal hook
//! ([`hook`]), SKILL.md manifest ([`manifest`]), prompt template rendering
//! ([`template`]), JSON Schema strict validation ([`schema`]), and the
//! registry surface ([`registry`]). The flat `turul_a2a_patterns::*`
//! re-exports below preserve the original API.

mod error;
mod handler;
mod hook;
mod manifest;
mod registry;
mod schema;
mod sink;
mod template;

pub use error::{ManifestError, RenderError, SinkError, SkillError, ValidationError};
pub use handler::SkillHandler;
pub use hook::{SkillOutcome, TerminalHook};
pub use manifest::{ExecutionHints, SkillCard};
pub use registry::{InMemorySkillRegistry, SkillDescriptor, SkillRegistry};
pub use schema::validate_json;
pub use sink::{ProgressState, SkillProgressSink};
