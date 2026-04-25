//! Lambda A2A **request** agent paired with an SQS worker Lambda for
//! durable executor continuation.
//!
//! This is the HTTP-side half of the two-Lambda demo. It handles
//! `POST /message:send` with `configuration.returnImmediately = true`:
//!
//! 1. `core_send_message` size-checks the durable-queue payload,
//!    creates the task as `WORKING`, registers any inline push
//!    config, enqueues a
//!    [`turul_a2a::durable_executor::QueuedExecutorJob`] on the SQS
//!    queue, and returns `200 OK` with the task snapshot.
//! 2. The executor is **not** spawned locally — post-return
//!    `tokio::spawn` is not guaranteed to complete on Lambda, so the
//!    framework defers execution to the SQS-triggered Lambda.
//!
//! The SQS side is `examples/lambda-durable-worker`: a separate
//! Lambda function wired via an SQS event source mapping. Both
//! Lambdas share the same storage backend (DynamoDB in production;
//! see `examples/lambda-infra/cloudformation.yaml`).
//!
//! Without the SQS wiring, the Lambda adapter rejects
//! `returnImmediately=true` with `UnsupportedOperationError`. This
//! example flips the capability back on by calling
//! `.with_sqs_return_immediately(queue_url, sqs_client)` —
//! supplying the queue is the capability; the flag cannot be claimed
//! without it.
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
//! this Lambda wrote, runs the executor, and commits the terminal
//! state via the same CAS-guarded atomic store. Deploy the DynamoDB
//! tables from `examples/lambda-infra/cloudformation.yaml` before
//! running.

use std::sync::Arc;

use async_trait::async_trait;
use turul_a2a::card_builder::{AgentCardBuilder, AgentSkillBuilder};
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::storage::dynamodb::{DynamoDbA2aStorage, DynamoDbConfig};
use turul_a2a_aws_lambda::LambdaA2aServerBuilder;
use turul_a2a_types::{Message, Task};

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
                "Two-Lambda durable executor demo. HTTP invocation enqueues \
                 a job; a separate SQS-triggered Lambda runs the executor. \
                 The executor echoes the incoming text, task id, context id, \
                 and message metadata keys so the payload survives the \
                 HTTP → SQS → dequeue hop intact.",
            )
            .url(self.public_url.as_str(), "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .streaming(false)
            .skill(
                AgentSkillBuilder::new(
                    "durable-echo",
                    "Durable Echo",
                    "Echoes the incoming text plus the framework-assigned task id, \
                     context id, and the *keys* of any client-supplied message \
                     metadata. Proves the payload survives the HTTP → SQS → \
                     dequeue → executor hop without alteration.",
                )
                .tags(vec!["echo", "durable", "sqs", "diagnostics"])
                .examples(vec![
                    "Send `{\"message\":{...,\"parts\":[{\"text\":\"probe-42\"}],\
                     \"metadata\":{\"trigger_id\":\"trig-x\",\"attempt\":1}},\
                     \"configuration\":{\"returnImmediately\":true}}`. The \
                     terminal artifact includes the probe text, both ids, and \
                     `metadata_keys: [attempt, trigger_id]` (keys only — values \
                     are not echoed)."
                ])
                .build(),
            )
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
        lambda_http::Error::from("A2A_EXECUTOR_QUEUE_URL is required — set it to the SQS queue URL")
    })?;
    // Agent card advertises this URL in /.well-known/agent-card.json.
    // Discovery clients use it — a wrong value breaks discovery.
    let public_url = match std::env::var("A2A_PUBLIC_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            tracing::warn!(
                "A2A_PUBLIC_URL not set; agent card will advertise a placeholder URL. \
                 Set this env var to the Function URL / API Gateway endpoint for this Lambda."
            );
            "https://lambda.invalid".to_string()
        }
    };

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
    // enqueued. Deploy the tables from
    // `examples/lambda-infra/cloudformation.yaml` before running.
    //
    // `with_push_dispatch_enabled(true)` omitted — this demo does not
    // register push configs, so the builder correctly rejects the
    // inconsistent pairing with no `push_delivery_store`. Production
    // deployments that want push delivery wire
    // `.push_delivery_store(storage.clone())` and flip the flag in
    // tandem.
    //
    // `.with_sqs_return_immediately(queue_url, sqs)` re-enables
    // `supports_return_immediately` on the `RuntimeConfig` as a side
    // effect of supplying the queue.
    LambdaA2aServerBuilder::new()
        .executor(DurableEchoExecutor { public_url })
        .storage(dynamodb_storage)
        .with_sqs_return_immediately(queue_url, sqs)
        .build()
        .map_err(|e| lambda_http::Error::from(format!("builder error: {e}")))?
        .run_http_only()
        .await
}
