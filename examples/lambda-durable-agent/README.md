# lambda-durable-agent + lambda-durable-worker

**ADR-018 production-shape demo**: Lambda durable executor continuation via AWS SQS, with shared DynamoDB storage.

Two Lambda functions share a DynamoDB backend (via `examples/lambda-infra/`), so the agent and worker run in separate containers without concurrency restrictions. This is what a production ADR-018 deployment looks like.

For a simpler single-Lambda demo that doesn't need DynamoDB — suitable for `cargo lambda watch` loops and throwaway experiments — see [`examples/lambda-durable-single`](../lambda-durable-single).

Verified end-to-end on AWS `ap-southeast-2` (2026-04-25): HTTP `returnImmediately=true` → task enqueued → SQS trigger on the worker Lambda → task reaches `TASK_STATE_COMPLETED` via the shared DynamoDB backend. Zero ERROR logs.

Without the SQS wiring shown here, the Lambda adapter rejects `returnImmediately=true` with `UnsupportedOperationError` per ADR-017.

## Two Lambdas, one deployable workflow

ADR-018's dispatch pattern splits across two Lambda functions that share the same storage backend:

| Crate | Event source | Role |
|---|---|---|
| `lambda-durable-agent` | HTTP (Function URL / API Gateway / ALB) | Receives `/message:send` with `returnImmediately=true`, size-checks the envelope, creates the task as `WORKING`, enqueues a `QueuedExecutorJob` on SQS, returns 200. Executor is NOT run locally. |
| `lambda-durable-worker` | SQS event source mapping | Triggered by the queue. Per record: loads the task, idempotency-checks it (terminal → no-op), checks the ADR-012 cancel marker (set → CANCELED direct commit, executor never invoked), otherwise runs the executor via `run_queued_executor_job`. Returns `SqsBatchResponse` with partial-batch failure semantics. |

This split matches the existing pattern (`lambda-agent` + `lambda-stream-worker` + `lambda-scheduled-worker`). The agent writes the task, the worker reads it — both via `DynamoDbA2aStorage` pointed at the same five tables (ADR-009 same-backend requirement).

## Prerequisites

