//! Server-side **delegating** A2A agent.
//!
//! Runs as a real A2A server, but its [`AgentExecutor::execute`] owns
//! a [`turul_a2a_client::A2aClient`] and forwards each inbound
//! [`Message`] to a configured remote A2A agent. Artifacts produced
//! by the remote agent are re-emitted as the delegate's own artifacts;
//! the delegate's task lifecycle is independent of the upstream task.
//!
//! Pattern shape: gateway agents, auth-gating proxies, region-fanout
//! precursors, A2A-shape mesh ingress.
//!
//! Run:
//!   cargo run -p remote-delegate-agent
//!   # In another shell, also bring up an upstream the delegate can
//!   # forward to:
//!   cargo run -p skill-manifest-ollama-agent
//!
//! Environment:
//!   REMOTE_AGENT_URL      Base URL of the upstream A2A agent.
//!                         Default: http://localhost:3010 (the offline
//!                         skill-manifest-ollama-agent).
//!   REMOTE_AGENT_BEARER   Optional bearer token the delegate
//!                         presents to the upstream. The delegate
//!                         does NOT forward caller credentials by
//!                         default — see README "Auth forwarding".
//!   REMOTE_TIMEOUT_SECS   Per-call HTTP timeout, default 30.
//!   A2A_PORT              Local bind port, default 3016.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use turul_a2a::A2aServer;
use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::profiles::{SKILL_ID_METADATA_KEY, SKILL_INVOCATION_PROFILE_V1};
use turul_a2a_client::response::SendResponse;
use turul_a2a_client::{A2aClient, A2aClientError, ClientAuth};
use turul_a2a_proto as pb;
use turul_a2a_types::wire::errors;
use turul_a2a_types::{Message, Task};

const DEFAULT_BIND_PORT: u16 = 3016;
const DEFAULT_REMOTE_URL: &str = "http://localhost:3010";
const DEFAULT_TIMEOUT_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// Executor: forward inbound messages to the configured remote agent.
// ---------------------------------------------------------------------------

struct RemoteDelegateExecutor {
    client: Arc<A2aClient>,
    local_card: pb::AgentCard,
    upstream_card: pb::AgentCard,
    timeout: Duration,
}

/// Extension URIs to activate on the upstream call for this message.
///
/// Activation is the `A2A-Extensions` request header, not
/// `Message.extensions` (that field only attributes extensions present
/// in the message). The delegate is the upstream's client, so it
/// activates the skill-invocation profile itself whenever the message
/// it forwards carries `a2a.skillId` routing metadata and the upstream
/// advertises the profile. Without the header the upstream would treat
/// the message as un-targeted and ignore the routing keys.
fn upstream_activations(message: &Message, upstream_card: &pb::AgentCard) -> Vec<String> {
    let targets_skill = message
        .metadata()
        .is_some_and(|m| m.fields.contains_key(SKILL_ID_METADATA_KEY));
    let upstream_advertises = upstream_card.capabilities.as_ref().is_some_and(|c| {
        c.extensions
            .iter()
            .any(|e| e.uri == SKILL_INVOCATION_PROFILE_V1)
    });
    if targets_skill && upstream_advertises {
        vec![SKILL_INVOCATION_PROFILE_V1.to_string()]
    } else {
        Vec::new()
    }
}

