//! Single-Lambda durable executor demo — one function handles both
//! HTTP and SQS.
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
use turul_a2a::card_builder::{AgentCardBuilder, AgentSkillBuilder};
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
struct DurableEchoExecutor {
    public_url: String,
}

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
                "Single-Lambda durable executor demo: one function handles \
                 both HTTP and SQS. Deployed with ReservedConcurrency=1 so \
                 the same warm container handles the request and the \
                 SQS-triggered invocation, letting in-memory storage survive \
                 the hand-off. The executor echoes the incoming text, task \
                 id, context id, and message metadata keys so the payload \
                 survives the HTTP → SQS → dequeue hop intact.",
            )
            .url(self.public_url.as_str(), "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .streaming(false)
            .skill(
                AgentSkillBuilder::new(
                    "durable-echo",
                    "Durable Echo (single Lambda)",
                    "Echoes the incoming text plus the framework-assigned task id, \
                     context id, and the *keys* of any client-supplied message \
                     metadata. The echo runs in the SQS-triggered invocation, not \
                     the HTTP one — proving the payload survives the HTTP → SQS → \
                     dequeue → executor hop intact, on a single Lambda function.",
                )
                .tags(vec!["echo", "durable", "sqs", "single-lambda"])
                .examples(vec![
                    "Send `{\"message\":{...,\"parts\":[{\"text\":\"probe-42\"}],\
                     \"metadata\":{\"trigger_id\":\"trig-x\",\"attempt\":1}},\
                     \"configuration\":{\"returnImmediately\":true}}`. After \
                     ~5 seconds, GET /tasks/{id} shows the terminal artifact \
                     containing the probe text, both ids, and `metadata_keys: \
                     [attempt, trigger_id]`.",
                ])
                .build(),
            )
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
        Error::from("A2A_EXECUTOR_QUEUE_URL is required — set it to the SQS queue URL")
    })?;
    // Agent card advertises this URL in /.well-known/agent-card.json.
    // Discovery clients use it — a wrong value breaks discovery.
    let public_url = match std::env::var("A2A_PUBLIC_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            tracing::warn!(
                "A2A_PUBLIC_URL not set; agent card will advertise a placeholder URL. \
                 Set this env var to the Function URL for this Lambda."
            );
            "https://lambda.invalid".to_string()
        }
    };

    let aws = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let sqs = Arc::new(aws_sdk_sqs::Client::new(&aws));

    LambdaA2aServerBuilder::new()
        .executor(DurableEchoExecutor { public_url })
        .storage(InMemoryA2aStorage::new())
        .with_sqs_return_immediately(queue_url, sqs)
        .run()
        .await
}
