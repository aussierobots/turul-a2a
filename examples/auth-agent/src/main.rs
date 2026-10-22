//! Auth Agent — demonstrates API Key authentication with turul-a2a.
//!
//! Run: cargo run -p auth-agent
//!
//! Test (no auth → 401):
//!   curl -s http://localhost:3001/message:send \
//!     -H 'Content-Type: application/json' \
//!     -H 'a2a-version: 1.0' \
//!     -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"hello"}]}}'
//!
//! Test (with auth → 200):
//!   curl -s http://localhost:3001/message:send \
//!     -H 'Content-Type: application/json' \
//!     -H 'a2a-version: 1.0' \
//!     -H 'X-API-Key: demo-key-alice' \
//!     -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"hello"}]}}'
//!
//! Agent card (public, no auth):
//!   curl -s http://localhost:3001/.well-known/agent-card.json | jq .securitySchemes

use std::collections::HashMap;
use std::sync::Arc;

use turul_a2a::A2aServer;
use turul_a2a::card_builder::{AgentCardBuilder, AgentSkillBuilder};
use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a_auth::{ApiKeyMiddleware, StaticApiKeyLookup};
use turul_a2a_types::{Message, Task};

struct AuthEchoExecutor;

#[async_trait::async_trait]
impl AgentExecutor for AuthEchoExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _message: &Message,
        ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
        // Use ExecutionContext to access caller identity
        let response = format!(
            "Authenticated as: {} (tenant: {})",
            ctx.owner,
            ctx.tenant.as_deref().unwrap_or("default"),
        );

        task.push_text_artifact(uuid::Uuid::now_v7().to_string(), "Auth Echo", response);
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        // Security schemes are auto-populated from middleware by the builder
        AgentCardBuilder::new("Auth Echo Agent", "0.1.0")
            .description("Demonstrates API Key authentication")
            .url("http://localhost:3001/jsonrpc", "JSONRPC", "1.0")
            .provider("Example Org", "https://example.com")
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

fn server() -> Result<A2aServer, A2aError> {
    // Configure API key auth with demo keys
    let mut keys = HashMap::new();
    keys.insert("demo-key-alice".to_string(), "alice".to_string());
    keys.insert("demo-key-bob".to_string(), "bob".to_string());

    let api_key_auth = ApiKeyMiddleware::new(Arc::new(StaticApiKeyLookup::new(keys)), "X-API-Key");

    A2aServer::builder()
        .executor(AuthEchoExecutor)
        .middleware(Arc::new(api_key_auth))
        .bind(([0, 0, 0, 0], 3001))
        .build()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let server = server()?;

    println!("Auth Echo Agent listening on http://0.0.0.0:3001");
    println!("Agent card: http://localhost:3001/.well-known/agent-card.json");
    println!();
    println!("Without auth (should 401):");
    println!("  curl -s http://localhost:3001/message:send \\");
    println!("    -H 'Content-Type: application/json' \\");
    println!("    -H 'a2a-version: 1.0' \\");
    println!(
        "    -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"hello\"}}]}}}}'"
    );
    println!();
    println!("With auth (should 200):");
    println!("  curl -s http://localhost:3001/message:send \\");
    println!("    -H 'Content-Type: application/json' \\");
    println!("    -H 'a2a-version: 1.0' \\");
    println!("    -H 'X-API-Key: demo-key-alice' \\");
    println!(
        "    -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"hello\"}}]}}}}'"
    );

    server.run().await?;
    Ok(())
}

/// In-process tests: the same server `main` runs (API-key middleware
/// included), driven through its router without binding a socket.
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
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn send(api_key: Option<&str>) -> Request<Body> {
        let body = json!({
            "message": {"messageId": "m-1", "role": "ROLE_USER", "parts": [{"text": "hello"}]}
        });
        let mut req = Request::post("/message:send")
            .header("content-type", "application/json")
            .header("a2a-version", "1.0");
        if let Some(key) = api_key {
            req = req.header("X-API-Key", key);
        }
        req.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn send_without_api_key_is_unauthorized() {
        let (status, _) = call(send(None)).await;
        assert_eq!(status, 401);
    }

    #[tokio::test]
    async fn send_with_unknown_api_key_is_unauthorized() {
        let (status, _) = call(send(Some("not-a-demo-key"))).await;
        assert_eq!(status, 401);
    }

    #[tokio::test]
    async fn send_with_api_key_runs_as_the_key_owner() {
        let (status, resp) = call(send(Some("demo-key-alice"))).await;
        assert_eq!(status, 200);
        assert_eq!(resp["task"]["status"]["state"], "TASK_STATE_COMPLETED");
        assert_eq!(
            resp["task"]["artifacts"][0]["parts"][0]["text"],
            "Authenticated as: alice (tenant: default)"
        );
    }

    /// The public card is served without credentials and advertises the
    /// middleware's API-key scheme.
    #[tokio::test]
    async fn agent_card_is_public_and_advertises_api_key_scheme() {
        let (status, card) = call(
            Request::get("/.well-known/agent-card.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, 200);
        let schemes = card["securitySchemes"]
            .as_object()
            .expect("securitySchemes object");
        assert_eq!(schemes.len(), 1, "{card:#}");
        let scheme = schemes.values().next().unwrap();
        assert_eq!(
            scheme["apiKeySecurityScheme"]["name"], "X-API-Key",
            "{card:#}"
        );
    }
}