impl RemoteDelegateExecutor {
    async fn build(config: DelegateConfig) -> Result<Self, Box<dyn std::error::Error>> {
        // Construct an HTTP client with our per-call timeout, then attach
        // optional bearer credentials. The bearer is the delegate's own
        // credential for the upstream — caller credentials are NOT
        // forwarded (see README "Auth forwarding").
        let mut client = A2aClient::new(&config.remote_url);
        if let Some(token) = config.bearer_token.as_deref() {
            client = client.with_auth(ClientAuth::Bearer(token.to_string()));
        }

        // Discover the upstream's AgentCard once at boot; cache it for
        // the process lifetime. Restarting the delegate is the supported
        // way to pick up upstream changes.
        let upstream_card = client.fetch_agent_card().await.map_err(|e| {
            format!(
                "upstream discovery failed (GET {url}/.well-known/agent-card.json): {e}",
                url = config.remote_url
            )
        })?;

        let local_card = AgentCardBuilder::new("Remote Delegate Agent", "0.1.0")
            .description(
                "A2A agent that forwards every inbound message to a configured remote \
                 A2A agent and re-emits its artifacts. Pattern: gateway / proxy / fan-out precursor.",
            )
            .url(
                format!("http://localhost:{DEFAULT_BIND_PORT}/jsonrpc"),
                "JSONRPC",
                "1.0",
            )
            .provider("turul-a2a", "https://github.com/aussierobots/turul-a2a")
            .streaming(false)
            .default_input_modes(vec!["text/plain", "application/json"])
            .default_output_modes(vec!["application/json", "text/plain"])
            .skill(pb::AgentSkill {
                id: "delegate".into(),
                name: "Delegate".into(),
                description: format!(
                    "Forwards the inbound message to the configured upstream agent (`{}`) \
                     and re-emits its artifacts as the delegate's own.",
                    upstream_card.name
                ),
                tags: vec!["proxy".into(), "delegate".into()],
                examples: vec![],
                input_modes: vec!["text/plain".into(), "application/json".into()],
                output_modes: vec!["application/json".into(), "text/plain".into()],
                security_requirements: vec![],
            })
            .build()?;

        Ok(Self {
            client: Arc::new(client),
            local_card,
            upstream_card,
            timeout: Duration::from_secs(config.timeout_secs),
        })
    }
}

#[async_trait]
impl AgentExecutor for RemoteDelegateExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let outbound = build_outbound_request(message);
        let activations = upstream_activations(message, &self.upstream_card);
        // `A2aClient` is cheap to clone (the HTTP pool is shared); the
        // per-call clone carries this message's extension activations.
        let client = if activations.is_empty() {
            self.client.as_ref().clone()
        } else {
            self.client.as_ref().clone().with_extensions(activations)
        };

        // The A2aClient already carries an internal HTTP timeout, but we
        // wrap with tokio::time::timeout so the deadline applies even if
        // the underlying client's timeout configuration changes shape.
        let send_result =
            tokio::time::timeout(self.timeout, client.send_message_proto(outbound)).await;

        let proto_resp = match send_result {
            Ok(Ok(resp)) => resp,
            Ok(Err(err)) => return Err(map_upstream_error(err)),
            Err(_elapsed) => {
                return Err(A2aError::Internal(format!(
                    "upstream timed out after {}s (upstream={})",
                    self.timeout.as_secs(),
                    self.upstream_card.name
                )));
            }
        };

        let response = SendResponse::try_from(proto_resp).map_err(|e| {
            A2aError::Internal(format!(
                "upstream returned malformed SendMessageResponse: {e}"
            ))
        })?;

        // Successful upstream send → re-emit artifacts and complete the
        // local task. Message-shaped responses are surfaced as a text
        // artifact so the caller still observes something concrete.
        match response {
            SendResponse::Task(remote_task) => {
                for artifact in remote_task.artifacts() {
                    let local_artifact = turul_a2a_types::Artifact::new(
                        uuid::Uuid::now_v7().to_string(),
                        artifact
                            .parts
                            .iter()
                            .map(|p| turul_a2a_types::Part::from(p.clone()))
                            .collect(),
                    )
                    .with_name(artifact.name.clone());

                    ctx.events
                        .emit_artifact(local_artifact, false, true)
                        .await
                        .map_err(|e| {
                            A2aError::Internal(format!("local emit_artifact failed: {e}"))
                        })?;
                }
            }
            SendResponse::Message(remote_msg) => {
                let body = remote_msg.text_parts().join(" ");
                let local_artifact = turul_a2a_types::Artifact::new(
                    uuid::Uuid::now_v7().to_string(),
                    vec![turul_a2a_types::Part::text(body)],
                )
                .with_name("upstream-message".to_string());
                ctx.events
                    .emit_artifact(local_artifact, false, true)
                    .await
                    .map_err(|e| A2aError::Internal(format!("local emit_artifact failed: {e}")))?;
            }
            // SendResponse is #[non_exhaustive]; unknown future variants
            // surface as Internal so the operator notices rather than
            // silently dropping the upstream response.
            other => {
                return Err(A2aError::Internal(format!(
                    "upstream returned an unrecognised SendResponse variant: {other:?}"
                )));
            }
        }

        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> pb::AgentCard {
        self.local_card.clone()
    }
}

