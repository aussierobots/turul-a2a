//! Auth Agent — demonstrates API Key authentication with turul-a2a.
//!
//! Run: cargo run -p auth-agent
//!
//! Test (no auth → 401):
//!   curl -s http://localhost:3001/message:send \
//!     -H 'Content-Type: application/json' \
//!     -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"hello"}]}}'
//!
//! Test (with auth → 200):
//!   curl -s http://localhost:3001/message:send \
//!     -H 'Content-Type: application/json' \
//!     -H 'X-API-Key: demo-key-alice' \
//!     -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"hello"}]}}'
//!
//! Agent card (public, no auth):
//!   curl -s http://localhost:3001/.well-known/agent-card.json | jq .securitySchemes

use std::collections::HashMap;
use std::sync::Arc;

use turul_a2a::card_builder::{AgentCardBuilder, AgentSkillBuilder};
use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::A2aServer;
use turul_a2a_auth::{ApiKeyMiddleware, StaticApiKeyLookup};
use turul_a2a_types::{Message, Task};

struct AuthEchoExecutor;

#[async_trait::async_trait]
impl AgentExecutor for AuthEchoExecutor {
    async fn execute(&self, task: &mut Task, _message: &Message) -> Result<(), A2aError> {
        let mut proto = task.as_proto().clone();
        proto.status = Some(turul_a2a_proto::TaskStatus {
            state: turul_a2a_proto::TaskState::Completed.into(),
            message: None,
            timestamp: None,
        });
        proto.artifacts.push(turul_a2a_proto::Artifact {
            artifact_id: uuid::Uuid::now_v7().to_string(),
            name: "Auth Echo".into(),
            description: String::new(),
            parts: vec![turul_a2a_proto::Part {
                content: Some(turul_a2a_proto::part::Content::Text(
                    "Authenticated echo response".into(),
                )),
                metadata: None,
                filename: String::new(),
                media_type: "text/plain".into(),
            }],
            metadata: None,
            extensions: vec![],
        });
        *task = Task::try_from(proto).unwrap();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        // Security schemes are auto-populated from middleware by the builder
        AgentCardBuilder::new("Auth Echo Agent", "0.1.0")
            .description("Demonstrates API Key authentication")
            .url("http://localhost:3001", "JSONRPC", "1.0")
            .provider("Aussie Robots", "https://github.com/aussierobots/turul-a2a")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .skill(
                AgentSkillBuilder::new("echo", "Auth Echo", "Echoes back, requires authentication")
                    .tags(vec!["echo", "auth"])
                    .build(),
            )
            .build()
            .expect("Auth agent card should be valid")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // Configure API key auth with demo keys
    let mut keys = HashMap::new();
    keys.insert("demo-key-alice".to_string(), "alice".to_string());
    keys.insert("demo-key-bob".to_string(), "bob".to_string());

    let api_key_auth = ApiKeyMiddleware::new(
        Arc::new(StaticApiKeyLookup::new(keys)),
        "X-API-Key",
    );

    let server = A2aServer::builder()
        .executor(AuthEchoExecutor)
        .middleware(Arc::new(api_key_auth))
        .bind(([0, 0, 0, 0], 3001))
        .build()?;

    println!("Auth Echo Agent listening on http://0.0.0.0:3001");
    println!("Agent card: http://localhost:3001/.well-known/agent-card.json");
    println!();
    println!("Without auth (should 401):");
    println!("  curl -s http://localhost:3001/message:send \\");
    println!("    -H 'Content-Type: application/json' \\");
    println!("    -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"hello\"}}]}}}}'");
    println!();
    println!("With auth (should 200):");
    println!("  curl -s http://localhost:3001/message:send \\");
    println!("    -H 'Content-Type: application/json' \\");
    println!("    -H 'X-API-Key: demo-key-alice' \\");
    println!("    -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"hello\"}}]}}}}'");

    server.run().await?;
    Ok(())
}
