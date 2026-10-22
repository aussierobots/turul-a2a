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

fn server() -> Result<A2aServer, A2aError> {
    A2aServer::builder()
        .executor(SkillSecurityExecutor)
        .bind(([0, 0, 0, 0], 3006))
        .build()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let server = server()?;

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

/// In-process tests: the same server `main` runs, driven through its
/// router without binding a socket.
#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn call(req: Request<Body>) -> (u16, Value) {
        let resp = super::server()
            .unwrap()
            .into_router()
            .oneshot(req)
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// Only `secure-summary` advertises a skill-level requirement, and it
    /// names the agent-level `bearer` scheme.
    #[tokio::test]
    async fn card_advertises_bearer_requirement_on_secure_summary_only() {
        let (status, card) = call(
            Request::get("/.well-known/agent-card.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, 200);
        assert!(card["securitySchemes"]["bearer"].is_object(), "{card:#}");

        let skills = card["skills"].as_array().expect("skills array");
        let by_id = |id: &str| {
            skills
                .iter()
                .find(|s| s["id"] == id)
                .unwrap_or_else(|| panic!("skill {id} missing: {card:#}"))
        };
        assert!(by_id("public-echo").get("securityRequirements").is_none());
        assert_eq!(
            by_id("secure-summary")["securityRequirements"],
            json!([{ "schemes": { "bearer": {} } }])
        );
    }

    /// Declaration-only: with no auth middleware, a request carrying no
    /// credentials still runs.
    #[tokio::test]
    async fn requests_succeed_without_credentials() {
        let body = json!({
            "message": {"messageId": "m-1", "role": "ROLE_USER", "parts": [{"text": "hello"}]}
        });
        let (status, resp) = call(
            Request::post("/message:send")
                .header("content-type", "application/json")
                .header("a2a-version", "1.0")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(resp["task"]["status"]["state"], "TASK_STATE_COMPLETED");
        assert_eq!(
            resp["task"]["artifacts"][0]["parts"][0]["text"],
            "received: hello"
        );
    }
}