// ---------------------------------------------------------------------------
// Outbound request construction. Documented contract:
//   - message_id is regenerated locally; upstream message IDs are opaque.
//   - context_id and task_id are NOT forwarded; the upstream call is a
//     fresh conversation from the upstream's perspective. Trade-off: the
//     upstream cannot correlate the delegate's tasks across calls.
//   - metadata is forwarded intact, so skill-dispatch routing keys
//     (`a2a.skillId` / `a2a.skillParams`) reach the upstream.
//   - extensions (attribution only) are forwarded intact. Activation is
//     the A2A-Extensions header, set per call by `upstream_activations`.
//   - role and parts are copied verbatim.
// ---------------------------------------------------------------------------

fn build_outbound_request(message: &Message) -> pb::SendMessageRequest {
    let inbound = message.as_proto();
    let outbound_message = pb::Message {
        message_id: uuid::Uuid::now_v7().to_string(),
        context_id: String::new(),
        task_id: String::new(),
        role: inbound.role,
        parts: inbound.parts.clone(),
        metadata: inbound.metadata.clone(),
        extensions: inbound.extensions.clone(),
        reference_task_ids: inbound.reference_task_ids.clone(),
    };

    pb::SendMessageRequest {
        message: Some(outbound_message),
        tenant: String::new(),
        configuration: None,
        metadata: None,
    }
}

// ---------------------------------------------------------------------------
// Error mapping (see README "Error mapping" for the contract table).
//
// The upstream's google.rpc.ErrorInfo `reason` field, when present,
// drives classification. HTTP status is the secondary signal — used
// when the upstream did not attach ErrorInfo (raw 4xx/5xx). Transport-
// level failures (DNS, TLS, connection refused, body parse) map to
// Internal with an "upstream unreachable" prefix so operators can
// distinguish them from upstream-application errors.
// ---------------------------------------------------------------------------

