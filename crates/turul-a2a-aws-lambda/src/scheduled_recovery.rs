//! EventBridge Scheduler-triggered push recovery handler
//! (ADR-013 §4.2 / §5.3).
//!
//! The scheduled worker is the **mandatory** backstop for Lambda push
//! recovery across all four backends. It exists because:
//!
//! - DynamoDB Streams (the [`crate::LambdaStreamRecoveryHandler`]
//!   trigger) have 24h retention and can be stalled by poison records.
//! - Non-DynamoDB backends have no equivalent stream. For SQLite /
//!   PostgreSQL / in-memory deployments this scheduler IS the
//!   recovery path.
//! - `tokio::spawn`ed dispatch continuations created inside a Lambda
//!   invocation are opportunistic at best: the
//!   execution environment MAY be frozen between invocations.
//!   Correctness cannot depend on post-return completion.
//!
//! On every tick the handler:
//!
//! 1. Enumerates up to `stale_markers_limit` pending-dispatch markers
//!    whose `recorded_at` is older than `now() - stale_cutoff` via
//!    [`A2aPushDeliveryStore::list_stale_pending_dispatches`] and
//!    drives each through [`PushDispatcher::try_redispatch_pending`].
//! 2. Enumerates up to `reclaimable_claims_limit` expired-non-terminal
//!    claim rows via
//!    [`A2aPushDeliveryStore::list_reclaimable_claims`] and drives
//!    each through [`PushDispatcher::redispatch_one`].
//! 3. Returns a [`LambdaScheduledRecoveryResponse`] summary with
//!    per-stage counts and a capped list of error strings for
//!    telemetry.
//!
//! No in-process background loop is assumed — a single `.invoke()`
//! equals one sweep tick. Operators wire an EventBridge Scheduler rule
//! (default recommendation: every 5 minutes).

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aws_lambda_events::event::eventbridge::EventBridgeEvent;
use serde::Serialize;
use turul_a2a::push::{A2aPushDeliveryStore, PushDispatcher};

/// Summary returned from a single scheduled-recovery tick.
///
/// Emitted as the Lambda response so operators can wire CloudWatch
/// metrics off it (e.g. alert when `stale_markers_transient_errors`
/// exceeds a threshold). Field shape is camelCase to match AWS Lambda
/// response conventions.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LambdaScheduledRecoveryResponse {
    /// Total stale markers read in this tick (pre-redispatch).
    pub stale_markers_found: usize,
    /// Markers whose redispatch completed (fan-out ran or task
    /// deleted).
    pub stale_markers_recovered: usize,
    /// Markers whose redispatch hit a transient storage error. The
    /// markers are retained for the next tick.
    pub stale_markers_transient_errors: usize,
    /// Reclaimable claims read in this tick.
    pub reclaimable_claims_found: usize,
    /// Claims redispatched (fire-and-forget — error signal isn't
    /// plumbed back through `redispatch_one`).
    pub reclaimable_claims_processed: usize,
    /// First few error strings surfaced during the sweep. Capped at
    /// [`LambdaScheduledRecoveryHandler::ERROR_SAMPLE_LIMIT`] to keep
    /// the response body bounded.
    pub errors: Vec<String>,
}

/// Configuration for a scheduled-recovery sweep.
#[derive(Debug, Clone)]
pub struct LambdaScheduledRecoveryConfig {
    /// A marker is "stale" when `now() - marker.recorded_at >= stale_cutoff`.
    /// Must be larger than the worst-case fresh-dispatch latency so
    /// in-flight dispatches are not preempted. Recommended default
    /// is the runtime `push_claim_expiry` value (10 min default).
    pub stale_cutoff: Duration,
    /// Max markers processed per tick.
    pub stale_markers_limit: usize,
    /// Max reclaimable claims processed per tick.
    pub reclaimable_claims_limit: usize,
}

impl Default for LambdaScheduledRecoveryConfig {
    fn default() -> Self {
        Self {
            stale_cutoff: Duration::from_secs(10 * 60),
            stale_markers_limit: 128,
            reclaimable_claims_limit: 128,
        }
    }
}

