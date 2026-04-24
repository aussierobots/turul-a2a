//! Streaming Agent library — the executor implementation lives here
//! so that both `src/main.rs` (the runnable binary) and
//! `tests/smoke.rs` (the integration smoke test) can share it.
//!
//! Demonstrates token-by-token SSE streaming via the
//! `EventSink::emit_artifact(append, last_chunk)` contract. The
//! executor tokenises a canned response and emits each word as a
//! chunk appended to a single artifact; the final chunk flips
//! `last_chunk=true` and then `complete(None)` fires the terminal
//! StatusUpdate.

use std::time::Duration;

use async_trait::async_trait;
use turul_a2a::card_builder::{AgentCardBuilder, AgentSkillBuilder};
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a_types::{Artifact, Message, Part, Task};

/// Artifact id used for every chunk. Reused across emits so
/// `append=true` merges them into a single growing artifact rather
/// than producing N independent ones — this is the point of the demo.
pub const ARTIFACT_ID: &str = "stream-1";

/// Deterministic suffix so the smoke test can assert the reassembled
/// text exactly.
pub const CANNED_SUFFIX: &str = "the quick brown fox jumps over the lazy dog";

/// Streaming executor. `delay_ms` between chunks controls how the
/// stream looks on the wire — set to ~100 ms for a human to watch
/// over `curl --no-buffer`, set to 0 in tests so the smoke suite
/// finishes in milliseconds.
pub struct StreamingExecutor {
    pub delay_ms: u64,
}

#[async_trait]
impl AgentExecutor for StreamingExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let prefix = message.text_parts().join(" ");
        let tokens: Vec<String> = if prefix.is_empty() {
            CANNED_SUFFIX.split_whitespace().map(String::from).collect()
        } else {
            std::iter::once(format!("[echo: {prefix}]"))
                .chain(CANNED_SUFFIX.split_whitespace().map(String::from))
                .collect()
        };
        let last_idx = tokens.len().saturating_sub(1);

        for (i, tok) in tokens.iter().enumerate() {
            if ctx.cancellation.is_cancelled() {
                ctx.events
                    .cancelled(Some("client cancelled mid-stream".into()))
                    .await?;
                return Ok(());
            }

            let chunk_text = if i == 0 {
                tok.clone()
            } else {
                format!(" {tok}")
            };
            let artifact = Artifact::new(ARTIFACT_ID, vec![Part::text(chunk_text)]);
            ctx.events
                .emit_artifact(
                    artifact,
                    /* append */ true,
                    /* last_chunk */ i == last_idx,
                )
                .await?;

            if i != last_idx && self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
        }

        ctx.events.complete(None).await?;
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Streaming Agent", "0.1.0")
            .description("Streams a canned response token-by-token over SSE")
            .url("http://localhost:3002/jsonrpc", "JSONRPC", "1.0")
            .provider("Example Org", "https://example.com")
            .streaming(true)
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .skill(
                AgentSkillBuilder::new(
                    "stream",
                    "Stream",
                    "Emits the canned suffix token-by-token, one artifact chunk per word",
                )
                .tags(vec!["streaming", "demo"])
                .examples(vec!["say something"])
                .build(),
            )
            .build()
            .expect("Streaming agent card should be valid")
    }
}
