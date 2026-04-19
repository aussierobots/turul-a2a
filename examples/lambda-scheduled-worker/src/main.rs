//! Lambda scheduled-recovery worker example (ADR-013 §5.3).
//!
//! This binary is the EventBridge Scheduler trigger for the
//! mandatory push-recovery backstop. One invocation equals one sweep
//! tick:
//!
//! 1. Read up to `stale_markers_limit` stale pending-dispatch markers
//!    via `list_stale_pending_dispatches` and redispatch each.
//! 2. Read up to `reclaimable_claims_limit` expired-non-terminal claim
//!    rows via `list_reclaimable_claims` and redispatch each.
//!
//! The response is a summary (see
//! `LambdaScheduledRecoveryResponse`) with per-stage counts. Wire it
//! to CloudWatch metrics for alerts.
//!
//! ## Why this Lambda is mandatory
//!
//! `tokio::spawn` continuations created inside the request Lambda are
//! opportunistic (ADR-013 §4.4). For non-DynamoDB backends (SQLite,
//! Postgres, in-memory) there is no event stream — the scheduled
//! worker IS the recovery path. For DynamoDB it runs alongside the
//! stream worker as belt-and-braces: the stream has 24h retention and
//! can be stalled by poison records; the scheduler picks up anything
//! the stream missed.
//!
//! ## Deploy
//!
//! 1. Package this binary as a Lambda function.
//! 2. Create an EventBridge Scheduler schedule (recommended: every
//!    5 minutes) that targets this Lambda. The input payload is
//!    ignored — each invocation is one sweep regardless of metadata.
//! 3. Grant the execution role `Scan/Query/Delete` on the pending
//!    dispatches + deliveries tables and `Get` on the tasks table.
//! 4. Tune `LambdaScheduledRecoveryConfig` to match your workload:
//!    - `stale_cutoff` should exceed the request Lambda's worst-case
//!      fresh-dispatch latency (recommended: equal to
//!      `push_claim_expiry`).
//!    - `stale_markers_limit` / `reclaimable_claims_limit` cap the
//!      batch size so a post-outage backlog recovers over several
//!      ticks rather than overwhelming the Lambda.

use std::sync::Arc;

use aws_lambda_events::event::eventbridge::EventBridgeEvent;
use lambda_runtime::{service_fn, Error, LambdaEvent};
use turul_a2a::push::delivery::{PushDeliveryConfig, PushDeliveryWorker};
use turul_a2a::push::{A2aPushDeliveryStore, PushDispatcher};
use turul_a2a::storage::{
    A2aPushNotificationStorage, A2aTaskStorage, InMemoryA2aStorage,
};
use turul_a2a_aws_lambda::{
    LambdaScheduledRecoveryConfig, LambdaScheduledRecoveryHandler,
    LambdaScheduledRecoveryResponse,
};

/// Replace this with the shared backend the request Lambda uses
/// (e.g. `DynamoDbA2aStorage::new(config).await?`). Same-backend
/// applies (ADR-009): the scheduled worker MUST observe the same
/// rows the request Lambda committed.
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

    let storage = build_storage();
    let delivery: Arc<dyn A2aPushDeliveryStore> = storage.clone();
    let push_storage: Arc<dyn A2aPushNotificationStorage> = storage.clone();
    let task_storage: Arc<dyn A2aTaskStorage> = storage.clone();

    let delivery_cfg = PushDeliveryConfig::default();
    let worker = Arc::new(
        PushDeliveryWorker::new(
            delivery.clone(),
            delivery_cfg,
            None,
            format!("scheduled-worker-{}", instance_tag()),
        )
        .map_err(|e| Error::from(format!("worker build failed: {e}")))?,
    );
    let dispatcher = Arc::new(PushDispatcher::new(worker, push_storage, task_storage));

    let handler = LambdaScheduledRecoveryHandler::new(dispatcher, delivery)
        .with_config(LambdaScheduledRecoveryConfig {
            stale_cutoff: std::time::Duration::from_secs(10 * 60),
            stale_markers_limit: 128,
            reclaimable_claims_limit: 128,
        });

    lambda_runtime::run(service_fn(
        move |event: LambdaEvent<EventBridgeEvent>| {
            let handler = handler.clone();
            async move {
                let response: LambdaScheduledRecoveryResponse =
                    handler.handle_scheduled_event(event.payload).await;
                tracing::info!(
                    target: "turul_a2a::scheduled_worker_tick",
                    stale_markers_found = response.stale_markers_found,
                    stale_markers_recovered = response.stale_markers_recovered,
                    stale_markers_transient_errors = response.stale_markers_transient_errors,
                    reclaimable_claims_found = response.reclaimable_claims_found,
                    reclaimable_claims_processed = response.reclaimable_claims_processed,
                    error_sample = ?response.errors,
                    "scheduled recovery tick complete"
                );
                Ok::<_, Error>(response)
            }
        },
    ))
    .await
}

fn instance_tag() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{now:x}-{}", std::process::id())
}
