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

Encryption: three choices, pick per deployment posture.

| Setting | Acronym | What it is | When |
|---|---|---|---|
| Omit `KmsMasterKeyId` (or set `SqsManagedSseEnabled: true`) | **SSE-SQS** | SQS-owned key, no KMS calls, no KMS cost | Demo / internal deployments where SQS-owned keys are sufficient |
| `KmsMasterKeyId: alias/aws/sqs` *(what this walk-through uses)* | **SSE-KMS, AWS-managed key** | KMS-encrypted with the AWS-managed `alias/aws/sqs` key | When you want KMS-audit-trail but not the cost/policy of a customer-managed CMK |
| `KmsMasterKeyId: <your CMK ARN>` | **SSE-KMS, customer-managed key** | KMS-encrypted with a CMK you control | ADR-018 §Security recommendation for tenant-sensitive deployments — lets you rotate keys and grant/revoke access per your IAM posture |

If you swap in a CMK, the Lambda execution role also needs `kms:Decrypt` on the key for the consumer (worker) and `kms:GenerateDataKey` for the producer (agent). `alias/aws/sqs` uses an AWS-scoped policy so no additional grants are needed.

### 2. Build both Lambda bundles

```bash
cargo lambda build --release -p lambda-durable-agent
cargo lambda build --release -p lambda-durable-worker
```

Outputs:
- `target/lambda/lambda-durable-agent/bootstrap.zip`
- `target/lambda/lambda-durable-worker/bootstrap.zip`

### 3. (If needed) Create an ephemeral IAM role

If you don't already have a Lambda execution role, create one scoped to this demo. Skip this step and substitute `ROLE_ARN=<your existing role>` below if you already have one.

```bash
cat > /tmp/trust-policy.json <<'EOF'
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Principal": {"Service": "lambda.amazonaws.com"},
    "Action": "sts:AssumeRole"
  }]
}
EOF
aws iam create-role \
  --role-name turul-a2a-demo-lambda-role \
  --assume-role-policy-document file:///tmp/trust-policy.json

aws iam attach-role-policy --role-name turul-a2a-demo-lambda-role \
  --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole
aws iam attach-role-policy --role-name turul-a2a-demo-lambda-role \
  --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaSQSQueueExecutionRole

cat > /tmp/sqs-producer-policy.json <<'EOF'
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Action": ["sqs:SendMessage", "sqs:GetQueueAttributes"],
    "Resource": "arn:aws:sqs:*:*:turul-a2a-durable-executor-demo*"
  }]
}
EOF
aws iam put-role-policy --role-name turul-a2a-demo-lambda-role \
  --policy-name SqsProducer \
  --policy-document file:///tmp/sqs-producer-policy.json

ROLE_ARN=$(aws iam get-role --role-name turul-a2a-demo-lambda-role --query 'Role.Arn' --output text)
echo "ROLE_ARN=$ROLE_ARN"

# IAM can take a few seconds to propagate before Lambda accepts the role.
sleep 10
```

### 4. Create the **request** Lambda function (HTTP)

```bash
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

### 5. Create the **worker** Lambda function (SQS)

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

### 6. Wire the SQS event source mapping → worker

This is the bit that ties it all together. `ReportBatchItemFailures` is required so the worker's `SqsBatchResponse { batch_item_failures }` is honoured — without it, Lambda retries the entire batch on any failure.

```bash
aws lambda create-event-source-mapping \
  --function-name turul-a2a-durable-worker \
  --event-source-arn "$QUEUE_ARN" \
  --batch-size 10 \
  --function-response-types ReportBatchItemFailures
```

### Reference: full IAM policy

Step 3's ephemeral role attaches the `AWSLambdaBasicExecutionRole` + `AWSLambdaSQSQueueExecutionRole` AWS-managed policies plus a narrow SQS producer inline policy. The worker reads from SQS (covered by `AWSLambdaSQSQueueExecutionRole`); the agent writes to SQS (covered by the inline producer policy). If you're wiring a production role by hand, this is the full shape:

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

Plus `AWSLambdaBasicExecutionRole` for CloudWatch Logs.

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

A few seconds later, the SQS event source mapping fires the worker Lambda:

```bash
aws logs tail /aws/lambda/turul-a2a-durable-worker --since 3m --format short
```

### Expected demo behavior (important)

**On this in-memory demo**, the worker log will contain a single line like:

```
ERROR ... task not found on SQS dequeue item=<uuid> tenant= task_id=<task id from the HTTP response>
```

**This is correct and is what the storage caveat below warns about.** The pipeline is wired end-to-end — the HTTP Lambda enqueued, the SQS event source mapping delivered, the worker deserialised the envelope, decoded the `tenant` + `task_id` correctly, and tried to load the task. The task was written into the **agent Lambda's** `InMemoryA2aStorage`, which lives in a different container; the worker's in-memory storage has no record of it, so `get_task` returns `None` and the worker reports the record as a batch-item failure. SQS retries three times (per our `RedrivePolicy`), then the record lands in the DLQ. Everything the framework is responsible for — envelope serialisation, size preflight, event source mapping, partial-batch response, terminal-no-op detection — is working.

In **production** with a shared DynamoDB backend (see `examples/lambda-infra/cloudformation.yaml`), the worker's `get_task` finds the task, runs the executor, and commits the terminal. GetTask then shows the task is `TASK_STATE_COMPLETED`:

```bash
TASK_ID=<task id from the first response>
curl -sS "$FN_URL/tasks/$TASK_ID" -H 'a2a-version: 1.0' | jq .task.status.state
# production with shared DynamoDB: "TASK_STATE_COMPLETED"
# demo with in-memory:             "TASK_STATE_WORKING" (agent's container never sees worker's commit)
```

## Tear down

```bash
# 1. Delete the SQS event source mapping.
MAPPING_UUID=$(aws lambda list-event-source-mappings \
  --function-name turul-a2a-durable-worker \
  --query 'EventSourceMappings[0].UUID' --output text)
aws lambda delete-event-source-mapping --uuid "$MAPPING_UUID"

# 2. Delete both Lambda functions (also auto-deletes the Function URL).
aws lambda delete-function --function-name turul-a2a-durable-agent
aws lambda delete-function --function-name turul-a2a-durable-worker

# 3. Delete CloudWatch log groups (optional but tidy).
aws logs delete-log-group --log-group-name /aws/lambda/turul-a2a-durable-agent
aws logs delete-log-group --log-group-name /aws/lambda/turul-a2a-durable-worker

# 4. Delete both SQS queues.
aws sqs delete-queue --queue-url "$QUEUE_URL"
aws sqs delete-queue --queue-url "$(aws sqs get-queue-url \
  --queue-name turul-a2a-durable-executor-demo-dlq --query QueueUrl --output text)"

# 5. Delete the demo IAM role.
aws iam detach-role-policy --role-name turul-a2a-demo-lambda-role \
  --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole
aws iam detach-role-policy --role-name turul-a2a-demo-lambda-role \
  --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaSQSQueueExecutionRole
aws iam delete-role-policy --role-name turul-a2a-demo-lambda-role --policy-name SqsProducer
aws iam delete-role --role-name turul-a2a-demo-lambda-role
```

Verify everything is gone:

```bash
aws lambda list-functions \
  --query 'Functions[?contains(FunctionName, `turul-a2a-durable`)].FunctionName'
aws sqs list-queues --queue-name-prefix turul-a2a
aws iam get-role --role-name turul-a2a-demo-lambda-role 2>&1 | tail -1
# → empty; empty; NoSuchEntity

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
