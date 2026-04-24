//! Skill-Security Agent — demonstrates declaration-only skill-level
//! security requirements on the agent card.
//!
//! Two skills on one agent:
//! - `public-echo`: no skill-level security metadata.
//! - `secure-summary`: advertises a bearer requirement at the skill
//!   level.
//!
//! Both skills run under the **same** agent-level runtime. No auth
//! middleware is installed, so every request succeeds regardless of
//! which skill a caller has in mind — declaring a skill-level
//! requirement does NOT install a gatekeeper. The bearer scheme is
//! declared at the agent card level so the skill requirement
//! resolves against a defined scheme and the card passes validation.
//!
//! To see agent-level enforcement in action, look at
//! `examples/auth-agent` instead — it installs real middleware.

use std::collections::HashMap;

use turul_a2a::A2aServer;
use turul_a2a::card_builder::{AgentCardBuilder, AgentSkillBuilder};
use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a_types::{Message, Task};

struct SkillSecurityExecutor;

#[async_trait::async_trait]
impl AgentExecutor for SkillSecurityExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        message: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
        let parts = message.text_parts();
        task.push_text_artifact(
            uuid::Uuid::now_v7().to_string(),
            "Response",
            format!("received: {}", parts.join(" ")),
        );
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        let bearer_scheme = turul_a2a_proto::SecurityScheme {
            scheme: Some(
                turul_a2a_proto::security_scheme::Scheme::HttpAuthSecurityScheme(
                    turul_a2a_proto::HttpAuthSecurityScheme {
                        description: "Optional bearer token for the secure-summary skill \
                                       (advertised only; not enforced by this example)."
                            .into(),
                        scheme: "Bearer".into(),
                        bearer_format: "JWT".into(),
                    },
                ),
            ),
        };

        let mut bearer_ref = HashMap::new();
        bearer_ref.insert(
            "bearer".to_string(),
            turul_a2a_proto::StringList { list: vec![] },
        );
        let bearer_requirement = turul_a2a_proto::SecurityRequirement {
            schemes: bearer_ref,
        };

        let public_echo = AgentSkillBuilder::new(
            "public-echo",
            "Public Echo",
            "Echoes input back. No advertised security requirement.",
        )
        .tags(vec!["echo", "public"])
        .examples(vec!["Say hello"])
        .build();

        let secure_summary = AgentSkillBuilder::new(
            "secure-summary",
            "Secure Summary",
            "Advertises a bearer requirement at the skill level for \
             discovery clients. The framework does not enforce it — \
             authorization is governed by agent-level middleware.",
        )
        .tags(vec!["summary", "auth-advertised"])
        .examples(vec!["Summarize this text"])
        .security_requirements(vec![bearer_requirement])
        .build();

        AgentCardBuilder::new("Skill-Security Agent", "0.1.0")
            .description("Demo: declaration-only skill-level security requirements.")
            .url("http://localhost:3006/jsonrpc", "JSONRPC", "1.0")
            .provider("Example Org", "https://example.com")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .security_scheme("bearer", bearer_scheme)
            .skill(public_echo)
            .skill(secure_summary)
            .build()
            .expect("skill-security-agent card must satisfy declaration-truthfulness validation")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let server = A2aServer::builder()
        .executor(SkillSecurityExecutor)
        .bind(([0, 0, 0, 0], 3006))
        .build()?;

    println!("Skill-Security Agent listening on http://0.0.0.0:3006");
    println!("Agent card: http://localhost:3006/.well-known/agent-card.json");
    println!();
    println!("Inspect advertised per-skill requirements:");
    println!(
        "  curl -s http://localhost:3006/.well-known/agent-card.json \\\n    | jq '.skills[] | {{id, securityRequirements}}'"
    );
    println!();
    println!("Verify requests still succeed without auth (no middleware installed):");
    println!("  curl -i http://localhost:3006/message:send \\");
    println!("    -H 'Content-Type: application/json' \\");
    println!("    -H 'a2a-version: 1.0' \\");
    println!(
        "    -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"hello\"}}]}}}}'"
    );

    server.run().await?;
    Ok(())
}