/// Handler for scheduled push-recovery ticks.
///
/// Construct once per Lambda cold start; invoke per scheduled event.
#[derive(Clone)]
pub struct LambdaScheduledRecoveryHandler {
    dispatcher: Arc<PushDispatcher>,
    delivery_store: Arc<dyn A2aPushDeliveryStore>,
    config: LambdaScheduledRecoveryConfig,
}

impl LambdaScheduledRecoveryHandler {
    /// Cap on `errors` in a single response so Lambda doesn't return
    /// megabytes of stringified errors under a persistent outage.
    pub const ERROR_SAMPLE_LIMIT: usize = 8;

    pub fn new(
        dispatcher: Arc<PushDispatcher>,
        delivery_store: Arc<dyn A2aPushDeliveryStore>,
    ) -> Self {
        Self {
            dispatcher,
            delivery_store,
            config: LambdaScheduledRecoveryConfig::default(),
        }
    }

    pub fn with_config(mut self, config: LambdaScheduledRecoveryConfig) -> Self {
        self.config = config;
        self
    }

    /// Run one recovery sweep. The `_event` argument is accepted so
    /// the signature matches `lambda_runtime::service_fn`; the
    /// EventBridge payload is not inspected — each invocation is one
    /// tick regardless of cron metadata.
    pub async fn handle_scheduled_event(
        &self,
        _event: EventBridgeEvent,
    ) -> LambdaScheduledRecoveryResponse {
        self.run_sweep().await
    }

    /// Testable entry point. Runs the full sweep and returns a
    /// summary. External callers almost always want
    /// [`Self::handle_scheduled_event`]; this method is exposed so
    /// unit tests can exercise the recovery logic without
    /// constructing an `EventBridgeEvent`.
    pub async fn run_sweep(&self) -> LambdaScheduledRecoveryResponse {
        let mut response = LambdaScheduledRecoveryResponse::default();

        // Stage 1: stale pending-dispatch markers. The cutoff is
        // `now() - stale_cutoff`; markers older than this are
        // eligible for redispatch.
        let cutoff = SystemTime::now()
            .checked_sub(self.config.stale_cutoff)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        match self
            .delivery_store
            .list_stale_pending_dispatches(cutoff, self.config.stale_markers_limit)
            .await
        {
            Ok(markers) => {
                response.stale_markers_found = markers.len();
                for marker in markers {
                    match self.dispatcher.try_redispatch_pending(marker).await {
                        Ok(()) => response.stale_markers_recovered += 1,
                        Err(e) => {
                            response.stale_markers_transient_errors += 1;
                            self.sample_error(&mut response.errors, format!("marker: {e}"));
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    target: "turul_a2a::lambda_scheduled_recovery_list_markers_failed",
                    error = %e,
                    "list_stale_pending_dispatches failed; stage skipped this tick"
                );
                self.sample_error(
                    &mut response.errors,
                    format!("list_stale_pending_dispatches: {e}"),
                );
            }
        }

        // Stage 2: reclaimable claims — non-terminal claim rows whose
        // expiry has passed. `redispatch_one` is fire-and-forget on
        // the dispatcher side; we count every enumerated row as
        // "processed" and rely on claim fencing for correctness.
        match self
            .delivery_store
            .list_reclaimable_claims(self.config.reclaimable_claims_limit)
            .await
        {
            Ok(claims) => {
                response.reclaimable_claims_found = claims.len();
                for claim in claims {
                    self.dispatcher.redispatch_one(claim).await;
                    response.reclaimable_claims_processed += 1;
                }
            }
            Err(e) => {
                tracing::error!(
                    target: "turul_a2a::lambda_scheduled_recovery_list_claims_failed",
                    error = %e,
                    "list_reclaimable_claims failed; stage skipped this tick"
                );
                self.sample_error(
                    &mut response.errors,
                    format!("list_reclaimable_claims: {e}"),
                );
            }
        }

        response
    }

    fn sample_error(&self, errors: &mut Vec<String>, msg: String) {
        if errors.len() < Self::ERROR_SAMPLE_LIMIT {
            errors.push(msg);
        }
    }
}
