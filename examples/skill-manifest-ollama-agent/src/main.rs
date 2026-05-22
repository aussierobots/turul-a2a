//! A2A agent that exposes a manifest-backed skill end-to-end.
//!
//! - Parses `skills/demo/SKILL.md` at startup (ADR-021 §2.2 item 3).
//! - Registers a `SkillHandler` in an `InMemorySkillRegistry`.
//! - Routes every inbound `execute()` call to that handler — this is the
//!   **example-owned dispatcher**; ADR-021 §2.4 defers a framework-level
//!   one.
//! - Offline by default: the handler returns a deterministic stub that
//!   satisfies the manifest's output schema, so `cargo test` is hermetic.
//! - Live Ollama: enabled by setting `OLLAMA_BASE_URL` (or
//!   `RUN_OLLAMA_SMOKE=1`); the handler POSTs to `/api/chat` with
//!   structured-output `format` set to the manifest's output schema.
//!
//! Run:
//!   cargo run -p skill-manifest-ollama-agent
//!   # then in another shell:
//!   cargo run -p turul-a2a-rust-client-example  # see README.md for caveat

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
use turul_a2a_types::{Artifact, Message, Part, Task, TaskState};

const DEFAULT_BIND_PORT: u16 = 3010;

/// SKILL.md shipped with the example. Embedded so the binary is
/// self-contained; the source file under `skills/demo/SKILL.md` is the
/// canonical reference for adopters.
const SKILL_MANIFEST: &str = include_str!("../skills/demo/SKILL.md");

