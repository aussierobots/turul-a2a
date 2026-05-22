//! Generic A2A agent demonstrating the **critic / evaluator** role idiom
//! with deterministic, non-LLM logic.
//!
//! Shape:
//! 1. Two manifest-backed skills are registered in an `InMemorySkillRegistry`
//!    from `skills/<id>/SKILL.md`:
//!    - `validate_against_schema` — runs a JSON Schema 2020-12 validation
//!      via the public `turul_a2a_patterns::validate_json` helper.
//!    - `check_invariants` — runs a deterministic invariant table over a
//!      value: `non_empty`, `min_length`, `max_length`, `contains`.
//! 2. The executor's `dispatch` parses the inbound text into a `(skill_id,
//!    params)` tuple, then looks up the handler in the registry. Plain
//!    prose falls back to `check_invariants` against a single `non_empty`
//!    rule so the agent always produces a structured artifact.
//! 3. The skill's structured JSON output is emitted as an artifact.
//!
//! No LLM, no network egress, no proprietary surfaces. Offline by default
//! so `cargo test` is hermetic.
//!
//! Run:
//!   cargo run -p agent-role-critic-agent

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
    SkillProgressSink, SkillRegistry, validate_json,
};
use turul_a2a_types::{Artifact, Message, Part, Task};

const DEFAULT_BIND_PORT: u16 = 3013;

// Manifests embedded at build time so the binary is self-contained.
const VALIDATE_AGAINST_SCHEMA_MANIFEST: &str =
    include_str!("../skills/validate_against_schema/SKILL.md");
const CHECK_INVARIANTS_MANIFEST: &str = include_str!("../skills/check_invariants/SKILL.md");

// ---------------------------------------------------------------------------
// EventSink bridge. Local newtype so we can `impl SkillProgressSink`
// without violating Rust's orphan rule.
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

