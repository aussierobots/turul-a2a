//! DynamoDB Stream-triggered push recovery handler (ADR-013 §5.2 / §8).
//!
//! When a DynamoDB Stream is configured on the
//! `a2a_push_pending_dispatches` table with `StreamViewType::NEW_IMAGE`,
//! every committed marker row triggers a Lambda invocation. This
//! handler iterates the batch, parses each INSERT record into a
//! [`PendingDispatch`], and drives it through
//! [`PushDispatcher::try_redispatch_pending`]. Transient failures are
//! reported as `BatchItemFailures` so the Lambda service re-delivers
//! the affected records; successes (including deleted-task cases) are
//! acknowledged by returning an empty `item_identifier` slot for
//! that record.
//!
//! ## Record classification
//!
//! | Record shape                              | Response                    |
//! |-------------------------------------------|-----------------------------|
//! | `INSERT` with well-formed NEW_IMAGE + OK  | success (no BatchItemFailure) |
//! | `INSERT` with well-formed NEW_IMAGE, task was deleted | success (marker deleted by dispatcher) |
//! | `INSERT` with transient storage error     | BatchItemFailure (record's SequenceNumber) |
//! | `INSERT` with unparseable NEW_IMAGE       | BatchItemFailure (logged) |
//! | `MODIFY` or `REMOVE`                      | skipped silently — marker either refreshed or already consumed |
//! | Record with no SequenceNumber             | logged + skipped (cannot be retried) |
//!
//! Duplicate records are safe: [`PushDispatcher::try_redispatch_pending`]
//! is idempotent under claim fencing. Two invocations
//! targeting the same `(tenant, task_id, event_sequence)` resolve to
//! at most one terminal claim row and at most one POST per config.

use std::sync::Arc;

use aws_lambda_events::event::dynamodb::Event as DynamoDbEvent;
use aws_lambda_events::event::streams::{DynamoDbBatchItemFailure, DynamoDbEventResponse};
use turul_a2a::push::PushDispatcher;
use turul_a2a::push::claim::PendingDispatch;

/// Handler for DynamoDB Stream events on `a2a_push_pending_dispatches`.
///
/// Construct once per Lambda cold start; reuse across invocations. The
/// handler holds an `Arc<PushDispatcher>` — the same dispatcher the
/// request-Lambda builds when `push_delivery_store` is wired, so you
/// can share one state bundle across both entry points.
#[derive(Clone)]
pub struct LambdaStreamRecoveryHandler {
    dispatcher: Arc<PushDispatcher>,
}

impl LambdaStreamRecoveryHandler {
    pub fn new(dispatcher: Arc<PushDispatcher>) -> Self {
        Self { dispatcher }
    }

    /// Handle a batch of DynamoDB Stream records. Returns the
    /// `DynamoDbEventResponse` with `BatchItemFailures` populated for
    /// records whose redispatch hit a transient error or whose
    /// NEW_IMAGE could not be parsed. Records with a missing
    /// `SequenceNumber` are skipped with a log warning — they cannot
    /// be surfaced in `BatchItemFailures` (the field is required by
    /// the Lambda service to identify the record to retry).
    pub async fn handle_stream_event(&self, event: DynamoDbEvent) -> DynamoDbEventResponse {
        let mut failures: Vec<DynamoDbBatchItemFailure> = Vec::new();

        for record in event.records {
            // MODIFY / REMOVE are not dispatch triggers. MODIFY may
            // appear when the dispatcher calls record_pending_dispatch
            // to refresh recorded_at; REMOVE when the marker is
            // consumed. Either way, the next commit or scheduler tick
            // handles recovery if anything needs retry.
            if record.event_name != "INSERT" {
                continue;
            }

            let seq_number = record.change.sequence_number.clone();

            match parse_pending_from_new_image(&record.change.new_image) {
                Ok(pending) => {
                    match self.dispatcher.try_redispatch_pending(pending).await {
                        Ok(()) => {
                            // Success: marker consumed (fan-out
                            // completed or task was deleted).
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "turul_a2a::lambda_stream_recovery_transient",
                                error = %e,
                                sequence_number = ?seq_number,
                                "stream redispatch returned transient error; \
                                 surfacing BatchItemFailure"
                            );
                            push_failure(&mut failures, seq_number);
                        }
                    }
                }
                Err(parse_err) => {
                    tracing::error!(
                        target: "turul_a2a::lambda_stream_recovery_parse_error",
                        error = %parse_err,
                        sequence_number = ?seq_number,
                        "failed to parse pending-dispatch NEW_IMAGE; \
                         surfacing BatchItemFailure"
                    );
                    push_failure(&mut failures, seq_number);
                }
            }
        }

