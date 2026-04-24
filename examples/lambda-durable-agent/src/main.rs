//! Lambda A2A **request** agent with ADR-018 durable executor continuation.
//!
//! This is the HTTP-side half of the ADR-018 Pattern B demo. It
//! handles `POST /message:send` with
//! `configuration.returnImmediately = true`:
//!
//! 1. `core_send_message` size-checks the durable-queue payload,
//!    creates the task as `WORKING`, registers any inline push
//!    config, enqueues a
//!    [`turul_a2a::durable_executor::QueuedExecutorJob`] on the SQS
//!    queue, and returns `200 OK` with the task snapshot.
//! 2. The executor is **not** spawned locally — post-return
//!    `tokio::spawn` is opportunistic on Lambda (ADR-013 §4.4).
//!
//! The SQS side is `examples/lambda-durable-worker`: a separate Lambda
//! function wired via an SQS event source mapping. Both Lambdas share
//! the same storage backend (DynamoDB in production; see
//! `examples/lambda-infra/cloudformation.yaml`).
//!
//! Without the SQS wiring, the Lambda adapter rejects
//! `returnImmediately=true` with `UnsupportedOperationError` per
//! ADR-017. This example flips the capability back on by calling
//! `.with_sqs_return_immediately(queue_url, sqs_client)` —
//! "capability, not intent": the flag cannot be claimed without
//! supplying the queue.
//!
//! ## Deploy
//!
//! See `examples/lambda-durable-agent/README.md` for the full
//! walk-through (SQS queue + DLQ, Function URL, IAM policy, event
//! source mapping to the worker Lambda).
//!
//! ## Storage caveat
//!
//! This example uses `InMemoryA2aStorage` for `cargo check` and
//! local-invoke convenience. Production deployments MUST use a
//! shared backend — the request Lambda and the worker Lambda run in
//! **different** containers, so in-memory state does not survive
//! the cold-start handoff. Swap in `DynamoDbA2aStorage`.

use std::sync::Arc;

use async_trait::async_trait;
use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_aws_lambda::LambdaA2aServerBuilder;
use turul_a2a_types::{Message, Task};

struct DurableEchoExecutor;

#[async_trait]
impl AgentExecutor for DurableEchoExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        task.push_text_artifact(
            "durable-echo",
            "Durable Echo",
            "Hello from the SQS-invoked executor!",
        );
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Durable Echo Agent", "0.1.0")
            .description(
                "ADR-018 demo: Lambda durable executor continuation via SQS. \
                 HTTP invocation enqueues; SQS invocation runs the executor.",
            )
            .url("https://lambda.example.com", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .streaming(false)
            .build()
            .expect("durable agent card should be valid")
    }
}

#[tokio::main]
async fn main() -> Result<(), lambda_http::Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .without_time()
        .init();

    let queue_url = std::env::var("A2A_EXECUTOR_QUEUE_URL").map_err(|_| {
        lambda_http::Error::from(
            "A2A_EXECUTOR_QUEUE_URL is required — set it to the SQS queue URL \
             (default name: turul-a2a-durable-executor-demo)",
        )
    })?;

    let aws = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let sqs = Arc::new(aws_sdk_sqs::Client::new(&aws));

    let handler = LambdaA2aServerBuilder::new()
        .executor(DurableEchoExecutor)
        // InMemoryA2aStorage is for demo convenience only. Real
        // deployments MUST use a shared backend — the worker Lambda
        // runs in a different container and can't see this one's
        // in-memory state. Swap for `DynamoDbA2aStorage`.
        .storage(InMemoryA2aStorage::new().with_push_dispatch_enabled(true))
        // ADR-018: wire the SQS durable executor queue. Re-enables
        // `supports_return_immediately` on the RuntimeConfig as a
        // side effect of supplying the queue (capability, not intent).
        .with_sqs_return_immediately(queue_url, sqs)
        .build()
        .map_err(|e| lambda_http::Error::from(format!("builder error: {e}")))?;

    lambda_http::run(lambda_http::service_fn(move |event| {
        let handler = handler.clone();
        async move { handler.handle(event).await }
    }))
    .await
}
