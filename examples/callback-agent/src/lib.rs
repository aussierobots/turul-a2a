//! Callback Agent library — shared between `src/main.rs` (runnable
//! binary) and `tests/smoke.rs` (in-process end-to-end test).
//!
//! Demonstrates the A2A push-notification contract:
//!
//! 1. Caller registers a `TaskPushNotificationConfig` — webhook URL
//!    plus a shared secret `token` — against a task id.
//! 2. When the task reaches a terminal state, the framework POSTs
//!    the terminal Task JSON to that URL with the `token` echoed in
//!    the `X-Turul-Push-Token` header (note: framework header, not
//!    the spec's `X-A2A-Notification-Token`).
//! 3. The receiver validates the header matches the token it
//!    registered. A mismatch means the call isn't ours — reject.
//!
//! The `CallbackExecutor` below is a deliberately slow echo: sleep
//! for `delay_ms`, append one artifact with the echoed text, then
//! commit `Completed`. The sleep lets the caller race a push-config
//! registration in before terminal fires (configs registered *after*
//! terminal are not retroactively delivered).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use tokio::sync::mpsc;
use turul_a2a::card_builder::{AgentCardBuilder, AgentSkillBuilder};
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a_types::{Artifact, Message, Part, Task};

/// Header the framework uses to echo the `TaskPushNotificationConfig.token`
/// back to the receiver.
pub const TOKEN_HEADER: &str = "X-Turul-Push-Token";

/// Header carrying the task's event sequence (monotonic per task).
pub const EVENT_SEQUENCE_HEADER: &str = "X-Turul-Event-Sequence";

/// Executor: echoes the caller's text, sleeps `delay_ms`, then commits
/// terminal Completed. Slow on purpose so the caller can register a
/// push config between `send` and terminal.
pub struct CallbackExecutor {
    pub delay_ms: u64,
}

#[async_trait]
impl AgentExecutor for CallbackExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let echoed = message.text_parts().join(" ");
        let reply = if echoed.is_empty() {
            "no input".to_string()
        } else {
            format!("echo: {echoed}")
        };

        if self.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        }
        if ctx.cancellation.is_cancelled() {
            ctx.events
                .cancelled(Some("client cancelled before completion".into()))
                .await?;
            return Ok(());
        }

        let artifact = Artifact::new("callback-result", vec![Part::text(reply)]);
        ctx.events.emit_artifact(artifact, false, true).await?;
        ctx.events.complete(None).await?;
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Callback Agent", "0.1.0")
            .description("Fires a webhook callback when a task terminates")
            .url("http://localhost:3003/jsonrpc", "JSONRPC", "1.0")
            .provider("Example Org", "https://example.com")
            .push_notifications(true)
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .skill(
                AgentSkillBuilder::new(
                    "callback",
                    "Callback",
                    "Echoes the input with a delay; fires a push callback on completion",
                )
                .tags(vec!["push", "callback", "demo"])
                .examples(vec!["hello"])
                .build(),
            )
            .build()
            .expect("Callback agent card should be valid")
    }
}

/// A single webhook call observed by the receiver. `body` is the raw
/// JSON the framework POSTed (the proto-JSON form of the terminal
/// `Task`).
#[derive(Debug, Clone)]
pub struct ReceivedWebhook {
    pub token_header: Option<String>,
    pub event_sequence_header: Option<String>,
    pub body: serde_json::Value,
}

#[derive(Clone)]
struct ReceiverState {
    tx: Arc<mpsc::UnboundedSender<ReceivedWebhook>>,
    expected_token: Arc<String>,
}

async fn webhook_handler(
    State(state): State<ReceiverState>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, &'static str) {
    let token_header = headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let event_sequence_header = headers
        .get(EVENT_SEQUENCE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // Validate the shared-secret token. In production this is the
    // primary defence against forged webhooks from arbitrary
    // internet callers — reject anything that doesn't match.
    if token_header.as_deref() != Some(state.expected_token.as_str()) {
        let _ = state.tx.send(ReceivedWebhook {
            token_header,
            event_sequence_header,
            body: serde_json::json!({"_rejected_reason": "token mismatch"}),
        });
        return (StatusCode::UNAUTHORIZED, "invalid push token");
    }

    let parsed: serde_json::Value =
        serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body));
    let _ = state.tx.send(ReceivedWebhook {
        token_header,
        event_sequence_header,
        body: parsed,
    });
    (StatusCode::OK, "ok")
}

/// Spawn an HTTP webhook receiver on the given listener. Every
/// received call is forwarded on `tx` (so callers — binary or test —
/// can drain them however they like). Returns the JoinHandle so the
/// caller can abort on shutdown and avoid leaking the listener.
///
/// Token validation is enforced: calls whose `X-Turul-Push-Token`
/// header does not match `expected_token` are rejected with 401 and
/// surfaced with a `_rejected_reason` marker in `body`.
pub fn spawn_webhook_receiver(
    listener: tokio::net::TcpListener,
    tx: mpsc::UnboundedSender<ReceivedWebhook>,
    expected_token: impl Into<String>,
) -> tokio::task::JoinHandle<()> {
    let state = ReceiverState {
        tx: Arc::new(tx),
        expected_token: Arc::new(expected_token.into()),
    };
    let app = Router::new()
        .route("/webhook", post(webhook_handler))
        .with_state(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    })
}
