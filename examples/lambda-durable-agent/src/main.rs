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
//! ## Shared storage
//!
//! The request Lambda and the worker Lambda run in **different**
//! containers, so the two-Lambda topology requires a shared backend.
//! This example uses `DynamoDbA2aStorage` — the worker reads the task
//! this Lambda wrote, runs the executor, and commits the terminal via
//! the same CAS-guarded atomic store (ADR-009 same-backend
//! requirement). Deploy the five DynamoDB tables from
//! `examples/lambda-infra/cloudformation.yaml` before running.

use std::sync::Arc;

use async_trait::async_trait;
use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::storage::dynamodb::{DynamoDbA2aStorage, DynamoDbConfig};
use turul_a2a_aws_lambda::LambdaA2aServerBuilder;
use turul_a2a_types::{Message, Task};

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
        task.append_artifact(turul_a2a_types::Artifact::new(
            "durable-echo",
            vec![turul_a2a_types::Part::text(body)],
        ));
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Durable Echo Agent", "0.1.0")
            .description(
                "ADR-018 demo: Lambda durable executor continuation via SQS. \
                 HTTP invocation enqueues; SQS invocation runs the executor. \
                 Executor echoes the incoming text, task_id, context_id, and \
                 metadata keys so the payload-survival invariant is visible \
                 in the task's artifact.",
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

    let dynamodb_storage = DynamoDbA2aStorage::new(DynamoDbConfig::default())
        .await
        .map_err(|e| lambda_http::Error::from(format!("dynamodb storage build failed: {e}")))?;

    // Pure HTTP Lambda — it accepts `message:send` and enqueues
    // durable jobs onto SQS, but it is not itself triggered by SQS.
    // The consumer side is `lambda-durable-worker`.
    //
    // DynamoDB-backed storage so the worker Lambda (different
    // container, same AWS account) can load the task this Lambda
    // enqueued. Deploy the five tables from
    // examples/lambda-infra/cloudformation.yaml before running.
    //
    // `with_push_dispatch_enabled(true)` omitted — this demo doesn't
    // register push configs, so the builder correctly rejects the
    // inconsistent pairing with no `push_delivery_store`. Production
    // deployments that want push delivery wire
    // `.push_delivery_store(storage.clone())` and flip the flag in
    // tandem.
    //
    // `.with_sqs_return_immediately(queue_url, sqs)` re-enables
    // `supports_return_immediately` on the RuntimeConfig as a side
    // effect of supplying the queue (capability, not intent).
    LambdaA2aServerBuilder::new()
        .executor(DurableEchoExecutor)
        .storage(dynamodb_storage)
        .with_sqs_return_immediately(queue_url, sqs)
        .build()
        .map_err(|e| lambda_http::Error::from(format!("builder error: {e}")))?
        .run_http_only()
        .await
}
