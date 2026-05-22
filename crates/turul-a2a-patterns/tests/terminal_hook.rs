//! ADR-021 §9 Q5 — terminal-hook trait (simpler-generic variant).
//!
//! The trait is dispatcher-independent: adopters invoke `on_terminal`
//! from their own dispatch code after `SkillHandler::run` returns.
//! These tests pin the trait's object-safety and confirm it observes
//! both Success and Failure outcomes with the expected skill_id.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};

use turul_a2a_patterns::{SkillError, SkillOutcome, TerminalHook};

#[derive(Debug, Clone)]
enum CapturedOutcome {
    Success(Value),
    Failure(String),
}

#[derive(Default)]
struct CapturingHook {
    seen: Mutex<Vec<(String, CapturedOutcome)>>,
}

#[async_trait]
impl TerminalHook for CapturingHook {
    async fn on_terminal<'a>(&self, skill_id: &'a str, outcome: SkillOutcome<'a>) {
        let captured = match outcome {
            SkillOutcome::Success(v) => CapturedOutcome::Success(v.clone()),
            SkillOutcome::Failure(e) => CapturedOutcome::Failure(e.to_string()),
            _ => CapturedOutcome::Failure("unknown variant".to_string()),
        };
        let id = skill_id.to_string();
        self.seen.lock().unwrap().push((id, captured));
    }
}

#[tokio::test]
async fn hook_observes_success_outcome() {
    let hook = CapturingHook::default();
    let output = json!({"ok": true});
    hook.on_terminal("greet", SkillOutcome::Success(&output))
        .await;
    let seen = hook.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "greet");
    match &seen[0].1 {
        CapturedOutcome::Success(v) => assert_eq!(v, &output),
        other => panic!("expected Success, got {other:?}"),
    }
}

#[tokio::test]
async fn hook_observes_failure_outcome() {
    let hook = CapturingHook::default();
    let err = SkillError::InvalidRequest("missing field".to_string());
    hook.on_terminal("greet", SkillOutcome::Failure(&err)).await;
    let seen = hook.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "greet");
    match &seen[0].1 {
        CapturedOutcome::Failure(msg) => {
            assert!(
                msg.contains("missing field"),
                "expected reason in error string, got: {msg}"
            );
        }
        other => panic!("expected Failure, got {other:?}"),
    }
}

/// Object-safety / dyn-compatibility assertion at test-binary scope.
/// The crate already has a `const _` assertion in `src/lib.rs`; this
/// test reinforces it from the consumer side.
#[test]
fn terminal_hook_is_dyn_compatible() {
    fn assert_dyn<T: ?Sized + TerminalHook>() {}
    assert_dyn::<dyn TerminalHook>();
    // Smoke: build a dyn reference.
    let hook: Box<dyn TerminalHook> = Box::new(CapturingHook::default());
    let _ref: &dyn TerminalHook = hook.as_ref();
}
