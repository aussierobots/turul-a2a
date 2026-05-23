//! Showcase agent for the skill-invocation dispatcher profile.
//!
//! Two manifest-backed skills (`echo_loud`, `reverse`) are registered in an
//! `InMemorySkillRegistry`. The agent advertises the profile URI
//! `https://turul.dev/a2a/extensions/skill-invocation/v1` in its
//! `AgentCapabilities.extensions`. Clients activate the profile by sending
//! the `A2A-Extensions` HTTP header and placing the target skill id in
//! `Message.metadata["a2a.skillId"]` (with optional inputs in
//! `Message.metadata["a2a.skillParams"]`).
//!
//! The framework's transport layer handles header parsing, advertisement
//! validation, and echo on the response. This example focuses on the
//! adopter side: reading the metadata keys and dispatching to the
//! registered skill.
//!
//! Run:
//!   cargo run -p skill-dispatch-profile-agent

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use turul_a2a::A2aServer;
use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::event_sink::EventSink;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::profile_dispatch::SKILL_INVOCATION_PROFILE_V1;
use turul_a2a_patterns::{
    InMemorySkillRegistry, ProgressState, SinkError, SkillCard, SkillError, SkillHandler,
    SkillProgressSink, SkillRegistry,
};
use turul_a2a_types::pbjson::{struct_to_json_object, value_to_json};
use turul_a2a_types::{Artifact, Message, Part, Task};

const DEFAULT_BIND_PORT: u16 = 3015;

/// Reserved metadata key holding the target `AgentSkill.id`.
const META_SKILL_ID: &str = "a2a.skillId";
/// Reserved metadata key holding the structured parameter object.
const META_SKILL_PARAMS: &str = "a2a.skillParams";

// Manifests embedded so the binary is self-contained; the source files
// under `skills/<id>/SKILL.md` are the canonical reference for adopters.
const ECHO_LOUD_MANIFEST: &str = include_str!("../skills/echo_loud/SKILL.md");
const REVERSE_MANIFEST: &str = include_str!("../skills/reverse/SKILL.md");

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

