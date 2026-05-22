//! Terminal observer hook fired after `SkillHandler::run` returns. Hooks
//! are best-effort observers — failures MUST NOT abort the surrounding
//! execution.

use async_trait::async_trait;

use crate::error::SkillError;

/// Outcome of a skill execution observed by a terminal hook.
#[non_exhaustive]
#[derive(Debug)]
pub enum SkillOutcome<'a> {
    /// Handler returned Ok with the structured output value.
    Success(&'a serde_json::Value),
    /// Handler returned Err with the structured error.
    Failure(&'a SkillError),
}

/// Post-execution hook fired after `SkillHandler::run` returns.
///
/// Hooks are best-effort observers — failures from `on_terminal`
/// MUST NOT abort the surrounding execution. Hook implementations
/// SHOULD bound their own work (timeouts, panic isolation) on the
/// adopter side. The patterns crate does NOT impose framework-side
/// timeout or isolation semantics in this initial version: extractor /
/// registry semantics with isolation are dispatcher-dependent and remain
/// out of scope for the patterns crate.
///
/// Implemented with `#[async_trait]` so adopter impls write standard
/// `async fn` instead of hand-rolled `Pin<Box<dyn Future>>`. The macro
/// produces an object-safe trait, verified by the const-fn assertion
/// at the bottom of this module.
#[async_trait]
pub trait TerminalHook: Send + Sync {
    async fn on_terminal<'a>(&self, skill_id: &'a str, outcome: SkillOutcome<'a>);
}

// Compile-time dyn-compatibility assertion.
const _: fn() = || {
    fn assert<T: ?Sized + TerminalHook>() {}
    assert::<dyn TerminalHook>();
};
