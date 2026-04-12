//! Lambda A2A agent — for local testing with cargo-lambda.
//!
//! Run: cargo lambda watch -p lambda-agent
//! Test: cargo lambda invoke lambda-agent --data-ascii '{"httpMethod":"GET","path":"/.well-known/agent-card.json"}'

use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_aws_lambda::LambdaA2aServerBuilder;
use turul_a2a_types::{Message, Task};

struct LambdaEchoExecutor;

#[async_trait::async_trait]
impl AgentExecutor for LambdaEchoExecutor {
    async fn execute(&self, task: &mut Task, _msg: &Message) -> Result<(), A2aError> {
        task.push_text_artifact("lambda-result", "Lambda Echo", "Hello from Lambda!");
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Lambda Echo Agent", "0.1.0")
            .description("A2A agent running on AWS Lambda")
            .url("https://lambda.example.com", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build()
            .expect("Lambda agent card should be valid")
    }
}

#[tokio::main]
async fn main() -> Result<(), lambda_http::Error> {
    let handler = LambdaA2aServerBuilder::new()
        .executor(LambdaEchoExecutor)
        .storage(InMemoryA2aStorage::new())
        .build()
        .map_err(|e| lambda_http::Error::from(format!("{e}")))?;

    lambda_http::run(lambda_http::service_fn(move |event| {
        let handler = handler.clone();
        async move { handler.handle(event).await }
    }))
    .await
}
