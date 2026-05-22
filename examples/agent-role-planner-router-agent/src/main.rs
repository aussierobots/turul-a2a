//! Generic A2A agent demonstrating the **planner + router** role idioms
//! with deterministic, non-LLM logic.
//!
//! Shape:
//! 1. Two manifest-backed skills are registered in an
//!    `InMemorySkillRegistry` via `SkillCard::parse` + `register_manifest`
//!    (ADR-021 §2.2 item 3):
//!    - `add` — sums two numbers (`skills/add/SKILL.md`).
//!    - `concat` — joins an array of strings (`skills/concat/SKILL.md`).
//! 2. A small **planner** inspects inbound text and decomposes it into a
//!    `(skill_id, params)` plan using a fixed rules table. The planner is
//!    code-first by design — it has no input/output schema, so a SKILL.md
//!    manifest is the wrong shape for it (see README).
//! 3. A **router** invokes the chosen skill via the registry, bridging the
//!    framework `EventSink` through an example-owned newtype that
//!    implements `SkillProgressSink` (ADR-021 §2.3).
//! 4. The skill's structured JSON output is emitted as an artifact.
//!
//! No LLM, no network egress, no proprietary surfaces. Offline by default
//! so `cargo test` is hermetic.
//!
//! Run:
//!   cargo run -p agent-role-planner-router-agent

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use turul_a2a::A2aServer;
use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::event_sink::EventSink;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a_patterns::{
    InMemorySkillRegistry, ProgressState, SinkError, SkillCard, SkillError, SkillHandler,
    SkillProgressSink, SkillRegistry,
};
use turul_a2a_types::{Artifact, Message, Part, Task};

const DEFAULT_BIND_PORT: u16 = 3012;

// SKILL.md manifests shipped with the example. Embedded so the binary is
// self-contained; the source files under `skills/<id>/SKILL.md` are the
// canonical reference for adopters.
const ADD_MANIFEST: &str = include_str!("../skills/add/SKILL.md");
const CONCAT_MANIFEST: &str = include_str!("../skills/concat/SKILL.md");

// ---------------------------------------------------------------------------
// EventSink bridge (ADR-021 §2.3). Local newtype so we can `impl
// SkillProgressSink` without violating Rust's orphan rule.
// ---------------------------------------------------------------------------

struct ExampleProgressSink(EventSink);

#[async_trait]
impl SkillProgressSink for ExampleProgressSink {
    async fn set_status(
        &self,
        state: ProgressState,
        message: Option<Message>,
    ) -> Result<(), SinkError> {
        let task_state = match state {
            ProgressState::Working => turul_a2a_types::TaskState::Working,
            ProgressState::InputRequired => turul_a2a_types::TaskState::InputRequired,
            ProgressState::AuthRequired => turul_a2a_types::TaskState::AuthRequired,
            _ => turul_a2a_types::TaskState::Working,
        };
        self.0
            .set_status(task_state, message)
            .await
            .map(|_seq| ())
            .map_err(map_event_sink_error)
    }

    async fn emit_artifact(
        &self,
        artifact: Artifact,
        append: bool,
        last_chunk: bool,
    ) -> Result<(), SinkError> {
        self.0
            .emit_artifact(artifact, append, last_chunk)
            .await
            .map(|_seq| ())
            .map_err(map_event_sink_error)
    }

    fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}

