# lambda-durable-agent + lambda-durable-worker

**ADR-018 end-to-end demo**: Lambda durable executor continuation via AWS SQS.

The one example pair in the workspace where `SendMessageConfiguration.returnImmediately = true` actually works on AWS Lambda. Without the SQS wiring shown here, the Lambda adapter rejects that flag with `UnsupportedOperationError` per ADR-017.

## Two Lambdas, one deployable workflow

ADR-018's dispatch pattern splits across two Lambda functions that share the same storage backend:

| Crate | Event source | Role |
|---|---|---|
| `lambda-durable-agent` | HTTP (Function URL / API Gateway / ALB) | Receives `/message:send` with `returnImmediately=true`, size-checks the envelope, creates the task as `WORKING`, enqueues a `QueuedExecutorJob` on SQS, returns 200. Executor is NOT run locally. |
| `lambda-durable-worker` | SQS event source mapping | Triggered by the queue. Per record: loads the task, idempotency-checks it (terminal → no-op), checks the ADR-012 cancel marker (set → CANCELED direct commit, executor never invoked), otherwise runs the executor via `run_queued_executor_job`. Returns `SqsBatchResponse` with partial-batch failure semantics. |

This split matches the existing pattern (`lambda-agent` + `lambda-stream-worker` + `lambda-scheduled-worker`). Both Lambdas share storage — the request Lambda writes the task, the worker reads it. **In-memory storage will not work across the handoff**; production deployments need DynamoDB (or another shared backend).

## Prerequisites

- AWS account + credentials in your environment (`aws sts get-caller-identity`).
- [`cargo-lambda`](https://www.cargo-lambda.info/) installed (`cargo install cargo-lambda`).
- A Lambda execution role you can attach (you can create one in the console or via CLI).

## Deploy

### 1. Create the SQS queue + DLQ

```bash
# DLQ first.
aws sqs create-queue \
  --queue-name turul-a2a-durable-executor-demo-dlq

DLQ_ARN=$(aws sqs get-queue-attributes \
  --queue-url "$(aws sqs get-queue-url --queue-name turul-a2a-durable-executor-demo-dlq --query QueueUrl --output text)" \
  --attribute-names QueueArn \
  --query 'Attributes.QueueArn' --output text)

# Main queue. VisibilityTimeout MUST exceed worst-case executor runtime.
# 60s is plenty for the demo echo executor; tune for real workloads.
aws sqs create-queue \
  --queue-name turul-a2a-durable-executor-demo \
  --attributes "{
    \"VisibilityTimeout\": \"60\",
    \"MessageRetentionPeriod\": \"345600\",
    \"KmsMasterKeyId\": \"alias/aws/sqs\",
    \"RedrivePolicy\": \"{\\\"deadLetterTargetArn\\\":\\\"$DLQ_ARN\\\",\\\"maxReceiveCount\\\":\\\"3\\\"}\"
  }"

QUEUE_URL=$(aws sqs get-queue-url \
  --queue-name turul-a2a-durable-executor-demo \
  --query QueueUrl --output text)
QUEUE_ARN=$(aws sqs get-queue-attributes \
  --queue-url "$QUEUE_URL" \
  --attribute-names QueueArn \
  --query 'Attributes.QueueArn' --output text)

echo "QUEUE_URL=$QUEUE_URL"
echo "QUEUE_ARN=$QUEUE_ARN"
```

`KmsMasterKeyId=alias/aws/sqs` enables SSE-SQS (AWS-managed key). Swap for a customer-managed KMS key ARN if the ADR-018 §Security "tenant-sensitive" recommendation applies.

### 2. Build both Lambda bundles

```bash
cargo lambda build --release -p lambda-durable-agent
cargo lambda build --release -p lambda-durable-worker
```

Outputs:
- `target/lambda/lambda-durable-agent/bootstrap.zip`
- `target/lambda/lambda-durable-worker/bootstrap.zip`

### 3. Create the **request** Lambda function (HTTP)

```bash
ROLE_ARN=<your Lambda execution role ARN>

aws lambda create-function \
  --function-name turul-a2a-durable-agent \
  --runtime provided.al2023 \
  --role "$ROLE_ARN" \
  --handler bootstrap \
  --architectures arm64 \
  --zip-file fileb://target/lambda/lambda-durable-agent/bootstrap.zip \
  --environment "Variables={A2A_EXECUTOR_QUEUE_URL=$QUEUE_URL}" \
  --timeout 30 \
  --memory-size 256

FN_URL=$(aws lambda create-function-url-config \
  --function-name turul-a2a-durable-agent \
  --auth-type NONE \
  --cors '{"AllowOrigins":["*"],"AllowMethods":["*"],"AllowHeaders":["*"]}' \
  --query 'FunctionUrl' --output text)

aws lambda add-permission \
  --function-name turul-a2a-durable-agent \
  --statement-id allow-function-url \
  --action lambda:InvokeFunctionUrl \
  --principal '*' \
  --function-url-auth-type NONE

echo "FN_URL=$FN_URL"
```

`auth-type NONE` is for the demo. Production: put API Gateway + an authoriser in front.

### 4. Create the **worker** Lambda function (SQS)

```bash
aws lambda create-function \
  --function-name turul-a2a-durable-worker \
  --runtime provided.al2023 \
  --role "$ROLE_ARN" \
  --handler bootstrap \
  --architectures arm64 \
  --zip-file fileb://target/lambda/lambda-durable-worker/bootstrap.zip \
  --environment "Variables={A2A_EXECUTOR_QUEUE_URL=$QUEUE_URL}" \
  --timeout 30 \
  --memory-size 256
```

### 5. Wire the SQS event source mapping → worker

This is the bit that ties it all together. `ReportBatchItemFailures` is required so the worker's `SqsBatchResponse { batch_item_failures }` is honoured — without it, Lambda retries the entire batch on any failure.

```bash
aws lambda create-event-source-mapping \
  --function-name turul-a2a-durable-worker \
  --event-source-arn "$QUEUE_ARN" \
  --batch-size 10 \
  --function-response-types ReportBatchItemFailures
```

### 6. IAM policy for the shared execution role

Minimum perms for both Lambdas (scope to specific queue ARNs in production):

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "SqsProducer",
      "Effect": "Allow",
      "Action": ["sqs:SendMessage", "sqs:GetQueueAttributes"],
      "Resource": "arn:aws:sqs:*:*:turul-a2a-durable-executor-demo"
    },
    {
      "Sid": "SqsConsumer",
      "Effect": "Allow",
      "Action": [
        "sqs:ReceiveMessage",
        "sqs:DeleteMessage",
        "sqs:ChangeMessageVisibility",
        "sqs:GetQueueAttributes",
        "sqs:GetQueueUrl"
      ],
      "Resource": "arn:aws:sqs:*:*:turul-a2a-durable-executor-demo"
    },
    {
      "Sid": "SqsDlq",
      "Effect": "Allow",
      "Action": ["sqs:SendMessage"],
      "Resource": "arn:aws:sqs:*:*:turul-a2a-durable-executor-demo-dlq"
    }
  ]
}
```

Plus the standard `AWSLambdaBasicExecutionRole` managed policy for CloudWatch Logs.

## Try it

```bash
curl -sS -X POST "$FN_URL/message:send" \
  -H 'a2a-version: 1.0' \
  -H 'content-type: application/json' \
  -d '{
    "message": {
      "messageId": "m1",
      "role": "ROLE_USER",
      "parts": [{"text": "hello durable executor"}]
    },
    "configuration": {"returnImmediately": true}
  }' | jq .