fn map_event_sink_error(err: A2aError) -> SinkError {
    match err {
        A2aError::InvalidRequest { ref message } if message.starts_with("EventSink is closed") => {
            SinkError::Closed
        }
        other => SinkError::Backend(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Skill handler: validate_against_schema
//
// Input shape (validated by the manifest input schema before reaching this
// handler if the executor opts in): { "value": <any>, "schema": <object> }.
// Output: { "valid": bool, "errors": [string] }.
// ---------------------------------------------------------------------------

struct ValidateAgainstSchemaSkill;

#[async_trait]
impl SkillHandler for ValidateAgainstSchemaSkill {
    async fn run(&self, params: Value, sink: &dyn SkillProgressSink) -> Result<Value, SkillError> {
        let value = params
            .get("value")
            .ok_or_else(|| SkillError::InvalidRequest("missing `value`".into()))?;
        let schema = params
            .get("schema")
            .ok_or_else(|| SkillError::InvalidRequest("missing `schema`".into()))?;

        let _ = sink.set_status(ProgressState::Working, None).await;

        match validate_json(schema, value) {
            Ok(()) => Ok(json!({ "valid": true, "errors": [] })),
            Err(err) => Ok(json!({
                "valid": false,
                "errors": [err.to_string()],
            })),
        }
    }
}

// ---------------------------------------------------------------------------
// Skill handler: check_invariants
//
// Input shape: { "value": <any>,
//                "invariants": [{"name": <string>, "check": <kind>, "args"?: <object>}] }
// Output:      { "verdict": "pass" | "fail",
//                "failures": [{"name": <string>, "reason": <string>}] }
// ---------------------------------------------------------------------------

struct CheckInvariantsSkill;

#[async_trait]
impl SkillHandler for CheckInvariantsSkill {
    async fn run(&self, params: Value, sink: &dyn SkillProgressSink) -> Result<Value, SkillError> {
        let value = params
            .get("value")
            .ok_or_else(|| SkillError::InvalidRequest("missing `value`".into()))?;
        let invariants = params
            .get("invariants")
            .and_then(Value::as_array)
            .ok_or_else(|| SkillError::InvalidRequest("missing array `invariants`".into()))?;

        let _ = sink.set_status(ProgressState::Working, None).await;

        let mut failures = Vec::new();
        for (i, inv) in invariants.iter().enumerate() {
            let name = inv
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    SkillError::InvalidRequest(format!("`invariants[{i}].name` missing"))
                })?
                .to_string();
            let kind = inv.get("check").and_then(Value::as_str).ok_or_else(|| {
                SkillError::InvalidRequest(format!("`invariants[{i}].check` missing"))
            })?;
            let args = inv.get("args").cloned().unwrap_or(Value::Null);

            if let Err(reason) = run_invariant(kind, value, &args) {
                failures.push(json!({ "name": name, "reason": reason }));
            }
        }

        let verdict = if failures.is_empty() { "pass" } else { "fail" };
        Ok(json!({ "verdict": verdict, "failures": failures }))
    }
}

/// Run a single invariant. Returns `Ok(())` on pass, `Err(reason)` on fail.
/// `InvalidRequest`-flavoured user errors (e.g. unknown `check` kind, missing
/// `args` fields) are surfaced as a failure reason rather than aborting the
/// whole call, so a malformed entry shows up as a normal failure in the
/// output rather than tanking sibling invariants. This matches the
/// "critic always produces a verdict" contract.
fn run_invariant(kind: &str, value: &Value, args: &Value) -> Result<(), String> {
    match kind {
        "non_empty" => check_non_empty(value),
        "min_length" => {
            let min = args
                .get("min")
                .and_then(Value::as_u64)
                .ok_or_else(|| "`args.min` missing or non-numeric".to_string())?;
            check_min_length(value, min as usize)
        }
        "max_length" => {
            let max = args
                .get("max")
                .and_then(Value::as_u64)
                .ok_or_else(|| "`args.max` missing or non-numeric".to_string())?;
            check_max_length(value, max as usize)
        }
        "contains" => {
            let needle = args
                .get("needle")
                .ok_or_else(|| "`args.needle` missing".to_string())?;
            check_contains(value, needle)
        }
        other => Err(format!("unknown invariant `{other}`")),
    }
}

fn check_non_empty(value: &Value) -> Result<(), String> {
    match value {
        Value::Null => Err("value is null".to_string()),
        Value::String(s) if s.is_empty() => Err("string is empty".to_string()),
        Value::Array(a) if a.is_empty() => Err("array is empty".to_string()),
        Value::Object(o) if o.is_empty() => Err("object is empty".to_string()),
        _ => Ok(()),
    }
}

fn check_min_length(value: &Value, min: usize) -> Result<(), String> {
    let len = json_len(value)
        .ok_or_else(|| "min_length only applies to strings or arrays".to_string())?;
    if len < min {
        Err(format!("length {len} is below minimum {min}"))
    } else {
        Ok(())
    }
}

fn check_max_length(value: &Value, max: usize) -> Result<(), String> {
    let len = json_len(value)
        .ok_or_else(|| "max_length only applies to strings or arrays".to_string())?;
    if len > max {
        Err(format!("length {len} exceeds maximum {max}"))
    } else {
        Ok(())
    }
}

fn check_contains(value: &Value, needle: &Value) -> Result<(), String> {
    match (value, needle) {
        (Value::String(s), Value::String(n)) => {
            if s.contains(n.as_str()) {
                Ok(())
            } else {
                Err(format!("string does not contain `{n}`"))
            }
        }
        (Value::Array(arr), _) => {
            if arr.iter().any(|v| v == needle) {
                Ok(())
            } else {
                Err("array does not contain needle".to_string())
            }
        }
        (Value::String(_), _) => {
            Err("contains needle must be a string for string targets".to_string())
        }
        _ => Err("contains only applies to strings or arrays".to_string()),
    }
}

/// Length helper: strings ⇒ char count, arrays ⇒ element count. Other JSON
/// kinds have no length and return `None`.
fn json_len(value: &Value) -> Option<usize> {
    match value {
        Value::String(s) => Some(s.chars().count()),
        Value::Array(a) => Some(a.len()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// AgentExecutor — dispatches based on the `kind` field of the inbound JSON.
// ---------------------------------------------------------------------------

struct CriticExecutor {
    registry: Arc<InMemorySkillRegistry>,
    agent_card: turul_a2a_proto::AgentCard,
}

impl CriticExecutor {
    async fn build() -> Result<Self, Box<dyn std::error::Error>> {
        let registry = Arc::new(InMemorySkillRegistry::new());

        // Manifest-backed registration. The manifest is the single source of
        // truth for the AgentSkill projection and the params schema.
        let validate_card = SkillCard::parse(VALIDATE_AGAINST_SCHEMA_MANIFEST)
            .map_err(|e| format!("parse validate_against_schema SKILL.md: {e}"))?;
        let validate_skill = validate_card.to_agent_skill();
        let validate_handler: Arc<dyn SkillHandler> = Arc::new(ValidateAgainstSchemaSkill);
        registry
            .register_manifest(validate_card, validate_handler)
            .await
            .map_err(|e| format!("register validate_against_schema: {e}"))?;

        let invariants_card = SkillCard::parse(CHECK_INVARIANTS_MANIFEST)
            .map_err(|e| format!("parse check_invariants SKILL.md: {e}"))?;
        let invariants_skill = invariants_card.to_agent_skill();
        let invariants_handler: Arc<dyn SkillHandler> = Arc::new(CheckInvariantsSkill);
        registry
            .register_manifest(invariants_card, invariants_handler)
            .await
            .map_err(|e| format!("register check_invariants: {e}"))?;

        let agent_card = AgentCardBuilder::new("Critic Agent", "0.1.0")
            .description(
                "Generic critic / evaluator example: two deterministic skills, \
                 `validate_against_schema` (JSON Schema 2020-12) and `check_invariants` \
                 (non_empty / min_length / max_length / contains). No LLM, no external services.",
            )
            .url(
                format!("http://localhost:{DEFAULT_BIND_PORT}/jsonrpc"),
                "JSONRPC",
                "1.0",
            )
            .provider("turul-a2a", "https://github.com/aussierobots/turul-a2a")
            .streaming(true)
            .default_input_modes(vec!["application/json", "text/plain"])
            .default_output_modes(vec!["application/json"])
            .skill(validate_skill)
            .skill(invariants_skill)
            .build()?;

        Ok(Self {
            registry,
            agent_card,
        })
    }
}

/// Pick a `(skill_id, params)` from the inbound message text. Tries to parse
/// the text as JSON and uses its `kind` field; falls back to a non-empty
/// invariant check so the agent always produces a structured artifact.
fn dispatch(text: &str) -> (String, Value) {
    if let Ok(value) = serde_json::from_str::<Value>(text.trim())
        && let Some(kind) = value.get("kind").and_then(Value::as_str)
    {
        match kind {
            "validate_against_schema" => {
                let params = json!({
                    "value": value.get("value").cloned().unwrap_or(Value::Null),
                    "schema": value.get("schema").cloned().unwrap_or(json!({})),
                });
                return ("validate_against_schema".to_string(), params);
            }
            "check_invariants" => {
                let params = json!({
                    "value": value.get("value").cloned().unwrap_or(Value::Null),
                    "invariants": value
                        .get("invariants")
                        .cloned()
                        .unwrap_or_else(|| json!([])),
                });
                return ("check_invariants".to_string(), params);
            }
            _ => {}
        }
    }

    // Fallback: treat the raw text as the value and check that it is
    // non-empty. Keeps the agent useful for arbitrary inbound prose.
    (
        "check_invariants".to_string(),
        json!({
            "value": text,
            "invariants": [
                {"name": "non_empty", "check": "non_empty"}
            ]
        }),
    )
}

#[async_trait]
impl AgentExecutor for CriticExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let text = message.text_parts().join(" ");
        let (skill_id, params) = dispatch(&text);
        tracing::info!(
            target: "critic",
            skill_id = %skill_id,
            "critic dispatched"
        );

        let handler = self
            .registry
            .handler(&skill_id)
            .await
            .ok_or_else(|| A2aError::Internal(format!("unknown skill `{skill_id}`")))?;

        let sink = ExampleProgressSink(ctx.events.clone());

        match handler.run(params, &sink).await {
            Ok(output) => {
                let artifact_id = uuid::Uuid::now_v7().to_string();
                let payload = serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string());
                let artifact =
                    Artifact::new(artifact_id, vec![Part::text(payload)]).with_name(skill_id);
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

    let executor = CriticExecutor::build().await?;

    let port = std::env::var("A2A_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_BIND_PORT);

    let server = A2aServer::builder()
        .executor(executor)
        .bind(([0, 0, 0, 0], port))
        .build()?;

    println!("Critic Agent listening on http://0.0.0.0:{port}");
    println!("Agent card: http://localhost:{port}/.well-known/agent-card.json");
    println!();
    println!("Try:");
    println!("  curl -X POST http://localhost:{port}/message:send \\");
    println!("    -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \\");
    println!(
        "    -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"{{\\\"kind\\\":\\\"validate_against_schema\\\",\\\"value\\\":42,\\\"schema\\\":{{\\\"type\\\":\\\"integer\\\"}}}}\"}}]}}}}'"
    );

    server.run().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests for the invariant helpers and the dispatch router.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn non_empty_rejects_empty_string() {
        assert!(check_non_empty(&json!("")).is_err());
        assert!(check_non_empty(&json!("ok")).is_ok());
    }

    #[test]
    fn non_empty_rejects_empty_collections() {
        assert!(check_non_empty(&json!([])).is_err());
        assert!(check_non_empty(&json!({})).is_err());
        assert!(check_non_empty(&Value::Null).is_err());
        assert!(check_non_empty(&json!([1])).is_ok());
    }

    #[test]
    fn min_length_string_and_array() {
        assert!(check_min_length(&json!("abc"), 3).is_ok());
        assert!(check_min_length(&json!("ab"), 3).is_err());
        assert!(check_min_length(&json!([1, 2, 3, 4]), 3).is_ok());
    }

    #[test]
    fn max_length_string_and_array() {
        assert!(check_max_length(&json!("abc"), 3).is_ok());
        assert!(check_max_length(&json!("abcd"), 3).is_err());
        assert!(check_max_length(&json!([1, 2]), 3).is_ok());
    }

    #[test]
    fn contains_string_and_array() {
        assert!(check_contains(&json!("hello world"), &json!("world")).is_ok());
        assert!(check_contains(&json!("hello world"), &json!("nope")).is_err());
        assert!(check_contains(&json!(["a", "b", "c"]), &json!("b")).is_ok());
        assert!(check_contains(&json!([1, 2, 3]), &json!(4)).is_err());
    }

    #[test]
    fn dispatch_validate_kind() {
        let raw = json!({
            "kind": "validate_against_schema",
            "value": 42,
            "schema": {"type": "integer"}
        });
        let (id, params) = dispatch(&raw.to_string());
        assert_eq!(id, "validate_against_schema");
        assert_eq!(params["value"], json!(42));
    }

    #[test]
    fn dispatch_invariants_kind() {
        let raw = json!({
            "kind": "check_invariants",
            "value": "hi",
            "invariants": [{"name": "ne", "check": "non_empty"}]
        });
        let (id, _params) = dispatch(&raw.to_string());
        assert_eq!(id, "check_invariants");
    }

    #[test]
    fn dispatch_fallback_to_non_empty() {
        let (id, params) = dispatch("plain text input");
        assert_eq!(id, "check_invariants");
        assert_eq!(params["value"], json!("plain text input"));
        assert_eq!(params["invariants"][0]["check"], "non_empty");
    }

    #[test]
    fn manifests_parse_and_register() {
        // Build-time guarantee: both bundled SKILL.md files parse and round-trip
        // through the registry. Failing this test means the manifest authoring
        // drifted from `SkillCard` schema (e.g. a strict-keyword regression).
        let validate = SkillCard::parse(VALIDATE_AGAINST_SCHEMA_MANIFEST)
            .expect("validate_against_schema SKILL.md parses");
        assert_eq!(validate.id, "validate_against_schema");
        assert!(validate.input_schema.is_some());
        assert!(validate.output_schema.is_some());

        let invariants =
            SkillCard::parse(CHECK_INVARIANTS_MANIFEST).expect("check_invariants SKILL.md parses");
        assert_eq!(invariants.id, "check_invariants");
        assert!(invariants.input_schema.is_some());
        assert!(invariants.output_schema.is_some());
    }
}
