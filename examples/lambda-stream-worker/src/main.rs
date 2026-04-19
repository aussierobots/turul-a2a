//! Lambda stream-recovery worker example (ADR-013 §5.2).
//!
//! This binary is the DynamoDB Stream trigger for the
//! `a2a_push_pending_dispatches` table. It receives NEW_IMAGE records
//! committed by the request Lambda's
//! `A2aAtomicStore::update_task_status_with_events` and drives them
//! through `PushDispatcher::try_redispatch_pending`, mapping outcomes
//! to `BatchItemFailures` per the Lambda partial-batch response
//! contract.
//!
//! ## Why two Lambdas (stream + scheduled)?
//!
//! ADR-013 §4.4: `tokio::spawn` continuations created inside a Lambda
//! invocation are **opportunistic**. Between invocations the execution
//! environment may be frozen indefinitely; correctness cannot depend
//! on post-return completion. External triggers are mandatory:
//!
//! - **DynamoDB Stream (this worker)** — fast path: every atomic
//!   marker commit in the request Lambda produces an INSERT record
//!   that fans out to registered push webhooks with sub-second latency.
//! - **EventBridge Scheduler** — mandatory backstop for cases the
//!   stream cannot handle (poison records, stream outages). For
//!   non-DynamoDB backends the scheduler is the only recovery path.
//!
//! See `examples/lambda-scheduled-worker` for the backstop
//! implementation.
//!
//! ## Deploy
//!
//! 1. Package this binary as a Lambda function (e.g. `cargo lambda
//!    build --release`).
//! 2. Create a DynamoDB Stream on the `a2a_push_pending_dispatches`
//!    table with `StreamViewType::NEW_IMAGE`.
//! 3. Create an event source mapping from the stream to this Lambda
//!    with:
//!    - `FunctionResponseTypes: ["ReportBatchItemFailures"]` (so the
//!       response's `batch_item_failures` field is honoured)
//!    - `MaximumRetryAttempts` and a DLQ configured per your ops
//!       posture
//! 4. Grant the execution role `Put/Delete` on the deliveries and
//!    pending-dispatch tables plus `Get`/`Query` on the tasks table.

use std::sync::Arc;

use aws_lambda_events::event::dynamodb::Event as DynamoDbEvent;
use aws_lambda_events::event::streams::DynamoDbEventResponse;
use lambda_runtime::{service_fn, Error, LambdaEvent};
use turul_a2a::push::delivery::{PushDeliveryConfig, PushDeliveryWorker};
use turul_a2a::push::{A2aPushDeliveryStore, PushDispatcher};
use turul_a2a::storage::{
    A2aPushNotificationStorage, A2aTaskStorage, InMemoryA2aStorage,
};
use turul_a2a_aws_lambda::LambdaStreamRecoveryHandler;

/// In a real deployment, replace this with
/// `DynamoDbA2aStorage::new(config).await?` or whichever backend the
/// request Lambda uses. The same-backend requirement (ADR-009 + ADR-013)
/// applies: the stream worker MUST observe the same rows the request
/// Lambda committed.
///
/// This example uses the in-memory backend only so `cargo check` and
/// local-invoke smoke testing work without AWS credentials. An
/// in-memory stream worker has no useful effect because the markers
/// live in a different process — deploy with a shared backend.
fn build_storage() -> Arc<InMemoryA2aStorage> {
    Arc::new(InMemoryA2aStorage::new().with_push_dispatch_enabled(true))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .without_time()
        .init();

    // One-time cold-start setup: build the dispatcher against the
    // shared backend. The dispatcher is Clone so it can be shared
    // between invocations.
    let storage = build_storage();
    let delivery: Arc<dyn A2aPushDeliveryStore> = storage.clone();
    let push_storage: Arc<dyn A2aPushNotificationStorage> = storage.clone();
    let task_storage: Arc<dyn A2aTaskStorage> = storage.clone();

    let mut delivery_cfg = PushDeliveryConfig::default();
    delivery_cfg.allow_insecure_urls =
        std::env::var("A2A_ALLOW_INSECURE_PUSH").is_ok_and(|v| v == "1");
    let worker = Arc::new(
        PushDeliveryWorker::new(
            delivery,
            delivery_cfg,
            None,
            format!("stream-worker-{}", uuid_v7()),
        )
        .map_err(|e| Error::from(format!("worker build failed: {e}")))?,
    );
    let dispatcher = Arc::new(PushDispatcher::new(worker, push_storage, task_storage));
    let handler = LambdaStreamRecoveryHandler::new(dispatcher);

    lambda_runtime::run(service_fn(move |event: LambdaEvent<DynamoDbEvent>| {
        let handler = handler.clone();
        async move {
            let response: DynamoDbEventResponse =
                handler.handle_stream_event(event.payload).await;
            Ok::<_, Error>(response)
        }
    }))
    .await
}

/// Local uuid helper — avoids taking a direct dep on `uuid` in this
/// example when the transitive one would work, but also demonstrates
/// that the instance id is an operator concern.
fn uuid_v7() -> String {
    // `uuid::Uuid::now_v7` is re-exported via turul-a2a-aws-lambda's
    // dep tree in practice; in the example we just use tokio time +
    // pid to avoid a new dep.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{now:x}-{}", std::process::id())
}
