//! ADR-018 single-Lambda demo — one function handles both HTTP and SQS.
//!
//! Deployed with `ReservedConcurrency=1` so the HTTP and SQS
//! invocations reuse the same warm container, letting
//! `InMemoryA2aStorage` survive the hand-off without a shared
//! backend. Demo-only — see `lambda-durable-agent` +
//! `lambda-durable-worker` for the production shape with shared
//! DynamoDB.
//!
//! The mixed-event dispatch (HTTP envelope conversion + classify +
//! routing) is hidden behind [`LambdaA2aServerBuilder::run`]; this
//! `main.rs` only handles storage setup and wiring and ends with a
//! single `.run().await?` call.

use std::sync::Arc;

use async_trait::async_trait;
use lambda_runtime::Error;
use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_aws_lambda::LambdaA2aServerBuilder;
use turul_a2a_types::{Artifact, Message, Part, Task};

/// Echoes the incoming payload so adopters can verify that the
/// original message, task id, and context id survived the HTTP → SQS
/// → dequeue → executor hop. Also lists `Message.metadata` keys (not
/// values) so callers can see their correlation fields arrived
/// without exposing their contents. Does not surface auth identity
/// or claims.
struct DurableEchoExecutor;

#[async_trait]
impl AgentExecutor for DurableEchoExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        let text = msg.joined_text();
        let metadata_keys = msg.metadata_keys();
        let context_id = ctx.context_id.as_deref().unwrap_or("");

        tracing::info!(
            task_id = %ctx.task_id,
            context_id = %context_id,
            text = %text,
            metadata_keys = ?metadata_keys,
            "durable executor echoed incoming payload"
        );

        let body = format!(
            "echoed from durable executor\n  text: {text}\n  task_id: {task_id} context_id: {context_id}\n  metadata_keys: [{keys}]",
            task_id = ctx.task_id,
            keys = metadata_keys.join(", "),
        );
        task.append_artifact(Artifact::new("durable-echo", vec![Part::text(body)]));
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Durable Echo Agent (single Lambda)", "0.1.0")
            .description(
                "ADR-018 single-Lambda demo: one function handles both \
                 HTTP and SQS. ReservedConcurrency=1 pins a single container \
                 so in-memory storage works across the HTTP → SQS hand-off. \
                 Executor echoes the incoming text, task_id, context_id, and \
                 metadata keys so the payload-survival invariant is visible \
                 in the task's artifact.",
            )
            .url("https://lambda.example.com", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .streaming(false)
            .build()
            .expect("single-lambda agent card should be valid")
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .without_time()
        .init();

    let queue_url = std::env::var("A2A_EXECUTOR_QUEUE_URL").map_err(|_| {
        Error::from(
            "A2A_EXECUTOR_QUEUE_URL is required — set it to the SQS queue URL \
             (default name: turul-a2a-durable-executor-demo)",
        )
    })?;

    let aws = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let sqs = Arc::new(aws_sdk_sqs::Client::new(&aws));

    LambdaA2aServerBuilder::new()
        .executor(DurableEchoExecutor)
        .storage(InMemoryA2aStorage::new())
        .with_sqs_return_immediately(queue_url, sqs)
        .run()
        .await
}
