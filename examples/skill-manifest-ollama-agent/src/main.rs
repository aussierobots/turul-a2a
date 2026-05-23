//! A2A agent that exposes a manifest-backed skill end-to-end.
//!
//! - Parses `skills/demo/SKILL.md` at startup.
//! - Registers a `SkillHandler` in an `InMemorySkillRegistry`.
//! - Routes every inbound `execute()` call to that handler — this is the
//!   **example-owned dispatcher**; the framework does not ship one.
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
use serde::{Deserialize, Serialize};
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
use turul_llm_core::{CompletionRequest, LlmClient, LlmError};
use turul_llm_ollama::OllamaClient;

const DEFAULT_BIND_PORT: u16 = 3010;

/// SKILL.md shipped with the example. Embedded so the binary is
/// self-contained; the source file under `skills/demo/SKILL.md` is the
/// canonical reference for adopters.
const SKILL_MANIFEST: &str = include_str!("../skills/demo/SKILL.md");

// ---------------------------------------------------------------------------
// Bridge: orphan-rule-safe newtype around the framework's EventSink so we
// can `impl SkillProgressSink for ExampleProgressSink`. (Rust orphan rules
// forbid implementing an external trait on an external type from a third
// crate.)
// ---------------------------------------------------------------------------

struct ExampleProgressSink {
    event_sink: EventSink,
}

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
        self.event_sink
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
        self.event_sink
            .emit_artifact(artifact, append, last_chunk)
            .await
            .map(|_seq| ())
            .map_err(map_event_sink_error)
    }

    fn is_closed(&self) -> bool {
        self.event_sink.is_closed()
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
/// TODO: once `turul-a2a-patterns` is published, `turul-a2a` gains a
/// direct `impl SkillProgressSink for EventSink` and adopters no longer
/// need this mapper. Until then, every newtype bridge across the
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

/// Map `LlmError` from the provider adapter into the skill-handler error
/// type. The example treats every LLM failure as `Internal` because no
/// LLM variant currently signals a caller-recoverable condition — schema
/// violations from the provider are still operator-visible bugs in the
/// SKILL.md output schema or the model's structured-output support.
fn map_llm_error(err: LlmError) -> SkillError {
    SkillError::Internal(format!("LLM call failed: {err}"))
}

// ---------------------------------------------------------------------------
// Typed input / output for the `greet` skill.
//
// These structs are example-local ergonomics; SKILL.md is the authoritative
// contract. The handler still runs `card.validate_input` and
// `card.validate_output` against the manifest schemas — typed structs sit
// between those calls. Tests at the bottom of this file pin every typed
// payload round-trip against the manifest so struct/schema drift is caught.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GreetInput {
    user: UserInput,
    #[serde(default)]
    style: GreetingStyle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UserInput {
    name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum GreetingStyle {
    #[default]
    Casual,
    Formal,
}

impl GreetingStyle {
    fn salutation(&self) -> &'static str {
        match self {
            GreetingStyle::Formal => "Good day",
            GreetingStyle::Casual => "Hi",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GreetOutput {
    greeting: String,
}

// ---------------------------------------------------------------------------
// Skill handler — offline stub by default, Ollama when env vars are set.
// ---------------------------------------------------------------------------

struct GreetHandler {
    card: SkillCard,
}

impl GreetHandler {
    fn new(card: SkillCard) -> Self {
        Self { card }
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

    async fn run_live(&self, base_url: &str, prompt: &str) -> Result<GreetOutput, SkillError> {
        // The schema-passthrough lives inside the OllamaClient adapter (it
        // maps `output_schema` to Ollama's `format` field).
        let client = OllamaClient::new(base_url, self.provider_model());
        let mut request = CompletionRequest::new(prompt);
        if let Some(schema) = self.card.output_schema.clone() {
            request = request.with_output_schema(schema);
        }

        let response = client.complete(request).await.map_err(map_llm_error)?;
        serde_json::from_value::<GreetOutput>(response.parsed_output).map_err(|e| {
            SkillError::Internal(format!(
                "LLM response did not match typed output struct \
                 (manifest schema passed but struct rejected — possible drift): {e}"
            ))
        })
    }

    fn run_offline(&self, input: &GreetInput, prompt: &str) -> GreetOutput {
        let _ = prompt;
        GreetOutput {
            greeting: format!(
                "{}, {}! (offline stub)",
                input.style.salutation(),
                input.user.name
            ),
        }
    }
}

#[async_trait]
impl SkillHandler for GreetHandler {
    async fn run(&self, params: Value, sink: &dyn SkillProgressSink) -> Result<Value, SkillError> {
        // 1. Validate inbound params against the manifest's input schema.
        //    SKILL.md is authoritative — typed-struct deserialisation below
        //    layers on top of this check, not in place of it.
        self.card
            .validate_input(&params)
            .map_err(|e| SkillError::InvalidRequest(format!("inputSchema violation: {e}")))?;

        // 2. Deserialise into the typed input. If this fails after the
        //    manifest validator passed, the typed struct has drifted from the
        //    schema — flag it as Internal so the operator notices.
        let input: GreetInput = serde_json::from_value(params.clone()).map_err(|e| {
            SkillError::Internal(format!(
                "manifest input validated but typed struct rejected \
                 (struct/schema drift?): {e}"
            ))
        })?;

        // 3. Emit a Working ping so streaming subscribers see motion.
        let _ = sink.set_status(ProgressState::Working, None).await;

        // 4. Render the SKILL.md body as a prompt against the params.
        let prompt = self
            .card
            .render_prompt(&params)
            .map_err(|e| SkillError::Internal(format!("prompt render failed: {e}")))?;

        // 5. Dispatch to live Ollama or offline stub.
        let typed_output = if let Some(base) = Self::ollama_base_url() {
            tracing::info!(target: "skill-manifest-ollama-agent", base, "ollama dispatch");
            self.run_live(&base, &prompt).await?
        } else {
            tracing::info!(target: "skill-manifest-ollama-agent", "offline stub dispatch");
            self.run_offline(&input, &prompt)
        };

        // 6. Re-serialise to JSON and validate against the manifest's output
        //    schema — the manifest is still the authoritative output contract.
        let output = serde_json::to_value(&typed_output)
            .map_err(|e| SkillError::Internal(format!("output serialisation failed: {e}")))?;
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
        // the trait the handler expects.
        let sink = ExampleProgressSink {
            event_sink: ctx.events.clone(),
        };

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
        let input: GreetInput = serde_json::from_value(params).unwrap();
        let output = handler.run_offline(&input, &prompt);
        let output_json = serde_json::to_value(&output).unwrap();
        card.validate_output(&output_json)
            .expect("offline stub output must satisfy outputSchema");
        assert!(output.greeting.contains("Ada"));
    }

    // -----------------------------------------------------------------
    // Typed struct ↔ SKILL.md schema round-trip tests.
    //
    // These pin every example payload to the typed structs AND assert
    // the structs serialise back to schema-valid JSON. If SKILL.md or
    // the structs drift apart, one of these fails before the example
    // ships a broken contract.
    //
    // SKILL.md remains the authoritative schema — the structs layer
    // over it for ergonomics, never replace it.
    // -----------------------------------------------------------------

    /// Every JSON payload an external caller might send must deserialise
    /// into `GreetInput`. The list mirrors the README + smoke + SKILL.md
    /// `examples` block so adding new doc examples is forced through this
    /// gate.
    const EXAMPLE_PAYLOADS: &[&str] = &[
        // README "JSON-shaped greeting" curl payload (also smoke.rs).
        r#"{"user":{"name":"Ada"},"style":"formal"}"#,
        // README plain-text fallback after extract_params lifts a name.
        r#"{"user":{"name":"Ada"}}"#,
        // SKILL.md examples block "Casually greet Grace" — extract_params
        // produces this shape from plain text input.
        r#"{"user":{"name":"Grace"},"style":"casual"}"#,
        // Bare minimum the schema permits (style defaults to casual).
        r#"{"user":{"name":"friend"}}"#,
    ];

    #[test]
    fn every_example_payload_deserialises_into_struct() {
        for raw in EXAMPLE_PAYLOADS {
            let v: Value = serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("payload `{raw}` is not valid JSON: {e}"));
            serde_json::from_value::<GreetInput>(v).unwrap_or_else(|e| {
                panic!(
                    "GreetInput failed to deserialise example payload `{raw}`: {e} \
                     (the typed struct may have drifted from SKILL.md inputSchema)"
                )
            });
        }
    }

    #[test]
    fn typed_input_serialises_to_schema_valid_json() {
        let card = SkillCard::parse(SKILL_MANIFEST).unwrap();
        let sample = GreetInput {
            user: UserInput { name: "Ada".into() },
            style: GreetingStyle::Formal,
        };
        let v = serde_json::to_value(&sample).unwrap();
        card.validate_input(&v).expect(
            "GreetInput must serialise to JSON that satisfies SKILL.md inputSchema \
             (struct ↔ schema drift?)",
        );
    }

    #[test]
    fn typed_output_serialises_to_schema_valid_json() {
        let card = SkillCard::parse(SKILL_MANIFEST).unwrap();
        let sample = GreetOutput {
            greeting: "Hello, world!".into(),
        };
        let v = serde_json::to_value(&sample).unwrap();
        card.validate_output(&v).expect(
            "GreetOutput must serialise to JSON that satisfies SKILL.md outputSchema \
             (struct ↔ schema drift?)",
        );
    }

    /// SKILL.md inputSchema enforces invariants that the typed struct
    /// alone cannot express (here: `user.name` `minLength: 1`). The
    /// handler must run `card.validate_input` BEFORE typed deserialise
    /// so this kind of constraint is caught at the manifest boundary,
    /// not silently accepted by serde.
    #[test]
    fn manifest_validates_before_struct_deserialises() {
        let card = SkillCard::parse(SKILL_MANIFEST).unwrap();
        let invalid = json!({"user": {"name": ""}});

        // The manifest must reject empty name.
        assert!(
            card.validate_input(&invalid).is_err(),
            "SKILL.md inputSchema must reject `user.name = \"\"` (minLength: 1); \
             if this passes, the manifest is no longer enforcing real constraints"
        );

        // Confirms the typed struct alone would NOT catch this — serde
        // happily accepts an empty string. This is exactly why the
        // handler runs validate_input first.
        let struct_accepts: GreetInput =
            serde_json::from_value(invalid).expect("serde should accept empty string");
        assert_eq!(struct_accepts.user.name, "");
    }
}