// ---------------------------------------------------------------------------
// Bridge: orphan-rule-safe newtype around the framework's EventSink so we
// can `impl SkillProgressSink for ExampleProgressSink`. See ADR-021 §2.3.
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
            ProgressState::Working => TaskState::Working,
            ProgressState::InputRequired => TaskState::InputRequired,
            ProgressState::AuthRequired => TaskState::AuthRequired,
            _ => TaskState::Working,
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
/// "EventSink is closed" prefix is the least-bad way to detect the
/// closed-sink race today.
///
/// TODO: when §4 gates clear and `turul-a2a-patterns` becomes
/// publishable (per ADR-021 §4.1), `turul-a2a` gains a direct
/// `impl SkillProgressSink for EventSink` and adopters no longer need
/// this mapper. Until then, every newtype bridge across the
/// `examples/` tree carries its own copy. The framework-side fix is
/// also tracked: expose a typed close-state on `A2aError` (or a
/// dedicated `EventSinkError`) so adapters don't need to string-match
/// at all.
fn map_event_sink_error(err: A2aError) -> SinkError {
    match err {
        A2aError::InvalidRequest { message } if message.starts_with("EventSink is closed") => {
            SinkError::Closed
        }
        other => SinkError::Backend(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Skill handler — offline stub by default, Ollama when env vars are set.
// ---------------------------------------------------------------------------

struct GreetHandler {
    card: SkillCard,
    http: reqwest::Client,
}

impl GreetHandler {
    fn new(card: SkillCard) -> Self {
        Self {
            card,
            http: reqwest::Client::new(),
        }
    }

    fn ollama_base_url() -> Option<String> {
        if let Ok(v) = std::env::var("OLLAMA_BASE_URL")
            && !v.trim().is_empty()
        {
            return Some(v.trim().trim_end_matches('/').to_string());
        }
        if std::env::var("RUN_OLLAMA_SMOKE").ok().as_deref() == Some("1") {
            return Some("http://localhost:11434".to_string());
        }
        None
    }

    fn provider_model(&self) -> &str {
        self.card
            .provider_config
            .as_ref()
            .and_then(|v| v.get("model"))
            .and_then(|m| m.as_str())
            .unwrap_or("llama3.1")
    }

    async fn run_live(&self, base_url: &str, prompt: &str) -> Result<Value, SkillError> {
        let model = self.provider_model().to_string();
        let format_schema = self
            .card
            .output_schema
            .clone()
            .unwrap_or_else(|| json!({"type": "object"}));

        let body = json!({
            "model": model,
            "stream": false,
            "format": format_schema,
            "messages": [
                {"role": "user", "content": prompt},
            ],
        });

        let url = format!("{base_url}/api/chat");
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| SkillError::Internal(format!("ollama POST {url} failed: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| SkillError::Internal(format!("ollama body read failed: {e}")))?;
        if !status.is_success() {
            return Err(SkillError::Internal(format!(
                "ollama {url} returned HTTP {status}: {text}"
            )));
        }

        let envelope: Value = serde_json::from_str(&text)
            .map_err(|e| SkillError::Internal(format!("ollama JSON parse failed: {e}: {text}")))?;
        let content = envelope
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .ok_or_else(|| {
                SkillError::Internal(format!(
                    "ollama response missing /message/content: {envelope}"
                ))
            })?;

        serde_json::from_str::<Value>(content).map_err(|e| {
            SkillError::Internal(format!(
                "ollama structured-output payload not valid JSON: {e}: {content}"
            ))
        })
    }

    fn run_offline(&self, params: &Value, prompt: &str) -> Value {
        let user_name = params
            .get("user")
            .and_then(|u| u.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("friend");
        let style = params
            .get("style")
            .and_then(|s| s.as_str())
            .unwrap_or("casual");
        let prefix = if style == "formal" { "Good day" } else { "Hi" };
        let _ = prompt; // referenced via tracing below; offline ignores it
        json!({
            "greeting": format!("{prefix}, {user_name}! (offline stub)")
        })
    }
}

#[async_trait]
impl SkillHandler for GreetHandler {
    async fn run(&self, params: Value, sink: &dyn SkillProgressSink) -> Result<Value, SkillError> {
        // 1. Validate inbound params against the manifest's input schema.
        self.card
            .validate_input(&params)
            .map_err(|e| SkillError::InvalidRequest(format!("inputSchema violation: {e}")))?;

        // 2. Emit a Working ping so streaming subscribers see motion.
        let _ = sink.set_status(ProgressState::Working, None).await;

        // 3. Render the SKILL.md body as a prompt against the params.
        let prompt = self
            .card
            .render_prompt(&params)
            .map_err(|e| SkillError::Internal(format!("prompt render failed: {e}")))?;

        // 4. Dispatch to live Ollama or offline stub.
        let output = if let Some(base) = Self::ollama_base_url() {
            tracing::info!(target: "skill-manifest-ollama-agent", base, "ollama dispatch");
            self.run_live(&base, &prompt).await?
        } else {
            tracing::info!(target: "skill-manifest-ollama-agent", "offline stub dispatch");
            self.run_offline(&params, &prompt)
        };

        // 5. Validate output against the manifest's output schema.
        self.card
            .validate_output(&output)
            .map_err(|e| SkillError::Internal(format!("outputSchema violation: {e}")))?;

        // 6. Emit as an artifact. The executor below handles task terminal.
        let artifact_id = uuid::Uuid::now_v7().to_string();
        let payload = serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string());
        let artifact =
            Artifact::new(artifact_id, vec![Part::text(payload)]).with_name("greeting".to_string());
        sink.emit_artifact(artifact, false, true)
            .await
            .map_err(|e| SkillError::Internal(format!("sink emit_artifact failed: {e}")))?;

        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// AgentExecutor — example-owned dispatcher routes every message to the
// single registered skill. A real adopter with multiple skills would
// inspect the message (e.g. extensions, text content) to pick a handler.
// ---------------------------------------------------------------------------

struct ManifestExecutor {
    registry: Arc<InMemorySkillRegistry>,
    agent_card: turul_a2a_proto::AgentCard,
    skill_id: String,
}

impl ManifestExecutor {
    async fn build() -> Result<Self, Box<dyn std::error::Error>> {
        let card = SkillCard::parse(SKILL_MANIFEST)?;
        let skill_id = card.id.clone();
        let agent_skill = card.to_agent_skill();

        let registry = Arc::new(InMemorySkillRegistry::new());
        let handler: Arc<dyn SkillHandler> = Arc::new(GreetHandler::new(card.clone()));
        registry
            .register_manifest(card, handler)
            .await
            .map_err(|e| format!("register_manifest failed: {e}"))?;

        let agent_card = AgentCardBuilder::new("Skill Manifest Ollama Agent", "0.1.0")
            .description(
                "Reference example: a SKILL.md manifest drives this agent's advertised skill, \
                 input/output validation, and (optionally) an Ollama call.",
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
            .skill(agent_skill)
            .build()?;

        Ok(Self {
            registry,
            agent_card,
            skill_id,
        })
    }
}

#[async_trait]
impl AgentExecutor for ManifestExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        // Extract params: prefer a JSON text part; fall back to the
        // concatenated text parts under "user.name". Keeps the example
        // exercisable from plain-text clients without forcing them to
        // know the input schema.
        let params = extract_params(message);

        // Look up the handler by manifest id.
        let handler = self.registry.handler(&self.skill_id).await.ok_or_else(|| {
            A2aError::Internal(format!("skill `{}` not registered", self.skill_id))
        })?;

        // Bridge the framework EventSink through our newtype to satisfy
        // the trait the handler expects (ADR-021 §2.3).
        let sink = ExampleProgressSink(ctx.events.clone());

        match handler.run(params, &sink).await {
            Ok(_output) => {
                // Sink already emitted the artifact; framework will commit
                // the COMPLETED terminal post-execute from `task.complete()`.
                task.complete();
                Ok(())
            }
            Err(SkillError::InvalidRequest(msg)) => Err(A2aError::InvalidRequest { message: msg }),
            Err(SkillError::Internal(msg)) => Err(A2aError::Internal(msg)),
            // SkillError is #[non_exhaustive]; treat unknown variants as
            // Internal so the example keeps compiling across patterns
            // crate revisions.
            Err(other) => Err(A2aError::Internal(format!("unhandled SkillError: {other}"))),
        }
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        self.agent_card.clone()
    }
}

/// Try to interpret the message's text parts as JSON. If that fails, fall
/// back to a minimal `{user: {name: "<text>"}, style: "casual"}` shape so
/// the example is usable from plain-text echo-style clients (and the
/// prompt template always finds every `{{ path }}` it references).
fn extract_params(message: &Message) -> Value {
    let combined = message.text_parts().join(" ");
    if combined.trim().is_empty() {
        return json!({"user": {"name": "friend"}, "style": "casual"});
    }
    let parsed = serde_json::from_str::<Value>(&combined)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"user": {"name": combined.trim()}}));
    fill_defaults(parsed)
}

/// Fill in manifest-declared defaults so plain-text inputs don't trip
/// the template renderer on missing variables. Keeps the example
/// permissive without weakening the inputSchema validator.
fn fill_defaults(mut value: Value) -> Value {
    if let Some(obj) = value.as_object_mut()
        && !obj.contains_key("style")
    {
        obj.insert("style".to_string(), Value::String("casual".to_string()));
    }
    value
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load `.env` from the current working directory if present. Per-developer
    // config (e.g. `OLLAMA_BASE_URL=http://<your-ollama-host>:11434`) lives in `.env`
    // and is gitignored; `.env.example` is committed as documentation.
    // Tests do not call `main()` and remain hermetic regardless of `.env`.
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt::init();

    let executor = ManifestExecutor::build().await?;

    let port = std::env::var("A2A_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_BIND_PORT);

    let mode = if GreetHandler::ollama_base_url().is_some() {
        "live-ollama"
    } else {
        "offline-stub"
    };

    let server = A2aServer::builder()
        .executor(executor)
        .bind(([0, 0, 0, 0], port))
        .build()?;

    println!("Skill Manifest Ollama Agent listening on http://0.0.0.0:{port}");
    println!("Mode: {mode}");
    println!("Agent card: http://localhost:{port}/.well-known/agent-card.json");
    println!();
    println!("Send a JSON-shaped greeting:");
    println!("  curl -X POST http://localhost:{port}/message:send \\");
    println!("    -H 'Content-Type: application/json' \\");
    println!("    -H 'a2a-version: 1.0' \\");
    println!(
        "    -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"{{\\\"user\\\":{{\\\"name\\\":\\\"Ada\\\"}},\\\"style\\\":\\\"formal\\\"}}\"}}]}}}}'"
    );

    server.run().await?;
    Ok(())
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn extract_params_from_plain_text() {
        let m = Message::new("m1", turul_a2a_types::Role::User, vec![Part::text("Ada")]);
        let v = extract_params(&m);
        assert_eq!(v["user"]["name"], "Ada");
    }

    #[test]
    fn extract_params_from_json_text() {
        let m = Message::new(
            "m2",
            turul_a2a_types::Role::User,
            vec![Part::text(r#"{"user":{"name":"Grace"},"style":"formal"}"#)],
        );
        let v = extract_params(&m);
        assert_eq!(v["user"]["name"], "Grace");
        assert_eq!(v["style"], "formal");
    }

    #[test]
    fn skill_manifest_parses() {
        let card = SkillCard::parse(SKILL_MANIFEST).expect("manifest parses");
        assert_eq!(card.id, "greet");
        assert!(card.input_schema.is_some());
        assert!(card.output_schema.is_some());
    }

    #[tokio::test]
    async fn offline_handler_produces_schema_valid_output() {
        let card = SkillCard::parse(SKILL_MANIFEST).unwrap();
        let handler = GreetHandler::new(card.clone());
        let params = json!({"user": {"name": "Ada"}, "style": "formal"});
        let prompt = card.render_prompt(&params).unwrap();
        let output = handler.run_offline(&params, &prompt);
        card.validate_output(&output)
            .expect("offline stub output must satisfy outputSchema");
        assert!(output["greeting"].as_str().unwrap().contains("Ada"));
    }
}
