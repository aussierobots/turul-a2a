//! Echo Agent — minimal A2A agent that echoes messages back.
//!
//! Run: cargo run -p echo-agent
//! Test: curl -X POST http://localhost:3000/message:send \
//!         -H 'Content-Type: application/json' \
//!         -H 'a2a-version: 1.0' \
//!         -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"hello"}]}}'

use turul_a2a::card_builder::{AgentCardBuilder, AgentSkillBuilder};
use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::A2aServer;
use turul_a2a_types::{Message, Task};

struct EchoExecutor;

#[async_trait::async_trait]
impl AgentExecutor for EchoExecutor {
    async fn execute(&self, task: &mut Task, message: &Message, _ctx: &turul_a2a::executor::ExecutionContext) -> Result<(), A2aError> {
        // Echo the user's text parts back as an artifact
        let parts = message.text_parts();
        let echo_text = format!("Echo: {}", parts.join(" "));

        task.push_text_artifact(
            uuid::Uuid::now_v7().to_string(),
            "Echo Response",
            echo_text,
        );
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Echo Agent", "0.1.0")
            .description("Echoes user messages back as artifacts")
            .url("http://localhost:3000/jsonrpc", "JSONRPC", "1.0")
            .provider("Aussie Robots", "https://github.com/aussierobots/turul-a2a")
            .streaming(true)
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .skill(
                AgentSkillBuilder::new("echo", "Echo", "Echoes any text input back")
                    .tags(vec!["echo", "test"])
                    .examples(vec!["Say hello"])
                    .build(),
            )
            .build()
            .expect("Echo agent card should be valid")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let server = A2aServer::builder()
        .executor(EchoExecutor)
        .bind(([0, 0, 0, 0], 3000))
        .build()?;

    println!("Echo Agent listening on http://0.0.0.0:3000");
    println!("Agent card: http://localhost:3000/.well-known/agent-card.json");
    println!();
    println!("Try: curl -X POST http://localhost:3000/message:send \\");
    println!("  -H 'Content-Type: application/json' \\");
    println!("  -H 'a2a-version: 1.0' \\");
    println!("  -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"hello\"}}]}}}}'");

    server.run().await?;
    Ok(())
}
