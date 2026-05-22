//! Generic A2A agent demonstrating a **post-task terminal hook** firing
//! after a skill returns.
//!
//! Shape:
//! 1. Two manifest-backed skills are registered in an `InMemorySkillRegistry`:
//!    - `count` (skills/count/SKILL.md) — takes `{"n": <integer>}` and
//!      returns `{"squared": <n*n>}`.
//!    - `metrics` (skills/metrics/SKILL.md) — takes no input and returns
//!      the in-memory counter snapshot.
//! 2. A `TerminalHook` impl records each skill outcome (success / failure
//!    plus a short last-outcome summary) into a shared in-process counter.
//! 3. After `handler.run(...)` returns, the executor invokes the hook
//!    inline. The hook is best-effort: even if it errored or hung the
//!    skill response would still surface to the caller. Adopters wanting
//!    timeout / panic isolation should wrap the call in
//!    `tokio::time::timeout` or `tokio::task::spawn` — those framework-side
//!    semantics are deferred (see README).
//! 4. The `metrics` skill exposes the counter so callers can observe that
//!    the hook actually fired.
//!
//! The SKILL.md manifests are the source of truth for the advertised
//! `AgentSkill` and for input/output JSON Schema validation. The
//! `RecordingHook` is *not* a skill — it is an orthogonal observation
//! seam that fires after any skill's `run` returns.
//!
//! No LLM, no network egress, no external services. Offline by default.
//!
//! Run:
//!   cargo run -p post-task-hook-agent

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex as TokioMutex;
use turul_a2a::A2aServer;
use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::event_sink::EventSink;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a_patterns::{
    InMemorySkillRegistry, ProgressState, SinkError, SkillCard, SkillError, SkillHandler,
    SkillOutcome, SkillProgressSink, SkillRegistry, TerminalHook,
};
use turul_a2a_types::{Artifact, Message, Part, Task};

const DEFAULT_BIND_PORT: u16 = 3014;

/// `count` SKILL.md, embedded so the binary is self-contained. The source
/// file under `skills/count/SKILL.md` is the canonical reference.
const COUNT_MANIFEST: &str = include_str!("../skills/count/SKILL.md");

/// `metrics` SKILL.md, embedded for the same reason.
const METRICS_MANIFEST: &str = include_str!("../skills/metrics/SKILL.md");

// ---------------------------------------------------------------------------
// EventSink bridge. Local newtype so we can `impl SkillProgressSink`
// without violating the orphan rule.
// ---------------------------------------------------------------------------

struct ExampleProgressSink(EventSink);