- AWS account + credentials in your environment (`aws sts get-caller-identity`).
- [`cargo-lambda`](https://www.cargo-lambda.info/) installed (`cargo install cargo-lambda`).
- A Lambda execution role you can attach (you can create one in the console or via CLI).

## Deploy

### 1. Deploy the DynamoDB tables

Both Lambdas point at the five A2A tables from `examples/lambda-infra/cloudformation.yaml` (`a2a_tasks`, `a2a_push_configs`, `a2a_task_events`, `a2a_push_deliveries`, `a2a_push_pending_dispatches`).

```bash
aws cloudformation deploy \
  --stack-name a2a-tables \
  --template-file ../lambda-infra/cloudformation.yaml \
  --no-fail-on-empty-changeset
```

Wait for the stack to finish (~30s) before creating the Lambda functions. You can verify with `aws dynamodb list-tables --query 'TableNames[?starts_with(@,\`a2a_\`)]'`.

### 2. Create the SQS queue + DLQ

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
#
# Encryption: SSE-SQS (SQS-owned key). Free, no KMS calls, no extra IAM.
# Sufficient for this demo (no tenant data, no JWT claims). Production
# deployments: see "Encryption for production" below.
aws sqs create-queue \
  --queue-name turul-a2a-durable-executor-demo \
  --attributes "{
    \"VisibilityTimeout\": \"60\",
    \"MessageRetentionPeriod\": \"345600\",
    \"SqsManagedSseEnabled\": \"true\",
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

### Encryption for production

The demo uses **SSE-SQS** — SQS-owned key, no KMS calls, no per-request charges. That's the "lower-risk / internal" posture from ADR-018 §Security, and it fits a throwaway echo demo. Three choices, pick per your deployment posture:

| Setting | Acronym | Cost | When |
|---|---|---|---|
| `SqsManagedSseEnabled: true` *(what this walk-through uses)* | **SSE-SQS** | Free — no KMS calls | Demo + internal deployments. Sufficient where the AWS-owned-account boundary is enough. |
| `KmsMasterKeyId: alias/aws/sqs` | **SSE-KMS, AWS-managed key** | KMS request charges (~$0.03 / 10k requests) + CloudTrail audit-trail visibility | When you want a KMS audit trail without managing a CMK. |
| `KmsMasterKeyId: <your CMK ARN>` | **SSE-KMS, customer-managed key** | CMK monthly fee + request charges | **ADR-018 §Security recommendation for tenant-sensitive deployments.** Lets you rotate keys and grant/revoke access per your IAM posture. |

**Cost rider.** `SqsManagedSseEnabled: true` is the only option with zero encryption-related charges — SQS uses its own keys, no `kms:*` calls, no per-request KMS fees, no CMK monthly. Both `KmsMasterKeyId` options go through KMS and attract the usual KMS pricing (request charges for every `SendMessage`/`ReceiveMessage`, plus the CMK monthly fee if you own the key).

**Why set it explicitly.** New SQS queues currently default to SSE-SQS, but the default can drift with AWS policy changes. Writing `SqsManagedSseEnabled: true` into your IaC / setup commands locks the "no-KMS-cost" posture in source so a future default flip won't quietly opt you into KMS charges.

**IAM rider.** If you swap in a CMK, the Lambda execution role also needs `kms:Decrypt` on the key for the consumer (worker) and `kms:GenerateDataKey` for the producer (agent). `alias/aws/sqs` uses an AWS-scoped policy so no additional grants are needed. `SqsManagedSseEnabled` needs no KMS IAM at all.

### 3. Build both Lambda bundles

```bash
cargo lambda build --release -p lambda-durable-agent
cargo lambda build --release -p lambda-durable-worker
```

Outputs:
- `target/lambda/lambda-durable-agent/bootstrap.zip`
- `target/lambda/lambda-durable-worker/bootstrap.zip`

### 4. (If needed) Create an ephemeral IAM role

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

# DynamoDB perms (both Lambdas read + write the five A2A tables).
cat > /tmp/dynamo-policy.json <<'EOF'
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Action": [
      "dynamodb:GetItem", "dynamodb:PutItem", "dynamodb:UpdateItem",
      "dynamodb:DeleteItem", "dynamodb:Query", "dynamodb:Scan",
      "dynamodb:BatchGetItem", "dynamodb:BatchWriteItem",
      "dynamodb:TransactWriteItems", "dynamodb:TransactGetItems"
    ],
    "Resource": [
      "arn:aws:dynamodb:*:*:table/a2a_tasks",
      "arn:aws:dynamodb:*:*:table/a2a_push_configs",
      "arn:aws:dynamodb:*:*:table/a2a_task_events",
      "arn:aws:dynamodb:*:*:table/a2a_push_deliveries",
      "arn:aws:dynamodb:*:*:table/a2a_push_pending_dispatches"
    ]
  }]
}
EOF
aws iam put-role-policy --role-name turul-a2a-demo-lambda-role \
  --policy-name DynamoDbTables \
  --policy-document file:///tmp/dynamo-policy.json

ROLE_ARN=$(aws iam get-role --role-name turul-a2a-demo-lambda-role --query 'Role.Arn' --output text)
echo "ROLE_ARN=$ROLE_ARN"

# IAM can take a few seconds to propagate before Lambda accepts the role.
sleep 10
```

### 5. Create the **request** Lambda function (HTTP)

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

### 6. Create the **worker** Lambda function (SQS)

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

### 7. Wire the SQS event source mapping → worker

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
# 1. Fire the HTTP request with a distinctive probe text + metadata.
PROBE="payload-survival-probe-$(date +%s)"
RESP=$(curl -sS -X POST "$FN_URL/message:send" \
  -H 'a2a-version: 1.0' \
  -H 'content-type: application/json' \
  -d "{
    \"message\": {
      \"messageId\": \"m1\",
      \"role\": \"ROLE_USER\",
      \"parts\": [{\"text\": \"$PROBE\"}],
      \"metadata\": {\"trigger_id\": \"trig-$PROBE\", \"attempt\": 1}
    },
    \"configuration\": {\"returnImmediately\": true}
  }")
echo "$RESP" | jq .
TASK_ID=$(echo "$RESP" | jq -r .task.id)
CONTEXT_ID=$(echo "$RESP" | jq -r .task.contextId)

# 2. Wait for SQS → worker → DynamoDB commit.
sleep 8

# 3. Verify the payload survived HTTP → SQS → worker → executor:
#    state=COMPLETED, artifact echoes the probe text, task_id, context_id, and metadata keys.
curl -sS "$FN_URL/tasks/$TASK_ID" -H 'a2a-version: 1.0' \
  | jq '{state: .task.status.state, artifact: .task.artifacts[0].parts[0].text}'

# 4. Assert the artifact contains the probe + task_id + context_id + metadata KEYS;
#    metadata values must NOT leak into the artifact (keys-only guardrail).
ART=$(curl -sS "$FN_URL/tasks/$TASK_ID" -H 'a2a-version: 1.0' | jq -r '.task.artifacts[0].parts[0].text')
echo "$ART" | grep -q "$PROBE"      && echo "OK probe text"    || echo "MISSING probe text"
echo "$ART" | grep -q "$TASK_ID"    && echo "OK task_id"       || echo "MISSING task_id"
echo "$ART" | grep -q "$CONTEXT_ID" && echo "OK context_id"    || echo "MISSING context_id"
echo "$ART" | grep -q "trigger_id"  && echo "OK metadata keys" || echo "MISSING metadata keys"
echo "$ART" | grep -q "trig-"       && echo "LEAK value!"      || echo "OK values not echoed"

# 5. Clean worker log — no ERRORs on the happy path.
aws logs tail /aws/lambda/turul-a2a-durable-worker --since 3m --format short
aws logs tail /aws/lambda/turul-a2a-durable-agent  --since 3m --format short
```