```

Expected: `task.status.state == "TASK_STATE_WORKING"`, task id echoed back, HTTP 200.

A few seconds later, CloudWatch Logs for `turul-a2a-durable-worker` shows an invocation with an SQS event payload. The task is now `TASK_STATE_COMPLETED` — the SQS-invoked executor attached the echo artifact and terminated.

Verify via GetTask (note: only works on the same backend both Lambdas point at — the demo's in-memory storage is per-container, so this verification only works in production with a shared backend):

```bash
TASK_ID=<task id from the first response>
curl -sS "$FN_URL/tasks/$TASK_ID" -H 'a2a-version: 1.0' | jq .task.status.state
# → "TASK_STATE_COMPLETED"  (production with shared DynamoDB)
# → "TASK_STATE_WORKING"    (demo with in-memory — container doesn't see the worker's commit)
```

## Storage caveat (again, because it matters)

The in-memory backend shown in this example is for `cargo check` and local-invoke convenience. **On AWS with two separate Lambdas, each container has its own in-memory state — the GetTask verification above will show `WORKING` not `COMPLETED` because the agent's container never sees the worker's commit.**

For any real deployment:

1. Swap `InMemoryA2aStorage` for `DynamoDbA2aStorage` in both `main.rs` files (behind the `dynamodb` feature on `turul-a2a`).
2. Deploy the five DynamoDB tables — `examples/lambda-infra/cloudformation.yaml` provisions them.
3. Keep `.with_push_dispatch_enabled(true)` so terminal commits continue to write pending-dispatch markers (even with no push configs registered, since the ADR-018 §Pending-dispatch optimization skips the write when the task has zero configs).

Both Lambdas call `.with_sqs_return_immediately(queue_url, sqs)`. The worker never actually enqueues — it's consumer-only — but the builder call wires the capability flag and keeps the two binaries interchangeable in terms of the `AppState` they construct.

## Behavior table

| Flag / state | HTTP response | SQS behavior |
|---|---|---|
| `returnImmediately: true`, payload ≤ 256 KiB | 200 `Working`, job enqueued | Worker invocation fires → executor runs → `Completed` |
| `returnImmediately: true`, payload > 256 KiB | 400 `InvalidRequest`, no task persisted | — |
| `returnImmediately: false` (default) | 200 `Completed` (blocking, bounded by API Gateway 29s; worker never involved) | — |
| `returnImmediately: true`, no queue wired | 400 `UnsupportedOperation` (ADR-017 guard) | — |
| Task cancelled via `:cancel` before worker fires | — | Worker commits `Canceled` directly, executor never runs |
| Enqueue fails after task creation | Task transitioned to `Failed` with reason `"durable executor enqueue failed: <error>"` | — |
| Duplicate SQS delivery of same record | — | Second invocation observes terminal, no-ops success (ADR-009 CAS) |

## See also

- [ADR-018](../../docs/adr/ADR-018-lambda-durable-executor-sqs.md) — the design this example demonstrates.
- [`examples/lambda-agent`](../lambda-agent) — request Lambda without durable continuation (ADR-017 reject path; use when `returnImmediately=false` is fine for your call pattern).
- [`examples/lambda-stream-worker`](../lambda-stream-worker) / [`examples/lambda-scheduled-worker`](../lambda-scheduled-worker) — push-delivery workers (ADR-013). A production durable-agent deployment typically wires these alongside to fan out push notifications on terminal events.
- [`examples/lambda-infra`](../lambda-infra) — DynamoDB tables IaC reference.
