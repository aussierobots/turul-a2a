# lambda-stream-worker

The **fast-path push-recovery worker** in an ADR-013 Lambda deployment.
Triggered by a DynamoDB Stream on the `a2a_push_pending_dispatches` table —
every atomic marker the request Lambda commits becomes an INSERT record that
fans out to registered push webhooks with sub-second latency.

Part of a three-Lambda deployment:

1. `lambda-agent` — request Lambda, writes pending-dispatch markers.
2. `lambda-stream-worker` *(this crate)* — fast-path recovery.
3. `lambda-scheduled-worker` — mandatory backstop.

## Why this Lambda exists

`tokio::spawn` continuations inside the request Lambda are **opportunistic** —
between invocations the execution environment can be frozen, so post-return
work may never run (ADR-013 §4.4). Push fan-out therefore has to be driven by
an external trigger. The DynamoDB Stream is the low-latency path; the
scheduled worker is the backstop for poison records and stream outages.

## Build

```bash
cargo lambda build --release -p lambda-stream-worker
# Output: target/lambda/lambda-stream-worker/bootstrap.zip
```

## Deploy

1. Create a DynamoDB Stream on the `a2a_push_pending_dispatches` table with
   `StreamViewType: NEW_IMAGE`.
2. Create an event source mapping from the stream to this Lambda with:
   - `FunctionResponseTypes: ["ReportBatchItemFailures"]` — so the handler's
     `batch_item_failures` response controls retries.
   - `MaximumRetryAttempts` + a DLQ per your ops posture.
3. Grant the execution role:
   - `Put` / `Delete` on the deliveries and pending-dispatch tables.
   - `Get` / `Query` on the tasks table.

## Partial-batch response contract

`LambdaStreamRecoveryHandler::handle_stream_event` returns a
`DynamoDbEventResponse` whose `batch_item_failures` field lists the stream
records that should be retried. This is the standard Lambda partial-batch
pattern — a single poison record does not block the whole batch.

## Storage

The example wires `InMemoryA2aStorage` for `cargo check` convenience only —
an in-memory stream worker has no useful effect because the markers live in a
different process. Production deployments must point this Lambda at the same
DynamoDB (or other) backend the request Lambda uses (ADR-009 same-backend
requirement).

## See also

- ADR-011 (push delivery), ADR-013 (Lambda push-delivery parity).
- `examples/lambda-scheduled-worker` — the backstop that catches what this
  worker misses (and the only recovery path for non-DynamoDB backends).
