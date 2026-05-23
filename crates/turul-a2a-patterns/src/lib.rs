//! `turul-a2a-patterns` defines reusable abstractions for A2A skill
//! authoring — the `SkillHandler` trait, `SkillRegistry`, `SkillCard`
//! (SKILL.md) helpers, and the `SkillProgressSink` trait. It depends only
//! on `turul-a2a-proto` and `turul-a2a-types`, never on the server
//! runtime. It does NOT change the A2A wire contract; profile/extension
//! machinery lives elsewhere.
//!
//! Public surface (all re-exported from the crate root):
//!
//! - Errors: [`SkillError`], [`SinkError`], [`ManifestError`],
//!   [`RenderError`], [`ValidationError`].
//! - Skill handler trait: [`SkillHandler`] taking a
//!   [`SkillProgressSink`] for non-terminal status / artifact emits.
//! - Progress sink: [`SkillProgressSink`] + [`ProgressState`]
//!   (non-terminal states only; terminals come from the handler's
//!   return value).
//! - Terminal hook: [`TerminalHook`] + [`SkillOutcome`].
//! - SKILL.md manifest: [`SkillCard`] (parse, validate_input,
//!   validate_output, render_prompt, to_agent_skill) +
//!   [`ExecutionHints`].
//! - Registry: [`SkillRegistry`] trait + [`InMemorySkillRegistry`] +
//!   [`SkillDescriptor`].
//! - Schema validation: [`validate_json`] (JSON Schema 2020-12
//!   strict-keyword check).
//!
//! Internal modules (`error`, `sink`, `handler`, `hook`, `manifest`,
//! `template`, `schema`, `registry`) are implementation detail; use
//! the flat re-exports above.

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