#[async_trait]
impl SkillProgressSink for ExampleProgressSink {
    async fn set_status(
        &self,
        state: ProgressState,
        message: Option<Message>,
    ) -> Result<(), SinkError> {
        // ProgressState is #[non_exhaustive]; future variants fall back
        // to Working until the example is updated alongside the
        // patterns crate.
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
/// `turul_a2a_patterns::SinkError`. Lives in the bridge layer
/// (example-local), not in `turul-a2a-patterns/src/error.rs` — the
/// patterns crate doesn't depend on `turul-a2a` and so cannot see
/// `A2aError`. Typed match on `InvalidRequest { message }` + a literal
/// "EventSink is closed" prefix is the least-bad way to detect the
/// closed-sink race today.
fn map_event_sink_error(err: A2aError) -> SinkError {
    match err {
        A2aError::InvalidRequest { message } if message.starts_with("EventSink is closed") => {
            SinkError::Closed
        }
        other => SinkError::Backend(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Counter + TerminalHook impl. The counter lives in a shared `Arc` so both
// the hook and the `metrics` skill can read/write it.
// ---------------------------------------------------------------------------

struct OutcomeCounter {
    success: AtomicU64,
    failure: AtomicU64,
    last_summary: TokioMutex<Option<String>>,
}

impl OutcomeCounter {
    fn new() -> Self {
        Self {
            success: AtomicU64::new(0),
            failure: AtomicU64::new(0),
            last_summary: TokioMutex::new(None),
        }
    }

    async fn snapshot(&self) -> (u64, u64, Option<String>) {
        let s = self.success.load(Ordering::Relaxed);
        let f = self.failure.load(Ordering::Relaxed);
        let last = self.last_summary.lock().await.clone();
        (s, f, last)
    }
}

struct RecordingHook {
    counter: Arc<OutcomeCounter>,
}

#[async_trait]
impl TerminalHook for RecordingHook {
    async fn on_terminal<'a>(&self, skill_id: &'a str, outcome: SkillOutcome<'a>) {
        // Snapshot the outcome into owned data BEFORE awaiting — the
        // mutex lock is async and keeping owned strings here keeps the
        // hook impl simple.
        let summary = match &outcome {
            SkillOutcome::Success(v) => {
                self.counter.success.fetch_add(1, Ordering::Relaxed);
                format!("ok({skill_id}): {v}")
            }
            SkillOutcome::Failure(err) => {
                self.counter.failure.fetch_add(1, Ordering::Relaxed);
                format!("err({skill_id}): {err}")
            }
            // `SkillOutcome` is `#[non_exhaustive]` — future variants
            // fall through to a neutral counter increment so the hook
            // still observes the call without misclassifying it.
            _ => {
                self.counter.failure.fetch_add(1, Ordering::Relaxed);
                format!("unknown({skill_id})")
            }
        };
        // Adopter note: any panic / hang in here is the adopter's
        // problem to isolate. We deliberately do not wrap in
        // `tokio::time::timeout` so the example stays minimal — see
        // README ("Hook safety").
        let mut last = self.counter.last_summary.lock().await;
        *last = Some(summary);
    }
}

// ---------------------------------------------------------------------------
// Skill: count — squares a number. Handler stays a small Rust function;
// the manifest under skills/count/SKILL.md owns the advertised AgentSkill
// and input/output schemas.
// ---------------------------------------------------------------------------

struct CountSkill {
    card: SkillCard,
}

#[async_trait]
impl SkillHandler for CountSkill {
    async fn run(&self, params: Value, sink: &dyn SkillProgressSink) -> Result<Value, SkillError> {
        self.card
            .validate_input(&params)
            .map_err(|e| SkillError::InvalidRequest(format!("inputSchema violation: {e}")))?;

        let n = params
            .get("n")
            .and_then(Value::as_i64)
            .ok_or_else(|| SkillError::InvalidRequest("missing or non-integer `n`".into()))?;
        let _ = sink.set_status(ProgressState::Working, None).await;
        let squared = n.saturating_mul(n);
        let out = json!({ "squared": squared });

        self.card
            .validate_output(&out)
            .map_err(|e| SkillError::Internal(format!("outputSchema violation: {e}")))?;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Skill: metrics — returns the in-memory counter snapshot. Manifest is
// skills/metrics/SKILL.md.
// ---------------------------------------------------------------------------

struct MetricsSkill {
    card: SkillCard,
    counter: Arc<OutcomeCounter>,
}

#[async_trait]
impl SkillHandler for MetricsSkill {
    async fn run(&self, params: Value, _sink: &dyn SkillProgressSink) -> Result<Value, SkillError> {
        self.card
            .validate_input(&params)
            .map_err(|e| SkillError::InvalidRequest(format!("inputSchema violation: {e}")))?;

        let (s, f, last) = self.counter.snapshot().await;
        let out = json!({
            "success": s,
            "failure": f,
            "last": last,
        });

        self.card
            .validate_output(&out)
            .map_err(|e| SkillError::Internal(format!("outputSchema violation: {e}")))?;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Planner — minimal text → (skill_id, params) mapping. The agent's text
// dispatch is unchanged across the manifest refactor: the SKILL.md files
// drive the advertised schemas, this planner picks which skill to invoke.
// ---------------------------------------------------------------------------

struct PlanStep {
    skill_id: String,
    params: Value,
}

fn plan(text: &str) -> PlanStep {
    let trimmed = text.trim();

    // "metrics" (any case) → metrics skill, no params (empty object
    // matches the manifest's empty inputSchema).
    if trimmed.eq_ignore_ascii_case("metrics") {
        return PlanStep {
            skill_id: "metrics".to_string(),
            params: json!({}),
        };
    }

    // "count <token>" → count with n = parsed token (integer or string).
    // The manifest requires integer; non-integer values are forwarded
    // verbatim so the schema validator surfaces them as
    // `SkillError::InvalidRequest`, which the hook records as Failure.
    if let Some(rest) = trimmed.strip_prefix("count ") {
        let tok = rest.trim();
        let n_value: Value = match tok.parse::<i64>() {
            Ok(n) => json!(n),
            Err(_) => json!(tok),
        };
        return PlanStep {
            skill_id: "count".to_string(),
            params: json!({ "n": n_value }),
        };
    }

    // Default: unrecognised → count with a non-integer value so the
    // failure branch is exercised. Keeps the example deterministic.
    PlanStep {
        skill_id: "count".to_string(),
        params: json!({ "n": trimmed }),
    }
}

// ---------------------------------------------------------------------------
// AgentExecutor — dispatches, then invokes the terminal hook inline.
// ---------------------------------------------------------------------------

struct HookAgent {
    registry: Arc<InMemorySkillRegistry>,
    hook: Arc<dyn TerminalHook>,
    agent_card: turul_a2a_proto::AgentCard,
}

impl HookAgent {
    async fn build() -> Result<Self, Box<dyn std::error::Error>> {
        let counter = Arc::new(OutcomeCounter::new());

        let count_card = SkillCard::parse(COUNT_MANIFEST)?;
        let metrics_card = SkillCard::parse(METRICS_MANIFEST)?;
        let count_agent_skill = count_card.to_agent_skill();
        let metrics_agent_skill = metrics_card.to_agent_skill();

        let registry = Arc::new(InMemorySkillRegistry::new());
        let count_handler: Arc<dyn SkillHandler> = Arc::new(CountSkill {
            card: count_card.clone(),
        });
        registry
            .register_manifest(count_card, count_handler)
            .await
            .map_err(|e| format!("register count: {e}"))?;

        let metrics_handler: Arc<dyn SkillHandler> = Arc::new(MetricsSkill {
            card: metrics_card.clone(),
            counter: counter.clone(),
        });
        registry
            .register_manifest(metrics_card, metrics_handler)
            .await
            .map_err(|e| format!("register metrics: {e}"))?;

        let hook: Arc<dyn TerminalHook> = Arc::new(RecordingHook {
            counter: counter.clone(),
        });

        let agent_card = AgentCardBuilder::new("Post-Task Hook Agent", "0.1.0")
            .description(
                "Generic post-task terminal-hook example: every skill call \
                 fires a TerminalHook that records the outcome to an in-memory \
                 counter. The `metrics` skill exposes that counter so callers \
                 can observe the hook fired.",
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
            .skill(count_agent_skill)
            .skill(metrics_agent_skill)
            .build()?;

        Ok(Self {
            registry,
            hook,
            agent_card,
        })
    }
}

#[async_trait]
impl AgentExecutor for HookAgent {
    async fn execute(
        &self,
        task: &mut Task,
        message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let text = message.text_parts().join(" ");
        let step = plan(&text);
        tracing::info!(target: "post-task-hook", skill_id = %step.skill_id, "planner decided");

        let handler = self.registry.handler(&step.skill_id).await.ok_or_else(|| {
            A2aError::Internal(format!(
                "planner produced unknown skill `{}`",
                step.skill_id
            ))
        })?;

        let sink = ExampleProgressSink(ctx.events.clone());
        let skill_id = step.skill_id.clone();

        // 1. Run the skill.
        let result = handler.run(step.params, &sink).await;

        // 2. Fire the terminal hook with the outcome (best-effort).
        //    The hook is awaited inline — see README "Hook safety" for how
        //    a real adopter would isolate it (timeout / spawn).
        let outcome = match &result {
            Ok(v) => SkillOutcome::Success(v),
            Err(e) => SkillOutcome::Failure(e),
        };
        self.hook.on_terminal(&skill_id, outcome).await;

        // 3. Surface the result back to the caller.
        match result {
            Ok(output) => {
                let artifact_id = uuid::Uuid::now_v7().to_string();
                let payload = serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string());
                let artifact = Artifact::new(artifact_id, vec![Part::text(payload)])
                    .with_name(skill_id.clone());
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

    let executor = HookAgent::build().await?;

    let port = std::env::var("A2A_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_BIND_PORT);

    let server = A2aServer::builder()
        .executor(executor)
        .bind(([0, 0, 0, 0], port))
        .build()?;

    println!("Post-Task Hook Agent listening on http://0.0.0.0:{port}");
    println!("Agent card: http://localhost:{port}/.well-known/agent-card.json");
    println!();
    println!("Try:");
    println!("  curl -X POST http://localhost:{port}/message:send \\");
    println!("    -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \\");
    println!(
        "    -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"count 3\"}}]}}}}'"
    );
    println!("  # then:");
    println!(
        "  ... -d '{{\"message\":{{\"messageId\":\"2\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"metrics\"}}]}}}}'"
    );

    server.run().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests for the planner rules and the recording hook.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn plan_count_with_number() {
        let s = plan("count 3");
        assert_eq!(s.skill_id, "count");
        assert_eq!(s.params["n"], 3);
    }

    #[test]
    fn plan_count_with_non_number_passes_through_string() {
        let s = plan("count three");
        assert_eq!(s.skill_id, "count");
        assert_eq!(s.params["n"], "three");
    }

    #[test]
    fn plan_metrics_keyword() {
        let s = plan("metrics");
        assert_eq!(s.skill_id, "metrics");
        assert_eq!(s.params, json!({}));
    }

    #[test]
    fn count_manifest_parses() {
        let card = SkillCard::parse(COUNT_MANIFEST).expect("count manifest parses");
        assert_eq!(card.id, "count");
        assert!(card.input_schema.is_some());
        assert!(card.output_schema.is_some());
    }

    #[test]
    fn metrics_manifest_parses() {
        let card = SkillCard::parse(METRICS_MANIFEST).expect("metrics manifest parses");
        assert_eq!(card.id, "metrics");
        assert!(card.input_schema.is_some());
        assert!(card.output_schema.is_some());
    }

    #[tokio::test]
    async fn hook_records_success_and_failure() {
        let counter = Arc::new(OutcomeCounter::new());
        let hook = RecordingHook {
            counter: counter.clone(),
        };
        let ok = json!({"squared": 9});
        hook.on_terminal("count", SkillOutcome::Success(&ok)).await;
        let err = SkillError::InvalidRequest("bad input".into());
        hook.on_terminal("count", SkillOutcome::Failure(&err)).await;

        let (s, f, last) = counter.snapshot().await;
        assert_eq!(s, 1);
        assert_eq!(f, 1);
        assert!(last.unwrap().starts_with("err(count):"));
    }
}
