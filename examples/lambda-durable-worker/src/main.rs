//! Lambda A2A **SQS consumer worker** for ADR-018 durable executor
//! continuation.
//!
//! This is the dequeue-side half of the ADR-018 Pattern B demo. The
//! companion is `examples/lambda-durable-agent` (the HTTP request
//! Lambda). An SQS event source mapping triggers this Lambda with
//! records the request Lambda enqueued via
//! `LambdaA2aServerBuilder::with_sqs_return_immediately(...)`.
//!
//! Per ADR-018 §SQS invocation, each record passes through:
//!
//! 1. Deserialise [`turul_a2a::durable_executor::QueuedExecutorJob`].
//!    Unknown envelope `version` → batch-item failure (retry).
//! 2. Load the task. Not found → batch-item failure (DLQ).
//! 3. Task already terminal → idempotent success no-op.
//! 4. Cancel marker set via ADR-012
//!    `supervisor_get_cancel_requested` → commit CANCELED directly;
//!    executor is NEVER invoked. `TerminalStateAlreadySet` from a
//!    racing writer is treated as success.
//! 5. Non-terminal + no cancel → run the executor to terminal via
//!    `run_queued_executor_job`. The Lambda invocation *is* the
//!    executor; no `tokio::spawn`.
//! 6. Partial-batch response: failed records are returned in
//!    `SqsBatchResponse.batch_item_failures` so one poison record
//!    does not block the rest.
//!
//! ## Deploy
//!
//! See `examples/lambda-durable-agent/README.md` for the unified
//! deploy walk-through. This worker is wired as the target of an SQS
//! event source mapping with
//! `FunctionResponseTypes: ["ReportBatchItemFailures"]`.
//!
//! ## Storage caveat
//!
//! Same as `lambda-durable-agent`: the in-memory backend shown here
//! is for `cargo check` convenience only. The request Lambda and
//! this worker run in different containers, so in-memory state does
//! not survive the handoff. Production MUST share a backend —
//! `DynamoDbA2aStorage` is the idiomatic choice.

use std::sync::Arc;

use async_trait::async_trait;
use aws_lambda_events::event::sqs::{SqsBatchResponse, SqsEvent};
use lambda_runtime::{Error, LambdaEvent, service_fn};
use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_aws_lambda::LambdaA2aServerBuilder;
use turul_a2a_types::{Message, Task};

/// MUST match the executor wired into the request Lambda so the two
/// Lambdas behave consistently. In a real deployment this would be
/// your actual business-logic executor; here we ship the same
/// echo-style executor as `lambda-durable-agent`.
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
        // Not exposed — the worker does not serve HTTP — but required
        // by the trait.
        AgentCardBuilder::new("Durable Echo Agent (worker)", "0.1.0")
            .description("ADR-018 SQS worker for lambda-durable-agent")
            .url("https://lambda.example.com", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .streaming(false)
            .build()
            .expect("durable agent card should be valid")
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

    // The worker needs the same wiring as the request Lambda so the
    // durable-queue path is available on `AppState`. `with_sqs_return_immediately`
    // is a no-op here (the worker never enqueues); it's wired only to
    // satisfy the same-binary deployment shape if you later consolidate.
    // `InMemoryA2aStorage` is demo-only — swap in `DynamoDbA2aStorage`
    // for production.
    let queue_url = std::env::var("A2A_EXECUTOR_QUEUE_URL").map_err(|_| {
        Error::from(
            "A2A_EXECUTOR_QUEUE_URL is required — set it to the SQS queue URL \
             (default name: turul-a2a-durable-executor-demo)",
        )
    })?;

    let aws = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let sqs = Arc::new(aws_sdk_sqs::Client::new(&aws));

    let handler = LambdaA2aServerBuilder::new()
        .executor(DurableEchoExecutor)
        .storage(InMemoryA2aStorage::new().with_push_dispatch_enabled(true))
        .with_sqs_return_immediately(queue_url, sqs)
        .build()
        .map_err(|e| Error::from(format!("builder error: {e}")))?;

    lambda_runtime::run(service_fn(move |event: LambdaEvent<SqsEvent>| {
        let handler = handler.clone();
        async move {
            let (sqs_event, _ctx) = event.into_parts();
            let resp: SqsBatchResponse = handler.handle_sqs(sqs_event).await;
            Ok::<_, Error>(resp)
        }
    }))
    .await
}
