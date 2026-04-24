//! Durable executor continuation (ADR-018).
//!
//! Adopters on runtimes that cannot guarantee post-return
//! `tokio::spawn` continuation (AWS Lambda in particular) can opt in to
//! a durable executor dispatch path by wiring a [`DurableExecutorQueue`]
//! implementation. When wired, [`crate::router::core_send_message`] on
//! the `return_immediately = true` path enqueues a
//! [`QueuedExecutorJob`] envelope on the queue instead of spawning the
//! executor locally; a separate invocation consumes the queue and runs
//! the executor to terminal.
//!
//! The trait is the integration point. First-party impls ship in
//! `turul-a2a-aws-lambda` behind the `sqs` feature
//! (`SqsDurableExecutorQueue`); adopters may provide alternative
//! backends (Kinesis, SNS+queue, Step Functions task token, etc.) by
//! implementing this trait directly.
//!
//! See also: ADR-017 (`RuntimeConfig::supports_return_immediately`
//! capability gate) and ADR-013 (Lambda push-delivery parity).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Versioned SQS / durable-queue payload for an executor dispatch.
///
/// Serialised to the queue at enqueue time and deserialised on
/// consume. Carries enough context for the consumer to run the
/// executor without re-validating any of the HTTP-side auth (which
/// already succeeded to reach enqueue) — the `owner` and `claims`
/// fields encode the authenticated identity.
///
/// Schema evolution: `version` defaults to the current shape's
/// number. Consumers reject unknown versions at dequeue, which surfaces
/// as a batch-item failure so the record can be retried or DLQ'd once
/// the consumer's code catches up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedExecutorJob {
    /// Envelope version. Current: `1`. Increment on any schema change
    /// that cannot be handled by adding optional fields.
    pub version: u16,
    pub tenant: String,
    pub owner: String,
    pub task_id: String,
    pub context_id: String,
    /// The incoming `SendMessage.message` from the HTTP request,
    /// preserved verbatim. Forwarded to the executor's
    /// [`crate::executor::ExecutionContext`] by the consumer.
    pub message: turul_a2a_proto::Message,
    /// JWT claims from the HTTP invocation, if any. Ride with the
    /// envelope — the consumer does NOT re-validate the JWT. Mirrors
    /// `ExecutionContext.claims`.
    pub claims: Option<serde_json::Value>,
    /// Epoch microseconds at which the HTTP invocation enqueued the
    /// job. Used by consumers for lag / DLQ diagnostics.
    pub enqueued_at_micros: i64,
}

impl QueuedExecutorJob {
    /// Current envelope version.
    pub const VERSION: u16 = 1;
}

/// Errors returned by [`DurableExecutorQueue`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// Failed to serialise the job before enqueue. Unlikely in
    /// practice — all fields are plain `serde`-friendly types.
    #[error("failed to encode queue payload: {0}")]
    Encode(#[from] serde_json::Error),

    /// The encoded payload exceeds the transport's hard limit
    /// (`max_payload_bytes`). Callers handle this as a synchronous
    /// rejection (HTTP 400 `InvalidRequest`) before any task is
    /// created.
    #[error("queue payload too large: {actual} bytes exceeds limit of {max} bytes")]
    PayloadTooLarge { actual: usize, max: usize },

    /// Transport-level failure (SQS API error, network, IAM,
    /// throttling). Caller typically responds with FAILED-compensation
    /// on the already-created task (ADR-018 §Decision HTTP-enqueue
    /// step 7).
    #[error("queue transport error: {0}")]
    Transport(String),
}

/// Durable executor queue — the integration point for Pattern B
/// (framework-managed durable continuation) per ADR-018.
///
/// **Public extension point, semver-sensitive.** External
/// implementers should treat method additions (and signature changes)
/// as semver-minor breaking changes for their crates. First-party
/// impls ship behind `turul-a2a-aws-lambda`'s `sqs` feature
/// (`SqsDurableExecutorQueue`). Adopters may provide alternative
/// backends by implementing this trait directly, but must be ready to
/// re-implement against new methods when the trait evolves.
#[async_trait]
pub trait DurableExecutorQueue: Send + Sync {
    /// Hard payload ceiling for this transport, in bytes.
    /// Implementations MUST return the transport's real limit, not an
    /// idealised one:
    ///
    /// - SQS standard queue: `256 * 1024` (256 KiB).
    /// - Kinesis data stream: `1 * 1024 * 1024` (1 MiB).
    ///
    /// Consulted by [`Self::check_payload_size`] via the default
    /// implementation.
    fn max_payload_bytes(&self) -> usize;

    /// Pre-enqueue size check. Default implementation JSON-encodes the
    /// job and compares the encoded length against
    /// [`Self::max_payload_bytes`]. Implementations MAY override to
    /// use whatever encoding their [`Self::enqueue`] would produce
    /// (avoids double-serialisation on implementations where the
    /// native encoding differs from `serde_json`).
    ///
    /// Called by `core_send_message` BEFORE task creation so oversize
    /// payloads never persist a task row (ADR-018 §Decision HTTP-enqueue
    /// step 4).
    fn check_payload_size(&self, job: &QueuedExecutorJob) -> Result<usize, QueueError> {
        let encoded = serde_json::to_vec(job)?;
        let max = self.max_payload_bytes();
        if encoded.len() > max {
            Err(QueueError::PayloadTooLarge {
                actual: encoded.len(),
                max,
            })
        } else {
            Ok(encoded.len())
        }
    }

    /// Enqueue a job for asynchronous executor dispatch.
    /// Implementations SHOULD call [`Self::check_payload_size`]
    /// internally as a defence-in-depth guard; `core_send_message`
    /// also calls it upstream so `enqueue` failures for oversize
    /// payloads are unexpected at this point.
    async fn enqueue(&self, job: QueuedExecutorJob) -> Result<(), QueueError>;

    /// Identifier for logs / errors / diagnostics (`"sqs"`,
    /// `"kinesis"`, `"fake"`, etc.).
    fn kind(&self) -> &'static str;
}