The shared DynamoDB backend is what makes this work — the agent writes the task to `a2a_tasks`, the worker reads the same row, runs the executor, and the terminal commit goes through the same CAS-guarded atomic store as a single-process deployment would use.

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

# 5. Delete the DynamoDB tables (CloudFormation stack).
aws cloudformation delete-stack --stack-name a2a-tables
aws cloudformation wait stack-delete-complete --stack-name a2a-tables

# 6. Delete the demo IAM role.
aws iam detach-role-policy --role-name turul-a2a-demo-lambda-role \
  --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole
aws iam detach-role-policy --role-name turul-a2a-demo-lambda-role \
  --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaSQSQueueExecutionRole
aws iam delete-role-policy --role-name turul-a2a-demo-lambda-role --policy-name SqsProducer
aws iam delete-role-policy --role-name turul-a2a-demo-lambda-role --policy-name DynamoDbTables
aws iam delete-role --role-name turul-a2a-demo-lambda-role
```

Verify everything is gone:

```bash
aws lambda list-functions \
  --query 'Functions[?contains(FunctionName, `turul-a2a-durable`)].FunctionName'
aws sqs list-queues --queue-name-prefix turul-a2a
aws iam get-role --role-name turul-a2a-demo-lambda-role 2>&1 | tail -1
# → empty; empty; NoSuchEntity

## Shared storage + builder wiring

The two-Lambda topology runs the request and consumer sides in **different** containers, so both binaries must point at the same backend — the worker loads the task the request Lambda already wrote and commits the terminal through the same CAS-guarded atomic store (ADR-009 same-backend requirement). Both `main.rs` files in this demo use `DynamoDbA2aStorage` (behind the `dynamodb` feature on `turul-a2a`) against the five tables provisioned by `examples/lambda-infra/cloudformation.yaml`.

The two builders differ by exactly one line:

- **Request Lambda (`lambda-durable-agent`)** calls `.with_sqs_return_immediately(queue_url, sqs)` because it *enqueues* durable jobs onto SQS. The builder flips `supports_return_immediately` as a side effect of supplying the queue (capability, not intent).
- **Consumer Lambda (`lambda-durable-worker`)** does **not** call `.with_sqs_return_immediately(...)`. A worker never enqueues — it only reads jobs an SQS event-source mapping delivers to it and runs their executors. The runner method (`handler.run_sqs_only().await`) is what wires the SQS event loop.

If the deployment needs push-notification delivery, wire `.push_delivery_store(storage.clone())` on both builders and flip `.with_push_dispatch_enabled(true)` on the storage so terminal commits write pending-dispatch markers (ADR-013 §4.3). This demo does not register push configs, so both builders omit push-delivery wiring.

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
- [`examples/lambda-durable-single`](../lambda-durable-single) — simplest ADR-018 demo: one Lambda, in-memory, no DynamoDB. Good for quick experiments or `cargo lambda watch` loops.
- [`examples/lambda-agent`](../lambda-agent) — request Lambda without durable continuation (ADR-017 reject path; use when `returnImmediately=false` is fine for your call pattern).
- [`examples/lambda-stream-worker`](../lambda-stream-worker) / [`examples/lambda-scheduled-worker`](../lambda-scheduled-worker) — push-delivery workers (ADR-013). A production durable-agent deployment typically wires these alongside to fan out push notifications on terminal events.
- [`examples/lambda-infra`](../lambda-infra) — DynamoDB tables IaC (deployed by step 1 above).
