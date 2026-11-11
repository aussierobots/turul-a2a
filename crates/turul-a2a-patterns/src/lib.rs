//! Building blocks for skill-based A2A agents: a [`SkillHandler`] trait,
//! a [`SkillRegistry`] that maps skill ids to handlers, [`SkillCard`]
//! (a `SKILL.md` manifest projected to an A2A `AgentSkill`), and a
//! [`SkillProgressSink`] for progress updates.
//!
//! Use it when an agent exposes several named skills instead of one free-form
//! executor. The crate depends only on
//! [`turul-a2a-proto`](https://docs.rs/turul-a2a-proto) and
//! [`turul-a2a-types`](https://docs.rs/turul-a2a-types), not on the server;
//! wiring a registry into an executor is done with
//! [`turul-a2a`](https://docs.rs/turul-a2a).
//!
//! # Example
//!
//! ```toml
//! [dependencies]
//! turul-a2a-patterns = "0.2"
//! turul-a2a-proto = "0.2"
//! async-trait = "0.1"
//! serde_json = "1"
//! tokio = { version = "1", features = ["full"] }
//! ```
//!
//! ```rust
//! use std::sync::Arc;
//! use serde_json::{json, Value};
//! use turul_a2a_patterns::*;
//! use turul_a2a_proto::AgentSkill;
//!
//! struct Greet;
//!
//! #[async_trait::async_trait]
//! impl SkillHandler for Greet {
//!     async fn run(&self, params: Value, _sink: &dyn SkillProgressSink)
//!         -> Result<Value, SkillError>
//!     {
//!         let name = params["name"].as_str().unwrap_or("world");
//!         Ok(json!({ "greeting": format!("hello {name}") }))
//!     }
//! }
//!
//! # async fn run() -> Result<(), SkillError> {
//! let registry = InMemorySkillRegistry::new();
//! let skill = AgentSkill { id: "greet".into(), name: "Greet".into(), ..Default::default() };
//! registry.register_programmatic(skill, None, Arc::new(Greet)).await?;
//! assert!(registry.handler("greet").await.is_some());
//! # Ok(())
//! # }
//! ```
//!
//! # Where next
//!
//! - [`GatedHandler`] with a [`PreTerminalGate`] to veto a handler's output
//!   before the task becomes terminal
//! - [`TerminalHook`] and [`SkillOutcome`] to observe how a skill ended
//! - [`SkillCard::from_embedded`] to load a `SKILL.md` manifest, with
//!   [`validate_json`] for its input and output schemas
//! - [Examples](https://github.com/aussierobots/turul-a2a/tree/main/examples)
//!   on GitHub

mod error;
mod gate;
mod handler;
mod hook;
mod manifest;
mod registry;
mod schema;
mod sink;
mod template;

pub use error::{ManifestError, RenderError, SinkError, SkillError, ValidationError};
pub use gate::{GateContext, GatedHandler, PreTerminalGate};
pub use handler::SkillHandler;
pub use hook::{SkillOutcome, TerminalHook};
pub use manifest::{ExecutionHints, SkillCard};
pub use registry::{InMemorySkillRegistry, SkillDescriptor, SkillRegistry};
pub use schema::validate_json;
pub use sink::{ProgressState, SkillProgressSink};