/// Maps `turul_a2a::A2aError` from the framework `EventSink` into
/// `turul_a2a_patterns::SinkError`. The patterns crate cannot depend on
/// `turul-a2a`, so each example bridge carries its own mapper. A typed
/// match on `InvalidRequest { message }` with a literal "EventSink is
/// closed" prefix is the least-brittle way to detect the closed-sink
/// race today.
fn map_event_sink_error(err: A2aError) -> SinkError {
    match err {
        A2aError::InvalidRequest { message } if message.starts_with("EventSink is closed") => {
            SinkError::Closed
        }
        other => SinkError::Backend(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Skill: echo_loud — uppercases the `text` input.
// ---------------------------------------------------------------------------

struct EchoLoudSkill {
    card: SkillCard,
}

#[async_trait]
impl SkillHandler for EchoLoudSkill {
    async fn run(&self, params: Value, sink: &dyn SkillProgressSink) -> Result<Value, SkillError> {
        self.card
            .validate_input(&params)
            .map_err(|e| SkillError::InvalidRequest(format!("inputSchema violation: {e}")))?;

        let text = params
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| SkillError::InvalidRequest("missing string `text`".into()))?;

        let _ = sink.set_status(ProgressState::Working, None).await;

        let result = json!({ "shouted": text.to_uppercase() });

        self.card
            .validate_output(&result)
            .map_err(|e| SkillError::Internal(format!("outputSchema violation: {e}")))?;

        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Skill: reverse — reverses the `text` input character-by-character.
// ---------------------------------------------------------------------------

struct ReverseSkill {
    card: SkillCard,
}

#[async_trait]
impl SkillHandler for ReverseSkill {
    async fn run(&self, params: Value, sink: &dyn SkillProgressSink) -> Result<Value, SkillError> {
        self.card
            .validate_input(&params)
            .map_err(|e| SkillError::InvalidRequest(format!("inputSchema violation: {e}")))?;

        let text = params
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| SkillError::InvalidRequest("missing string `text`".into()))?;

        let _ = sink.set_status(ProgressState::Working, None).await;

        let reversed: String = text.chars().rev().collect();
        let result = json!({ "reversed": reversed });

        self.card
            .validate_output(&result)
            .map_err(|e| SkillError::Internal(format!("outputSchema violation: {e}")))?;

        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// AgentExecutor — the dispatcher. Reads `Message.metadata["a2a.skillId"]`
// and routes to the registry, or fails the task if the metadata is absent.
// ---------------------------------------------------------------------------

struct DispatcherExecutor {
    registry: Arc<InMemorySkillRegistry>,
    agent_card: turul_a2a_proto::AgentCard,
}

impl DispatcherExecutor {
    async fn build() -> Result<Self, Box<dyn std::error::Error>> {
        let echo_card = SkillCard::parse(ECHO_LOUD_MANIFEST)
            .map_err(|e| format!("parse echo_loud SKILL.md: {e}"))?;
        let reverse_card = SkillCard::parse(REVERSE_MANIFEST)
            .map_err(|e| format!("parse reverse SKILL.md: {e}"))?;

        let echo_agent_skill = echo_card.to_agent_skill();
        let reverse_agent_skill = reverse_card.to_agent_skill();

        let registry = Arc::new(InMemorySkillRegistry::new());

        let echo_handler: Arc<dyn SkillHandler> = Arc::new(EchoLoudSkill {
            card: echo_card.clone(),
        });
        registry
            .register_manifest(echo_card, echo_handler)
            .await
            .map_err(|e| format!("register echo_loud: {e}"))?;

        let reverse_handler: Arc<dyn SkillHandler> = Arc::new(ReverseSkill {
            card: reverse_card.clone(),
        });
        registry
            .register_manifest(reverse_card, reverse_handler)
            .await
            .map_err(|e| format!("register reverse: {e}"))?;

        let extension = turul_a2a_proto::AgentExtension {
            uri: SKILL_INVOCATION_PROFILE_V1.to_string(),
            description: "Skill-invocation dispatch via Message.metadata \
                          (a2a.skillId + a2a.skillParams)."
                .to_string(),
            required: false,
            params: None,
        };

        let agent_card = AgentCardBuilder::new("Skill Dispatch Profile Agent", "0.1.0")
            .description(
                "Multi-skill agent that demonstrates the skill-invocation dispatcher \
                 profile: clients activate the profile via the A2A-Extensions header \
                 and target a skill by setting Message.metadata[\"a2a.skillId\"].",
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
            .extension(extension)
            .skill(echo_agent_skill)
            .skill(reverse_agent_skill)
            .build()?;

        Ok(Self {
            registry,
            agent_card,
        })
    }
}

/// Inspect `Message.metadata` for the two reserved profile keys and
/// return `(skill_id, params)`. Returns `None` for `skill_id` if the key
/// is absent (the caller treats this as "profile not activated /
/// missing dispatch intent" and fails the task explanatorily).
///
/// `a2a.skillParams` is optional; absence is mapped to an empty object so
/// downstream schema validation still has a well-formed input to check.
fn extract_dispatch_payload(message: &Message) -> (Option<String>, Value) {
    let Some(metadata) = message.metadata() else {
        return (None, json!({}));
    };

    let skill_id = metadata
        .fields
        .get(META_SKILL_ID)
        .map(|v| value_to_json(v.clone()))
        .and_then(|v| v.as_str().map(str::to_string));

    let params = metadata
        .fields
        .get(META_SKILL_PARAMS)
        .map(|v| value_to_json(v.clone()))
        .unwrap_or_else(|| json!({}));

    (skill_id, params)
}

#[async_trait]
impl AgentExecutor for DispatcherExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let (skill_id, params) = extract_dispatch_payload(message);

        let Some(skill_id) = skill_id else {
            // Profile not activated, or activated but missing the required
            // key. We surface this as a Failed task with an explanatory
            // message — adopters can swap this for InvalidParamsError if
            // they prefer a hard-reject contract.
            let metadata_keys = message.metadata_keys();
            task.fail(format!(
                "Skill dispatch requires Message.metadata[\"{META_SKILL_ID}\"] (string). \
                 Send the request with header `A2A-Extensions: {SKILL_INVOCATION_PROFILE_V1}` \
                 and set the skill id in `metadata` (and inputs in `{META_SKILL_PARAMS}`). \
                 Observed metadata keys: {metadata_keys:?}."
            ));
            return Ok(());
        };

        tracing::info!(
            target: "skill-dispatch-profile-agent",
            skill_id = %skill_id,
            "dispatching to skill",
        );

        let handler =
            self.registry
                .handler(&skill_id)
                .await
                .ok_or_else(|| A2aError::InvalidRequest {
                    message: format!("unknown skill id `{skill_id}`"),
                })?;

        let sink = ExampleProgressSink(ctx.events.clone());

        match handler.run(params, &sink).await {
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

    let executor = DispatcherExecutor::build().await?;

    let port = std::env::var("A2A_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_BIND_PORT);

    let server = A2aServer::builder()
        .executor(executor)
        .bind(([0, 0, 0, 0], port))
        .build()?;

    println!("Skill Dispatch Profile Agent listening on http://0.0.0.0:{port}");
    println!("Agent card: http://localhost:{port}/.well-known/agent-card.json");
    println!();
    println!("Try (activates the skill-invocation profile):");
    println!("  curl -X POST http://localhost:{port}/message:send \\");
    println!("    -H 'Content-Type: application/json' -H 'a2a-version: 1.0' \\");
    println!("    -H 'A2A-Extensions: {SKILL_INVOCATION_PROFILE_V1}' \\");
    println!(
        "    -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"hi\"}}],\"metadata\":{{\"a2a.skillId\":\"echo_loud\",\"a2a.skillParams\":{{\"text\":\"hi\"}}}}}}}}'"
    );

    server.run().await?;
    Ok(())
}

// Silence dead_code on `struct_to_json_object` import if the future
// reads-by-key path is removed. Imported for adopter discoverability
// from `turul_a2a_types::pbjson`.
#[allow(dead_code)]
fn _force_pbjson_import_visibility() -> usize {
    let _ = struct_to_json_object;
    0
}

// ---------------------------------------------------------------------------
// Unit tests for the dispatch metadata extractor.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod unit {
    use super::*;
    use std::collections::HashMap;
    use turul_a2a_types::pbjson::json_object_to_struct;
    use turul_a2a_types::{Part, Role};

    fn message_with_metadata(meta: HashMap<String, Value>) -> Message {
        let inner = turul_a2a_proto::Message {
            message_id: "m-1".into(),
            role: turul_a2a_proto::Role::User.into(),
            parts: vec![turul_a2a_proto::Part {
                content: Some(turul_a2a_proto::part::Content::Text("hi".into())),
                metadata: None,
                filename: String::new(),
                media_type: String::new(),
            }],
            context_id: String::new(),
            task_id: String::new(),
            extensions: vec![],
            metadata: Some(json_object_to_struct(meta)),
            reference_task_ids: vec![],
        };
        Message::try_from(inner).expect("message conversion")
    }

    #[test]
    fn extract_returns_none_when_no_metadata() {
        let msg = Message::new("m-1", Role::User, vec![Part::text("hi")]);
        let (id, params) = extract_dispatch_payload(&msg);
        assert!(id.is_none());
        assert_eq!(params, json!({}));
    }

    #[test]
    fn extract_reads_skill_id_and_params() {
        let mut meta = HashMap::new();
        meta.insert(META_SKILL_ID.to_string(), json!("echo_loud"));
        meta.insert(META_SKILL_PARAMS.to_string(), json!({ "text": "hi" }));
        let msg = message_with_metadata(meta);
        let (id, params) = extract_dispatch_payload(&msg);
        assert_eq!(id.as_deref(), Some("echo_loud"));
        assert_eq!(params, json!({ "text": "hi" }));
    }

    #[test]
    fn extract_params_default_empty_object_when_absent() {
        let mut meta = HashMap::new();
        meta.insert(META_SKILL_ID.to_string(), json!("reverse"));
        let msg = message_with_metadata(meta);
        let (id, params) = extract_dispatch_payload(&msg);
        assert_eq!(id.as_deref(), Some("reverse"));
        assert_eq!(params, json!({}));
    }

    #[test]
    fn echo_loud_manifest_parses() {
        let card = SkillCard::parse(ECHO_LOUD_MANIFEST).expect("echo_loud SKILL.md parses");
        assert_eq!(card.id, "echo_loud");
        assert!(card.input_schema.is_some());
        assert!(card.output_schema.is_some());
    }

    #[test]
    fn reverse_manifest_parses() {
        let card = SkillCard::parse(REVERSE_MANIFEST).expect("reverse SKILL.md parses");
        assert_eq!(card.id, "reverse");
        assert!(card.input_schema.is_some());
        assert!(card.output_schema.is_some());
    }
}
