//! Object-safe `SkillHandler` trait — the per-skill execution entrypoint
//! invoked by a registry. Handlers receive a `&dyn SkillProgressSink` so
//! registries can store boxed handlers without per-call generics.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::SkillError;
use crate::sink::SkillProgressSink;

/// A handler that runs when a skill is invoked.
///
/// Object-safe: `SkillHandler::run` takes `&dyn SkillProgressSink`, not a
/// generic parameter, so registries can store boxed handlers.
#[async_trait]
pub trait SkillHandler: Send + Sync {
    async fn run(&self, params: Value, sink: &dyn SkillProgressSink) -> Result<Value, SkillError>;
}
