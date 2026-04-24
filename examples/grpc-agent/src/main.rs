//! gRPC Agent — tonic server binary that exposes the full A2A
//! `lf.a2a.v1.A2AService` surface over gRPC (ADR-014).
//!
//! Run: `cargo run -p grpc-agent --bin grpc-agent`
//!
//! The companion `grpc-client` binary in the same crate demonstrates
//! the ergonomic `turul_a2a_client::grpc::A2aGrpcClient` verbs against
//! this server. See README.md for the full flow.

use std::time::Duration;

use async_trait::async_trait;
use turul_a2a::A2aServer;
use turul_a2a::card_builder::{AgentCardBuilder, AgentSkillBuilder};
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Artifact, Message, Part, Task};

/// Default gRPC port. Chosen to not collide with the other examples
/// (echo=3000, auth=3001, streaming=3002, callback=3003/3004).
const GRPC_PORT: u16 = 3005;

/// Executor that emits two artifact chunks then completes. Slow enough
/// (50 ms between chunks) for the `grpc-client --stream` flow to show
/// chunking visibly.
struct GrpcEchoExecutor;

#[async_trait]
impl AgentExecutor for GrpcEchoExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        message: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let input = message.text_parts().join(" ");
        let reply = if input.is_empty() {
            "hello from grpc".to_string()
        } else {
            format!("echo: {input}")
        };

        // Chunk #1 (append=false creates the artifact), last_chunk=false
        let first = reply.chars().take(reply.len() / 2).collect::<String>();
        let artifact = Artifact::new("grpc-out", vec![Part::text(first)]);
        ctx.events.emit_artifact(artifact, false, false).await?;

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Chunk #2 (append=true continues the same artifact), last_chunk=true
        let rest = reply.chars().skip(reply.len() / 2).collect::<String>();
        let artifact = Artifact::new("grpc-out", vec![Part::text(rest)]);
        ctx.events.emit_artifact(artifact, true, true).await?;

        ctx.events.complete(None).await?;
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("gRPC Echo Agent", "0.1.0")
            .description("A2A v1.0 agent exposed over gRPC via ADR-014")
            .url(format!("grpc://127.0.0.1:{GRPC_PORT}"), "GRPC", "1.0")
            .provider("Example Org", "https://example.com")
            .streaming(true)
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .skill(
                AgentSkillBuilder::new(
                    "echo",
                    "Echo",
                    "Echoes back the caller's text in two streamed chunks",
                )
                .tags(vec!["grpc", "echo", "streaming"])
                .examples(vec!["hello world"])
                .build(),
            )
            .build()
            .expect("agent card")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let addr = format!("127.0.0.1:{GRPC_PORT}").parse()?;

    let server = A2aServer::builder()
        .executor(GrpcEchoExecutor)
        .storage(InMemoryA2aStorage::new())
        // `bind` is unused when we construct the tonic router ourselves,
        // but the builder requires a value to satisfy validation.
        .bind(([127, 0, 0, 1], GRPC_PORT))
        .build()?;

    let router = server.into_tonic_router();

    println!("gRPC A2A agent listening on {addr}");
    println!();
    println!("Try the companion client:");
    println!("  cargo run -p grpc-agent --bin grpc-client -- send   \"hello world\"");
    println!("  cargo run -p grpc-agent --bin grpc-client -- stream \"stream this\"");
    println!("  cargo run -p grpc-agent --bin grpc-client -- list");
    println!();
    println!("Or use grpcurl (if you enable --features grpc-reflection on the server):");
    println!("  grpcurl -plaintext 127.0.0.1:{GRPC_PORT} list");

    router.serve(addr).await?;
    Ok(())
}