fn map_upstream_error(err: A2aClientError) -> A2aError {
    match err {
        A2aClientError::A2aError {
            status,
            message,
            reason,
        } => match (reason.as_deref(), status) {
            // Several A2A errors share HTTP 400, so the ErrorInfo reason —
            // not the status — identifies which one the upstream raised.
            (Some(errors::REASON_TASK_NOT_FOUND), _) | (None, 404) => A2aError::TaskNotFound {
                task_id: format!("upstream:{message}"),
            },
            (Some(errors::REASON_UNSUPPORTED_OPERATION), _) => {
                A2aError::UnsupportedOperation { message }
            }
            (Some(errors::REASON_CONTENT_TYPE_NOT_SUPPORTED), _) => {
                A2aError::ContentTypeNotSupported {
                    content_type: message,
                }
            }
            (Some(errors::REASON_TASK_NOT_CANCELABLE), _) => A2aError::TaskNotCancelable {
                task_id: format!("upstream:{message}"),
            },
            (None, 400) => A2aError::InvalidRequest { message },
            _ => A2aError::Internal(format!(
                "upstream error (status={status}, reason={}): {message}",
                reason.as_deref().unwrap_or("<none>")
            )),
        },
        A2aClientError::Http { status, message } => {
            A2aError::Internal(format!("upstream non-A2A HTTP {status}: {message}"))
        }
        A2aClientError::Request(e) if e.is_timeout() => {
            A2aError::Internal(format!("upstream timed out (transport layer): {e}"))
        }
        A2aClientError::Request(e) => A2aError::Internal(format!("upstream unreachable: {e}")),
        A2aClientError::Json(e) => {
            A2aError::Internal(format!("upstream response not JSON-parseable: {e}"))
        }
        A2aClientError::Conversion(msg) => {
            A2aError::Internal(format!("upstream type conversion failed: {msg}"))
        }
        other => A2aError::Internal(format!("upstream error: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Configuration plumbing.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct DelegateConfig {
    remote_url: String,
    bearer_token: Option<String>,
    timeout_secs: u64,
}

impl DelegateConfig {
    fn from_env() -> Self {
        let remote_url = std::env::var("REMOTE_AGENT_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_REMOTE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let bearer_token = std::env::var("REMOTE_AGENT_BEARER")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let timeout_secs = std::env::var("REMOTE_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        Self {
            remote_url,
            bearer_token,
            timeout_secs,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt::init();

    let config = DelegateConfig::from_env();
    let port = std::env::var("A2A_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_BIND_PORT);

    let executor = RemoteDelegateExecutor::build(config.clone()).await?;
    let upstream_name = executor.upstream_card.name.clone();

    let server = A2aServer::builder()
        .executor(executor)
        .bind(([0, 0, 0, 0], port))
        .build()?;

    println!("Remote Delegate Agent listening on http://0.0.0.0:{port}");
    println!(
        "Forwarding to upstream: {} ({})",
        upstream_name, config.remote_url
    );
    println!("Auth forwarding: NEVER. Inbound caller credentials are not propagated.");
    println!("Streaming passthrough: NO. /message:stream falls back to buffered.");
    println!("Agent card: http://localhost:{port}/.well-known/agent-card.json");
    println!();
    println!("Send a message and watch it round-trip through the upstream:");
    println!("  curl -X POST http://localhost:{port}/message:send \\");
    println!("    -H 'Content-Type: application/json' \\");
    println!("    -H 'a2a-version: 1.0' \\");
    println!(
        "    -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"{{\\\"user\\\":{{\\\"name\\\":\\\"Ada\\\"}}}}\"}}]}}}}'"
    );

    server.run().await?;
    Ok(())
}

#[cfg(test)]
mod unit {
    use super::*;
    use serde_json::json;
    use turul_a2a_types::Part;

    #[test]
    fn outbound_regenerates_message_id_and_clears_context_and_task() {
        let inbound = Message::new(
            "caller-supplied-id",
            turul_a2a_types::Role::User,
            vec![Part::text("hello")],
        )
        .with_context_id("ctx-from-caller")
        .with_task_id("tid-from-caller");

        let req = build_outbound_request(&inbound);
        let outbound = req.message.expect("outbound message present");

        assert_ne!(outbound.message_id, "caller-supplied-id");
        assert!(!outbound.message_id.is_empty());
        assert_eq!(outbound.context_id, "");
        assert_eq!(outbound.task_id, "");
    }

    /// An inbound message with skill-dispatch routing metadata and an
    /// attribution entry in `Message.extensions`.
    fn skill_targeted_message() -> Message {
        let metadata = turul_a2a_types::pbjson::json_object_to_struct(
            [
                (SKILL_ID_METADATA_KEY.to_string(), json!("echo_loud")),
                ("a2a.skillParams".to_string(), json!({"text": "hi"})),
            ]
            .into(),
        );
        Message::try_from(pb::Message {
            message_id: "in-1".into(),
            role: pb::Role::User as i32,
            metadata: Some(metadata),
            extensions: vec![SKILL_INVOCATION_PROFILE_V1.into()],
            ..Default::default()
        })
        .unwrap()
    }

    fn upstream_card(advertised: &[&str]) -> pb::AgentCard {
        pb::AgentCard {
            capabilities: Some(pb::AgentCapabilities {
                extensions: advertised
                    .iter()
                    .map(|uri| pb::AgentExtension {
                        uri: (*uri).to_string(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// The routing metadata and the attribution list are copied to the
    /// outbound message unchanged. Neither activates anything upstream.
    #[test]
    fn outbound_preserves_metadata_and_attribution_extensions() {
        let inbound = skill_targeted_message();
        let outbound = build_outbound_request(&inbound)
            .message
            .expect("outbound message present");

        assert_eq!(
            outbound.metadata.as_ref(),
            inbound.as_proto().metadata.as_ref()
        );
        assert_eq!(
            outbound.extensions,
            vec![SKILL_INVOCATION_PROFILE_V1.to_string()]
        );
    }

    /// A skill-targeted message to an upstream advertising the profile
    /// activates it via the A2A-Extensions header.
    #[test]
    fn skill_targeted_message_activates_profile_upstream() {
        assert_eq!(
            upstream_activations(
                &skill_targeted_message(),
                &upstream_card(&[SKILL_INVOCATION_PROFILE_V1])
            ),
            vec![SKILL_INVOCATION_PROFILE_V1.to_string()]
        );
    }

    #[test]
    fn no_activation_without_routing_metadata_or_upstream_support() {
        let plain = Message::new("in-2", turul_a2a_types::Role::User, vec![Part::text("hi")]);
        assert!(
            upstream_activations(&plain, &upstream_card(&[SKILL_INVOCATION_PROFILE_V1])).is_empty()
        );
        assert!(upstream_activations(&skill_targeted_message(), &upstream_card(&[])).is_empty());
    }

    #[test]
    fn map_upstream_error_classifies_a2a_errors() {
        // TaskNotFound → 404 with ErrorInfo reason.
        let err = A2aClientError::A2aError {
            status: 404,
            message: "task xyz not found".into(),
            reason: Some(errors::REASON_TASK_NOT_FOUND.into()),
        };
        assert!(matches!(
            map_upstream_error(err),
            A2aError::TaskNotFound { .. }
        ));

        // UnsupportedOperation → 400 with ErrorInfo reason.
        let err = A2aClientError::A2aError {
            status: 400,
            message: "streaming not supported".into(),
            reason: Some(errors::REASON_UNSUPPORTED_OPERATION.into()),
        };
        assert!(matches!(
            map_upstream_error(err),
            A2aError::UnsupportedOperation { .. }
        ));

        // ContentTypeNotSupported → 400 with ErrorInfo reason.
        let err = A2aClientError::A2aError {
            status: 400,
            message: "image/png".into(),
            reason: Some(errors::REASON_CONTENT_TYPE_NOT_SUPPORTED.into()),
        };
        assert!(matches!(
            map_upstream_error(err),
            A2aError::ContentTypeNotSupported { .. }
        ));

        // Generic 400 without a known reason → InvalidRequest.
        let err = A2aClientError::A2aError {
            status: 400,
            message: "bad payload".into(),
            reason: None,
        };
        assert!(matches!(
            map_upstream_error(err),
            A2aError::InvalidRequest { .. }
        ));

        // Unknown status → Internal (with context).
        let err = A2aClientError::A2aError {
            status: 500,
            message: "boom".into(),
            reason: None,
        };
        assert!(matches!(map_upstream_error(err), A2aError::Internal(_)));
    }

    #[test]
    fn map_upstream_error_transport_failures_are_internal() {
        // Bare conversion: cannot easily fake a reqwest::Error,
        // so we exercise Http and the other variants instead.
        let err = A2aClientError::Http {
            status: 502,
            message: "bad gateway".into(),
        };
        let mapped = map_upstream_error(err);
        match mapped {
            A2aError::Internal(msg) => {
                assert!(msg.contains("upstream non-A2A HTTP 502"));
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }
}
