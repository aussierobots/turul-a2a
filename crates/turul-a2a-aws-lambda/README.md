# turul-a2a-aws-lambda

AWS Lambda adapter for the `turul-a2a` server framework — a thin wrapper
over the same `axum::Router` used in binary deployments.

- Request/response transport (HTTP + JSON-RPC).
- Streaming (SSE) verified end-to-end via `cargo-lambda`.
- API Gateway / Lambda Function URL authorizer anti-spoofing for trusted
  identity injection.
- Same-backend enforcement with shared storage (DynamoDB or PostgreSQL)
  so task state and durable events coordinate across cold starts and
  concurrent invocations.

The adapter is request/response-shaped: a single Lambda invocation
processes one HTTP request or one SQS batch and returns. Streaming
endpoints are buffered (the entire executor run executes within the
invocation); persistent SSE / `:subscribe` connections are not
supported on Lambda by design.

Enable the `dynamodb` feature to propagate DynamoDB support to the
underlying `turul-a2a` crate:

```toml
turul-a2a-aws-lambda = { version = "0.1", features = ["dynamodb"] }
```

## Quick start

```rust
use turul_a2a_aws_lambda::LambdaA2aServerBuilder;

let handler = LambdaA2aServerBuilder::new()
    .executor(MyAgent)
    .storage(my_dynamodb_storage)
    .build()?;

handler.run().await?;
```

`run()` is the universal entry point — it auto-detects whether the
incoming Lambda event is an HTTP request or an SQS batch and dispatches
accordingly. For pure single-event-type Lambdas the runner methods
`run_http_only()`, `run_sqs_only()`, and `run_http_and_sqs()` are
available for adopters who prefer to be explicit.

## Push-notification recovery

Push-notification delivery on Lambda requires **external triggers**.
`tokio::spawn` continuations created inside a Lambda invocation are
opportunistic only: the execution environment may be frozen
indefinitely between invocations, so the in-process
`PushDeliveryWorker` reclaim loop the binary server uses has no
equivalent. Correctness is carried by an atomic pending-dispatch
marker written inside the request Lambda's commit transaction, plus
two external workers:

| Worker | Trigger | Role |
|---|---|---|
| `LambdaStreamRecoveryHandler` | DynamoDB Stream on `a2a_push_pending_dispatches` (NEW_IMAGE) | Fast path — fires within ~1s of the marker commit. DynamoDB backends only. |
| `LambdaScheduledRecoveryHandler` | EventBridge Scheduler (e.g. every 5 min) | Mandatory backstop for all backends. For SQLite/Postgres/in-memory this is the **only** recovery path. |

Minimal wiring — see `examples/lambda-stream-worker` and
`examples/lambda-scheduled-worker`:

```rust
// Stream worker (DynamoDB only)
use turul_a2a_aws_lambda::LambdaStreamRecoveryHandler;
let handler = LambdaStreamRecoveryHandler::new(dispatcher);
// Lambda input type: aws_lambda_events::dynamodb::Event
// Response type: aws_lambda_events::streams::DynamoDbEventResponse

// Scheduled worker (all backends)
use turul_a2a_aws_lambda::{LambdaScheduledRecoveryHandler, LambdaScheduledRecoveryConfig};
let handler = LambdaScheduledRecoveryHandler::new(dispatcher, delivery_store)
    .with_config(LambdaScheduledRecoveryConfig::default());
// Lambda input type: aws_lambda_events::eventbridge::EventBridgeEvent
// Response: LambdaScheduledRecoveryResponse (per-stage counts + error sample)
```

Release-gate invariants (pinned by tests in `src/stream_recovery_tests.rs` and `src/scheduled_recovery_tests.rs`):

- Valid INSERT + successful redispatch → no `BatchItemFailure`, one POST per config.
- `get_task → Ok(None)` (task deleted) → delete marker, return success.
- Transient storage error → `BatchItemFailure` with the record's `SequenceNumber`.
- Unparseable NEW_IMAGE → `BatchItemFailure` (logged).
- Duplicate stream records → exactly one POST (claim fencing).
- Scheduled sweep: stale markers recovered, transient errors counted and markers retained, reclaimable claims redispatched, batch limits honoured.

Operator responsibilities (not framework-managed):

1. Provision the DynamoDB Stream (NEW_IMAGE view) on `a2a_push_pending_dispatches` — DynamoDB backends only.
2. Create an event-source mapping with `FunctionResponseTypes: ["ReportBatchItemFailures"]`, a DLQ, and `MaximumRetryAttempts`.
3. Create an EventBridge Scheduler schedule (recommended every 5 min) targeting the scheduled worker.
4. Grant IAM: stream-worker reads the stream, scheduled-worker scans + deletes markers, both read tasks and configs.
5. Set TTL on `a2a_push_pending_dispatches` to outlast the worst-case scheduler outage.

## See also

- [Workspace README](../../README.md) — project overview, crate map, server/client basics.
- `examples/lambda-agent`, `examples/lambda-durable-{agent,worker,single}` — runnable Lambda topologies.
- `examples/lambda-stream-worker`, `examples/lambda-scheduled-worker` — push-recovery workers.

## License

Licensed under either [MIT](../../LICENSE-MIT) or
[Apache 2.0](../../LICENSE-APACHE) at your option.
