//! Lambda A2A agent — for local testing with cargo-lambda.
//!
//! Run: cargo lambda watch -p lambda-agent
//! Test: cargo lambda invoke lambda-agent --data-ascii '{"httpMethod":"GET","path":"/.well-known/agent-card.json"}'

use turul_a2a::card_builder::{AgentCardBuilder, AgentSkillBuilder};
use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_aws_lambda::LambdaA2aServerBuilder;
use turul_a2a_types::{Message, Task};

struct LambdaEchoExecutor {
    public_url: String,
}

#[async_trait::async_trait]
impl AgentExecutor for LambdaEchoExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
        task.push_text_artifact("lambda-result", "Lambda Echo", "Hello from Lambda!");
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Lambda Echo Agent", "0.1.0")
            .description("A2A agent running on AWS Lambda")
            .url(self.public_url.as_str(), "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .skill(
                AgentSkillBuilder::new(
                    "lambda-echo",
                    "Lambda Echo",
                    "Echoes a fixed greeting back as an artifact. Demonstrates the \
                     A2A executor + axum router running inside the AWS Lambda \
                     adapter via a single Function URL invocation.",
                )
                .tags(vec!["echo", "lambda", "demo"])
                .examples(vec!["Send any text — the agent always replies with a fixed greeting."])
                .build(),
            )
            .build()
            .expect("Lambda agent card should be valid")
    }
}

#[tokio::main]
async fn main() -> Result<(), lambda_runtime::Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .without_time()
        .init();

    // Agent card advertises this URL in `/.well-known/agent-card.json`.
    // Discovery clients connect here — a wrong value breaks discovery.
    // Set it to the Function URL / API Gateway endpoint for this Lambda.
    let public_url = public_url_or_warn();

    LambdaA2aServerBuilder::new()
        .executor(LambdaEchoExecutor { public_url })
        // Demo: in-memory storage, no push-notification delivery. If
        // you want push delivery, wire both `.storage(store.clone())`
        // and `.push_delivery_store(store)` on a backend that
        // implements `A2aPushDeliveryStore`, and flip the
        // `with_push_dispatch_enabled(true)` flag on the storage. The
        // builder rejects mismatched combinations.
        .storage(InMemoryA2aStorage::new())
        .run()
        .await
}

fn public_url_or_warn() -> String {
    match std::env::var("A2A_PUBLIC_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            tracing::warn!(
                "A2A_PUBLIC_URL not set; agent card will advertise a placeholder URL. \
                 Set this env var to the real Function URL / API Gateway endpoint."
            );
            "https://lambda.invalid".to_string()
        }
    }
}
