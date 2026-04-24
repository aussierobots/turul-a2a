//! Lambda A2A **SQS consumer worker** for ADR-018 durable executor
//! continuation.
//!
//! This is the dequeue-side half of the ADR-018 Pattern B demo. The
//! companion is `examples/lambda-durable-agent` (the HTTP request
//! Lambda). An SQS event source mapping triggers this Lambda with
//! records the request Lambda enqueued.
//!
//! Pure consumer: the builder does *not* call
//! `with_sqs_return_immediately(...)` because this Lambda never
//! enqueues. It only reads tasks from the shared DynamoDB backend and
//! runs their executors. `LambdaA2aHandler::run_sqs_only` owns the
//! `lambda_runtime::run` loop — any non-SQS event shape errors
//! loudly.
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
//! ## Shared storage
//!
//! The worker and the request Lambda (`lambda-durable-agent`) run in
//! **different** containers, so both must point at the same backend
//! — the worker loads a task the request Lambda already wrote and
//! commits its terminal state through the same CAS-guarded atomic
//! store (ADR-009 same-backend requirement). This example uses
//! `DynamoDbA2aStorage` against the five tables provisioned by
//! `examples/lambda-infra/cloudformation.yaml`.

use async_trait::async_trait;
use lambda_runtime::Error;
use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::storage::dynamodb::{DynamoDbA2aStorage, DynamoDbConfig};
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

    // Worker is a pure SQS consumer — it reads the task the agent
    // enqueued, runs the executor, commits the terminal via the same
    // shared DynamoDB atomic store (ADR-009 same-backend requirement).
    // No `with_sqs_return_immediately(...)` — that wires the builder
    // to *enqueue*, which the worker never does.
    let dynamodb_storage = DynamoDbA2aStorage::new(DynamoDbConfig::default())
        .await
        .map_err(|e| Error::from(format!("dynamodb storage build failed: {e}")))?;

    LambdaA2aServerBuilder::new()
        .executor(DurableEchoExecutor)
        .storage(dynamodb_storage)
        .build()
        .map_err(|e| Error::from(format!("builder error: {e}")))?
        .run_sqs_only()
        .await
}
