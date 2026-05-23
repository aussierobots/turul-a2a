# lambda-durable-worker

The **SQS consumer worker** for the durable executor demo. Companion
to `examples/lambda-durable-agent` (the HTTP request Lambda). An SQS
event source mapping triggers this Lambda with records the request
Lambda enqueued via `with_sqs_return_immediately(...)`.

Pure consumer — never enqueues. `LambdaA2aHandler::run_sqs_only`
owns the `lambda_runtime::run` loop; any non-SQS event shape errors
loudly.

## Per-record dispatch contract

For each record in the SQS batch the worker:

1. Deserialises `turul_a2a::durable_executor::QueuedExecutorJob`.
   Unknown envelope `version` → batch-item failure (retry).
2. Loads the task. Not found → batch-item failure (DLQ).
3. If the task is already terminal → idempotent success no-op.
4. If the cancel marker is set → commits `Canceled` directly; the
   executor is **never invoked**. A concurrent terminal write is
   treated as success.
5. Otherwise → runs the executor to terminal. The Lambda invocation
   *is* the executor; no `tokio::spawn` continuation.
6. Failed records are returned in `SqsBatchResponse.batch_item_failures`
   so one poison record doesn't block the rest of the batch.

## Build

```bash
cargo lambda build --release -p lambda-durable-worker
# Output: target/lambda/lambda-durable-worker/bootstrap.zip
```

## Local smoke

End-to-end local smoke requires a real (or LocalStack) SQS queue and
DynamoDB tables — the bootstrap binary refuses to start without
`A2A_EXECUTOR_QUEUE_URL` and the table env vars. See
`examples/LOCAL_TESTING.md` for the hybrid and LocalStack paths.

The binary itself accepts a hand-authored SQS event payload via
`cargo lambda invoke --data-file <event.json>` once the env vars
point at reachable AWS resources. `LOCAL_TESTING.md` §3 carries the
exact payload shape.

## Deploy

The request side (`examples/lambda-durable-agent`) creates the SQS
queue; this Lambda subscribes via an event source mapping. See
`examples/lambda-durable-agent/README.md` for the full
five-resource provisioning walk-through (queue + DLQ + tables +
two Lambdas).

## Storage

`InMemoryA2aStorage` is wired only so `cargo check` succeeds without
AWS credentials. The worker is meaningful only when it shares the
DynamoDB backend (or other persistent store) with the request
Lambda — ADR-009's same-backend requirement.

## See also

- `examples/lambda-durable-agent` — the request Lambda this worker
  pairs with.
- `examples/lambda-durable-single` — single-Lambda variant
  (combines request + executor in one container with
  `ReservedConcurrency=1`).
- `examples/LOCAL_TESTING.md` — local-first test matrix including
  the `cargo lambda invoke --data-file` SQS-event recipe.
- `examples/lambda-infra` — reference IaC for the SQS + DynamoDB
  resources.