/// Maps `turul_a2a::A2aError` from the framework `EventSink` into
/// `turul_a2a_patterns::SinkError`. This adapter belongs in the bridge
/// layer (example-local), NOT in `turul-a2a-patterns/src/error.rs` —
/// the patterns crate doesn't depend on `turul-a2a` and so cannot see
/// `A2aError`. Typed match on `InvalidRequest { message }` + a literal
/// "EventSink is closed" prefix is the least-brittle way to detect the
/// closed-sink race today.
///
/// TODO: when the patterns crate becomes publishable, `turul-a2a` should
/// gain a direct `impl SkillProgressSink for EventSink` and adopters
/// will no longer need this mapper. Until then, every newtype bridge
/// across the `examples/` tree carries its own copy. The framework-side
/// fix is to expose a typed close-state on `A2aError` (or a dedicated
/// `EventSinkError`) so adapters don't need to pattern-match on a
/// message prefix.
fn map_event_sink_error(err: A2aError) -> SinkError {
    match err {
        A2aError::InvalidRequest { message } if message.starts_with("EventSink is closed") => {
            SinkError::Closed
        }
        other => SinkError::Backend(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Skill: add — sums two numbers from {a: number, b: number}. Manifest-backed
// validation runs before the deterministic body.
// ---------------------------------------------------------------------------

struct AddSkill {
    card: SkillCard,
}

#[async_trait]
impl SkillHandler for AddSkill {
    async fn run(&self, params: Value, sink: &dyn SkillProgressSink) -> Result<Value, SkillError> {
        self.card
            .validate_input(&params)
            .map_err(|e| SkillError::InvalidRequest(format!("inputSchema violation: {e}")))?;

        let a = params
            .get("a")
            .and_then(Value::as_f64)
            .ok_or_else(|| SkillError::InvalidRequest("missing or non-numeric `a`".into()))?;
        let b = params
            .get("b")
            .and_then(Value::as_f64)
            .ok_or_else(|| SkillError::InvalidRequest("missing or non-numeric `b`".into()))?;

        let _ = sink.set_status(ProgressState::Working, None).await;

        let sum = a + b;
        // Prefer integer output when both inputs were integral so JSON
        // consumers see `8` instead of `8.0` for the canonical "add 3 5".
        let result = if a.fract() == 0.0 && b.fract() == 0.0 {
            json!({ "result": sum as i64 })
        } else {
            json!({ "result": sum })
        };

        self.card
            .validate_output(&result)
            .map_err(|e| SkillError::Internal(format!("outputSchema violation: {e}")))?;

        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Skill: concat — joins an array of strings.
// ---------------------------------------------------------------------------

struct ConcatSkill {
    card: SkillCard,
}

#[async_trait]
impl SkillHandler for ConcatSkill {
    async fn run(&self, params: Value, sink: &dyn SkillProgressSink) -> Result<Value, SkillError> {
        self.card
            .validate_input(&params)
            .map_err(|e| SkillError::InvalidRequest(format!("inputSchema violation: {e}")))?;

        let arr = params
            .get("strings")
            .and_then(Value::as_array)
            .ok_or_else(|| SkillError::InvalidRequest("missing array `strings`".into()))?;
        let mut parts = Vec::with_capacity(arr.len());
        for (i, v) in arr.iter().enumerate() {
            let s = v.as_str().ok_or_else(|| {
                SkillError::InvalidRequest(format!("`strings[{i}]` is not a string"))
            })?;
            parts.push(s.to_string());
        }

        let _ = sink.set_status(ProgressState::Working, None).await;

        let output = json!({ "joined": parts.join(" ") });

        self.card
            .validate_output(&output)
            .map_err(|e| SkillError::Internal(format!("outputSchema violation: {e}")))?;

        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// Planner — deterministic rules: inbound text → (skill_id, params).
//
// The planner stays code-first: it has no input/output schema (the input is
// raw user text, the output is a routing decision), so SKILL.md is the wrong
// shape for it. The dispatch *target* is what's manifest-backed.
// ---------------------------------------------------------------------------

/// A single concrete plan step. The planner emits exactly one of these per
/// inbound message in this example. A multi-step planner would emit
/// `Vec<PlanStep>` and the router would invoke each in order.
#[derive(Debug, Clone)]
struct PlanStep {
    skill_id: String,
    params: Value,
}

/// Rules table — each variant maps an input pattern to a skill invocation.
/// The variants are listed in priority order; first match wins.
fn plan(text: &str) -> PlanStep {
    let trimmed = text.trim();

    // Rule 1: leading "add " keyword followed by two numbers.
    if let Some(rest) = trimmed.strip_prefix("add ")
        && let Some((a, b)) = parse_two_numbers_ws(rest)
    {
        return PlanStep {
            skill_id: "add".to_string(),
            params: json!({ "a": a, "b": b }),
        };
    }

    // Rule 2: infix "<n> + <n>" anywhere in the text.
    if let Some((a, b)) = parse_infix_plus(trimmed) {
        return PlanStep {
            skill_id: "add".to_string(),
            params: json!({ "a": a, "b": b }),
        };
    }

    // Rule 3: leading "concat:" or "join:" prefix → split tail on whitespace.
    for prefix in ["concat:", "join:"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let strings: Vec<String> = rest.split_whitespace().map(|s| s.to_string()).collect();
            return PlanStep {
                skill_id: "concat".to_string(),
                params: json!({ "strings": strings }),
            };
        }
    }

    // Default: hand the unrecognised input to concat as a labelled echo so
    // the agent still produces a structured artifact instead of erroring.
    PlanStep {
        skill_id: "concat".to_string(),
        params: json!({
            "strings": ["unrecognized:", trimmed]
        }),
    }
}

/// Parse two whitespace-separated numbers ("3 5" → (3.0, 5.0)). Returns
/// None unless exactly two numeric tokens are present.
fn parse_two_numbers_ws(s: &str) -> Option<(f64, f64)> {
    let mut toks = s.split_whitespace();
    let a = toks.next()?.parse::<f64>().ok()?;
    let b = toks.next()?.parse::<f64>().ok()?;
    if toks.next().is_some() {
        return None;
    }
    Some((a, b))
}

/// Find a "<n> + <n>" subexpression. Hand-rolled to keep deps minimal — the
/// example deliberately avoids `regex` to stay light.
fn parse_infix_plus(s: &str) -> Option<(f64, f64)> {
    let bytes = s.as_bytes();
    let plus = s.find('+')?;
    let left = s[..plus].trim();
    let right_start = plus + 1;
    if right_start > bytes.len() {
        return None;
    }
    let right = s[right_start..].trim();
    let a = left.split_whitespace().next_back()?.parse::<f64>().ok()?;
    let b = right.split_whitespace().next()?.parse::<f64>().ok()?;
    Some((a, b))
}

// ---------------------------------------------------------------------------
// AgentExecutor — the router. Picks the planned skill and dispatches.
// ---------------------------------------------------------------------------

struct PlannerRouterExecutor {
    registry: Arc<InMemorySkillRegistry>,
    agent_card: turul_a2a_proto::AgentCard,
}

impl PlannerRouterExecutor {
    async fn build() -> Result<Self, Box<dyn std::error::Error>> {
        let add_card =
            SkillCard::parse(ADD_MANIFEST).map_err(|e| format!("parse add SKILL.md: {e}"))?;
        let concat_card =
            SkillCard::parse(CONCAT_MANIFEST).map_err(|e| format!("parse concat SKILL.md: {e}"))?;

        // Project the discovery surfaces (eight AgentSkill fields per
        // §2.2 item 4) BEFORE registration consumes the cards.
        let add_agent_skill = add_card.to_agent_skill();
        let concat_agent_skill = concat_card.to_agent_skill();

        let registry = Arc::new(InMemorySkillRegistry::new());

        let add_handler: Arc<dyn SkillHandler> = Arc::new(AddSkill {
            card: add_card.clone(),
        });
        registry
            .register_manifest(add_card, add_handler)
            .await
            .map_err(|e| format!("register add: {e}"))?;

        let concat_handler: Arc<dyn SkillHandler> = Arc::new(ConcatSkill {
            card: concat_card.clone(),
        });
        registry
            .register_manifest(concat_card, concat_handler)
            .await
            .map_err(|e| format!("register concat: {e}"))?;

        let agent_card = AgentCardBuilder::new("Planner-Router Agent", "0.1.0")
            .description(
                "Generic planner+router example: a deterministic planner picks one of two \
                 manifest-backed skills (`add`, `concat`) and the router dispatches via the \
                 SkillRegistry. No LLM, no external services.",
            )
            .url(
                format!("http://localhost:{DEFAULT_BIND_PORT}/jsonrpc"),
                "JSONRPC",
                "1.0",
            )
            .provider("turul-a2a", "https://github.com/aussierobots/turul-a2a")
            .streaming(true)
            .default_input_modes(vec!["text/plain", "application/json"])
            .default_output_modes(vec!["application/json"])
            .skill(add_agent_skill)
            .skill(concat_agent_skill)
            .build()?;

        Ok(Self {
            registry,
            agent_card,
        })
    }
}

#[async_trait]
impl AgentExecutor for PlannerRouterExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        // 1. Planner step: collapse text parts and pick a plan.
        let text = message.text_parts().join(" ");
        let step = plan(&text);
        tracing::info!(
            target: "planner-router",
            skill_id = %step.skill_id,
            "planner decided"
        );

        // 2. Router step: look up the handler via the registry and run it.
        //    This is the canonical manifest-backed dispatch path —
        //    SkillRegistry::handler(id) → SkillHandler::run(params, sink).
        let handler = self.registry.handler(&step.skill_id).await.ok_or_else(|| {
            A2aError::Internal(format!(
                "planner produced unknown skill `{}`",
                step.skill_id
            ))
        })?;

        let sink = ExampleProgressSink(ctx.events.clone());

        match handler.run(step.params, &sink).await {
            Ok(output) => {
                // Emit the structured output as the task artifact, then
                // terminate with COMPLETED. The handler itself stayed
                // pure — it returned a Value and let the executor decide
                // how to surface it.
                let artifact_id = uuid::Uuid::now_v7().to_string();
                let payload = serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string());
                let artifact = Artifact::new(artifact_id, vec![Part::text(payload)])
                    .with_name(step.skill_id.clone());
                ctx.events
                    .emit_artifact(artifact, false, true)
                    .await
                    .map_err(|e| A2aError::Internal(format!("emit_artifact: {e}")))?;
                task.complete();
                Ok(())
            }
            Err(SkillError::InvalidRequest(msg)) => Err(A2aError::InvalidRequest { message: msg }),
            Err(SkillError::Internal(msg)) => Err(A2aError::Internal(msg)),
            Err(other) => Err(A2aError::Internal(format!("unhandled SkillError: {other}"))),
        }
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        self.agent_card.clone()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let executor = PlannerRouterExecutor::build().await?;

    let port = std::env::var("A2A_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_BIND_PORT);

    let server = A2aServer::builder()
        .executor(executor)
        .bind(([0, 0, 0, 0], port))
        .build()?;

    println!("Planner-Router Agent listening on http://0.0.0.0:{port}");
    println!("Agent card: http://localhost:{port}/.well-known/agent-card.json");
    println!();
    println!("Try:");
    println!("  curl -X POST http://localhost:{port}/message:send \\");
    println!("    -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \\");
    println!(
        "    -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"add 3 5\"}}]}}}}'"
    );

    server.run().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests for the planner rules table and manifest parsing.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn plan_add_keyword() {
        let s = plan("add 3 5");
        assert_eq!(s.skill_id, "add");
        assert_eq!(s.params["a"], 3.0);
        assert_eq!(s.params["b"], 5.0);
    }

    #[test]
    fn plan_infix_plus() {
        let s = plan("what is 7 + 12 please");
        assert_eq!(s.skill_id, "add");
        assert_eq!(s.params["a"], 7.0);
        assert_eq!(s.params["b"], 12.0);
    }

    #[test]
    fn plan_concat_prefix() {
        let s = plan("concat: foo bar baz");
        assert_eq!(s.skill_id, "concat");
        assert_eq!(s.params["strings"], json!(["foo", "bar", "baz"]));
    }

    #[test]
    fn plan_join_prefix() {
        let s = plan("join: hello world");
        assert_eq!(s.skill_id, "concat");
        assert_eq!(s.params["strings"], json!(["hello", "world"]));
    }

    #[test]
    fn plan_default_falls_back_to_concat() {
        let s = plan("totally unrelated input");
        assert_eq!(s.skill_id, "concat");
        let arr = s.params["strings"].as_array().unwrap();
        assert_eq!(arr[0], "unrecognized:");
        assert_eq!(arr[1], "totally unrelated input");
    }

    #[test]
    fn add_manifest_parses() {
        let card = SkillCard::parse(ADD_MANIFEST).expect("add SKILL.md parses");
        assert_eq!(card.id, "add");
        assert!(card.input_schema.is_some());
        assert!(card.output_schema.is_some());
        assert!(card.execution_hints.is_none());
        assert!(card.provider_config.is_none());
    }

    #[test]
    fn concat_manifest_parses() {
        let card = SkillCard::parse(CONCAT_MANIFEST).expect("concat SKILL.md parses");
        assert_eq!(card.id, "concat");
        assert!(card.input_schema.is_some());
        assert!(card.output_schema.is_some());
    }
}
