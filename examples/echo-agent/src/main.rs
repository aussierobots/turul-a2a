//! Echo Agent — minimal A2A agent that echoes messages back.
//!
//! Run: cargo run -p echo-agent
//! Test: curl -X POST http://localhost:3000/message:send \
//!         -H 'Content-Type: application/json' \
//!         -H 'a2a-version: 1.0' \
//!         -d '{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"hello"}]}}'

use turul_a2a::A2aServer;
use turul_a2a::card_builder::{AgentCardBuilder, AgentSkillBuilder};
use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a_types::{Message, Task};

struct EchoExecutor;

#[async_trait::async_trait]
impl AgentExecutor for EchoExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        message: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
        // Echo the user's text parts back as an artifact
        let parts = message.text_parts();
        let echo_text = format!("Echo: {}", parts.join(" "));

        task.push_text_artifact(uuid::Uuid::now_v7().to_string(), "Echo Response", echo_text);
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Echo Agent", "0.1.0")
            .description("Echoes user messages back as artifacts")
            .url("http://localhost:3000/jsonrpc", "JSONRPC", "1.0")
            .provider("Example Org", "https://example.com")
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

fn server() -> Result<A2aServer, A2aError> {
    A2aServer::builder()
        .executor(EchoExecutor)
        .bind(([0, 0, 0, 0], 3000))
        .build()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let server = server()?;

    println!("Echo Agent listening on http://0.0.0.0:3000");
    println!("Agent card: http://localhost:3000/.well-known/agent-card.json");
    println!();
    println!("Try: curl -X POST http://localhost:3000/message:send \\");
    println!("  -H 'Content-Type: application/json' \\");
    println!("  -H 'a2a-version: 1.0' \\");
    println!(
        "  -d '{{\"message\":{{\"messageId\":\"1\",\"role\":\"ROLE_USER\",\"parts\":[{{\"text\":\"hello\"}}]}}}}'"
    );

    server.run().await?;
    Ok(())
}

/// In-process tests: the same server `main` runs, driven through its
/// router without binding a socket.
#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn call(req: axum::http::Request<Body>) -> (u16, Value) {
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

    #[tokio::test]
    async fn send_echoes_text_as_artifact_and_completes() {
        let body = json!({
            "message": {"messageId": "m-1", "role": "ROLE_USER", "parts": [{"text": "hello"}]}
        });
        let (status, resp) = call(
            axum::http::Request::post("/message:send")
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
            "Echo: hello"
        );
    }

    #[tokio::test]
    async fn agent_card_advertises_echo_skill() {
        let (status, card) = call(
            axum::http::Request::get("/.well-known/agent-card.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(card["name"], "Echo Agent");
        assert_eq!(card["skills"][0]["id"], "echo");
    }
}