        let mut resp = DynamoDbEventResponse::default();
        resp.batch_item_failures = failures;
        resp
    }
}

fn push_failure(failures: &mut Vec<DynamoDbBatchItemFailure>, seq: Option<String>) {
    match seq {
        Some(identifier) => {
            let mut f = DynamoDbBatchItemFailure::default();
            f.item_identifier = Some(identifier);
            failures.push(f);
        }
        None => {
            // Without a SequenceNumber we cannot tell Lambda which
            // record to retry. Log loudly — this indicates a
            // malformed event; operators should confirm the stream
            // configuration.
            tracing::error!(
                target: "turul_a2a::lambda_stream_recovery_no_sequence_number",
                "stream record missing SequenceNumber; cannot surface as \
                 BatchItemFailure. Record will not be retried."
            );
        }
    }
}

/// Reconstruct a [`PendingDispatch`] from a DynamoDB Stream NEW_IMAGE.
///
/// Expects the attributes the backend writes via
/// `A2aAtomicStore::update_task_status_with_events` (ADR-013 §4.3 /
/// `dynamodb::DynamoDbA2aStorage` marker Put):
///
/// - `tenant` (S)
/// - `taskId` (S)
/// - `owner` (S)
/// - `eventSequence` (N)
/// - `recordedAtMicros` (N)
///
/// A missing or wrong-typed attribute surfaces as a parse error;
/// the caller turns that into a `BatchItemFailure` so operators can
/// investigate the malformed record out-of-band.
fn parse_pending_from_new_image(item: &serde_dynamo::Item) -> Result<PendingDispatch, ParseError> {
    let tenant = string_attr(item, "tenant")?;
    let task_id = string_attr(item, "taskId")?;
    let owner = string_attr(item, "owner")?;
    let event_sequence = number_attr::<u64>(item, "eventSequence")?;
    let recorded_at_micros = number_attr::<i64>(item, "recordedAtMicros")?;

    let recorded_at =
        std::time::UNIX_EPOCH + std::time::Duration::from_micros(recorded_at_micros.max(0) as u64);

    Ok(PendingDispatch::new(
        tenant,
        owner,
        task_id,
        event_sequence,
        recorded_at,
    ))
}

fn string_attr(item: &serde_dynamo::Item, key: &str) -> Result<String, ParseError> {
    let raw = item
        .get(key)
        .ok_or_else(|| ParseError::MissingAttribute(key.to_string()))?;
    match raw {
        serde_dynamo::AttributeValue::S(s) => Ok(s.clone()),
        _ => Err(ParseError::WrongType {
            key: key.to_string(),
            expected: "S",
        }),
    }
}

fn number_attr<T: std::str::FromStr>(
    item: &serde_dynamo::Item,
    key: &str,
) -> Result<T, ParseError> {
    let raw = item
        .get(key)
        .ok_or_else(|| ParseError::MissingAttribute(key.to_string()))?;
    let n = match raw {
        serde_dynamo::AttributeValue::N(s) => s,
        _ => {
            return Err(ParseError::WrongType {
                key: key.to_string(),
                expected: "N",
            });
        }
    };
    n.parse::<T>().map_err(|_| ParseError::NumberParse {
        key: key.to_string(),
        value: n.clone(),
    })
}

#[derive(Debug, thiserror::Error)]
enum ParseError {
    #[error("NEW_IMAGE missing required attribute {0}")]
    MissingAttribute(String),
    #[error("NEW_IMAGE attribute {key} has wrong type (expected {expected})")]
    WrongType { key: String, expected: &'static str },
    #[error("NEW_IMAGE attribute {key} = {value:?} is not parseable as a number")]
    NumberParse { key: String, value: String },
}
