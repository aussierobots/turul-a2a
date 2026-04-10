//! Echo Agent — minimal A2A agent that echoes messages back.
//!
//! Run: cargo run -p echo-agent
//! Test: curl -X POST http://localhost:3000/message:send \
//!         -H 'Content-Type: application/json' \
//!         -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"hello"}]}}'

use std::collections::HashMap;

use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::A2aServer;
use turul_a2a_types::{Message, Task};

struct EchoExecutor;

#[async_trait::async_trait]
impl AgentExecutor for EchoExecutor {
    async fn execute(&self, task: &mut Task, message: &Message) -> Result<(), A2aError> {
        // Echo the user's message back as an artifact
        let user_text = message
            .as_proto()
            .parts
            .iter()
            .filter_map(|p| match &p.content {
                Some(turul_a2a_proto::part::Content::Text(t)) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");

        let echo_text = format!("Echo: {user_text}");

        // Build completed task with artifact
        let mut proto = task.as_proto().clone();
        proto.status = Some(turul_a2a_proto::TaskStatus {
            state: turul_a2a_proto::TaskState::Completed.into(),
            message: None,
            timestamp: None,
        });
        proto.artifacts.push(turul_a2a_proto::Artifact {
            artifact_id: uuid::Uuid::now_v7().to_string(),
            name: "Echo Response".into(),
            description: String::new(),
            parts: vec![turul_a2a_proto::Part {
                content: Some(turul_a2a_proto::part::Content::Text(echo_text)),
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
        turul_a2a_proto::AgentCard {
            name: "Echo Agent".into(),
            description: "Echoes user messages back as artifacts".into(),
            supported_interfaces: vec![turul_a2a_proto::AgentInterface {
                url: "http://localhost:3000".into(),
                protocol_binding: "JSONRPC".into(),
                tenant: String::new(),
                protocol_version: "1.0".into(),
            }],
            provider: Some(turul_a2a_proto::AgentProvider {
                url: "https://github.com/aussierobots/turul-a2a".into(),
                organization: "Aussie Robots".into(),
            }),
            version: "0.1.0".into(),
            documentation_url: None,
            capabilities: Some(turul_a2a_proto::AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(false),
                extensions: vec![],
                extended_agent_card: Some(false),
            }),
            security_schemes: HashMap::new(),
            security_requirements: vec![],
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![turul_a2a_proto::AgentSkill {
                id: "echo".into(),
                name: "Echo".into(),
                description: "Echoes any text input back".into(),
                tags: vec!["echo".into(), "test".into()],
                examples: vec!["Say hello".into()],
                input_modes: vec![],
                output_modes: vec![],
                security_requirements: vec![],
            }],
            signatures: vec![],
            icon_url: None,
        }
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
    println!("  -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"hello\"}}]}}}}'");

    server.run().await?;
    Ok(())
}
