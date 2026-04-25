# lambda-durable-single

**ADR-018 single-Lambda demo** — one function handles both HTTP and SQS events. This is the simplest, self-contained ADR-018 Pattern B end-to-end example.

Verified end-to-end on AWS `ap-southeast-2` (2026-04-25): HTTP `returnImmediately=true` → task enqueued → SQS trigger → same warm container runs the executor → task reaches `TASK_STATE_COMPLETED`. Zero ERROR logs.

## How it works (single container, one Lambda)

1. HTTP `POST /message:send` with `configuration.returnImmediately = true` arrives at the Function URL.
2. `LambdaA2aServerBuilder::with_sqs_return_immediately(...)` routes the request through the ADR-018 enqueue path: task created as `WORKING`, `QueuedExecutorJob` enqueued, `200 OK` returned.
3. The SQS event source mapping fires the **same Lambda function** with the enqueued record.
4. Because the function is deployed with `ReservedConcurrency=1`, AWS uses the same (now warm) container. The worker's `handle_sqs` path loads the task from the container's `InMemoryA2aStorage`, runs the executor, commits `COMPLETED`.
5. A subsequent `GET /tasks/{id}` shows the terminal artifact.

The dual-event routing is now one call in `main.rs`:

```rust
LambdaA2aServerBuilder::new()
    .executor(DurableEchoExecutor)
    .storage(InMemoryA2aStorage::new())
    .with_sqs_return_immediately(queue_url, sqs)
    .run()           // default — with the `sqs` feature, dispatches HTTP and SQS
    .await
```

Since 0.1.15 (gated on the `sqs` feature), the adapter owns envelope classification and dispatch — adopter `main.rs` never touches `lambda_http::run`, `lambda_runtime::run`, `service_fn`, or `SqsEvent`. The builder-fluent `.run()` is the one-call default. For a deployment that wants strict fail-fast on a non-HTTP-or-SQS event shape, drop down one level:

```rust
let handler = LambdaA2aServerBuilder::new()
    .executor(DurableEchoExecutor)
    .storage(InMemoryA2aStorage::new())
    .with_sqs_return_immediately(queue_url, sqs)
    .build()?;
handler.run_http_and_sqs().await     // explicit dual-event runner
```

Earlier revisions of this example open-coded ~80 lines of `LambdaRequest` → `http::Request<Body>` / `ApiGatewayV2httpResponse` plumbing directly in `main.rs` — all of that now lives in the framework. `main.rs` is down to ~15 lines of wiring (storage + executor + queue URL + the runner call).

## When to use this vs the two-Lambda example

| Use | Demo shape |
|---|---|
| **`lambda-durable-single`** *(this crate)* | Quickest demo. One Lambda, `ReservedConcurrency=1`, `InMemoryA2aStorage`. Container-level storage means HTTP and SQS invocations share state. **Not for production** — the concurrency pin caps you at one inflight invocation. |
| `lambda-durable-agent` + `lambda-durable-worker` | Production shape. Two Lambda functions, shared DynamoDB backend (via `examples/lambda-infra/`), no concurrency limits. What you'd actually deploy. |

## Life-of-a-Task verification (post-deploy)

`scripts/life-of-a-task.sh` drives the deployed Function URL through
the A2A spec's task-refinement flow:

```bash
bash examples/lambda-durable-single/scripts/life-of-a-task.sh "$FN_URL"
```

The script POSTs an originating message, GETs the task, POSTs a
refinement (`referenceTaskIds=[task1.id]`, same `contextId`), and
GETs the new task. It asserts the spec invariants — task immutability
across refinement (different `task_id`s), `contextId` continuity,
both terminals `COMPLETED`, artifacts echo their own ids and the probe
text. Eight assertions; exits with the failure count.

## Local testing

Before deploying to AWS, see `examples/LOCAL_TESTING.md` for the
local-first test matrix — automated in-process tests, `cargo lambda
watch` + `cargo lambda invoke` for HTTP and SQS dispatch, hybrid
patterns against real AWS, and LocalStack full-local setup.

## Prerequisites

- AWS account + credentials (`aws sts get-caller-identity`).
- `cargo-lambda` (`cargo install cargo-lambda`).
- A Lambda execution role (or follow step 3 below to create one).

## Deploy

### 1. SQS queue + DLQ

```bash
aws sqs create-queue --queue-name turul-a2a-durable-executor-demo-dlq
DLQ_URL=$(aws sqs get-queue-url --queue-name turul-a2a-durable-executor-demo-dlq --query QueueUrl --output text)
DLQ_ARN=$(aws sqs get-queue-attributes --queue-url "$DLQ_URL" --attribute-names QueueArn --query 'Attributes.QueueArn' --output text)

cat > /tmp/queue-attrs.json <<EOF
{
  "VisibilityTimeout": "60",
  "MessageRetentionPeriod": "345600",
  "SqsManagedSseEnabled": "true",
  "RedrivePolicy": "{\"deadLetterTargetArn\":\"$DLQ_ARN\",\"maxReceiveCount\":\"3\"}"
}
EOF
aws sqs create-queue --queue-name turul-a2a-durable-executor-demo --attributes file:///tmp/queue-attrs.json

QUEUE_URL=$(aws sqs get-queue-url --queue-name turul-a2a-durable-executor-demo --query QueueUrl --output text)
QUEUE_ARN=$(aws sqs get-queue-attributes --queue-url "$QUEUE_URL" --attribute-names QueueArn --query 'Attributes.QueueArn' --output text)
```

### 2. Build

```bash
cargo lambda build --release --arm64 -p lambda-durable-single
mkdir -p /tmp/lambda-build/single
cp target/lambda/bootstrap/bootstrap /tmp/lambda-build/single/bootstrap
(cd /tmp/lambda-build/single && zip -q single.zip bootstrap)
```

### 3. (If needed) Create an ephemeral IAM role

```bash
cat > /tmp/trust-policy.json <<'EOF'
{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}
EOF
aws iam create-role --role-name turul-a2a-demo-lambda-role --assume-role-policy-document file:///tmp/trust-policy.json
aws iam attach-role-policy --role-name turul-a2a-demo-lambda-role --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole
aws iam attach-role-policy --role-name turul-a2a-demo-lambda-role --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaSQSQueueExecutionRole

cat > /tmp/sqs-producer-policy.json <<'EOF'
{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["sqs:SendMessage","sqs:GetQueueAttributes"],"Resource":"arn:aws:sqs:*:*:turul-a2a-durable-executor-demo*"}]}
EOF
aws iam put-role-policy --role-name turul-a2a-demo-lambda-role --policy-name SqsProducer --policy-document file:///tmp/sqs-producer-policy.json

ROLE_ARN=$(aws iam get-role --role-name turul-a2a-demo-lambda-role --query 'Role.Arn' --output text)
sleep 10   # IAM propagation
```

### 4. Create the Lambda with ReservedConcurrency=1

```bash
aws lambda create-function \
  --function-name turul-a2a-durable-single \
  --runtime provided.al2023 \
  --role "$ROLE_ARN" \
  --handler bootstrap \
  --architectures arm64 \
  --zip-file fileb:///tmp/lambda-build/single/single.zip \
  --environment "Variables={A2A_EXECUTOR_QUEUE_URL=$QUEUE_URL}" \
  --timeout 30 --memory-size 256

# PINS the function to a single container so HTTP and SQS invocations
# share state. Demo-only — remove for real deployments.
aws lambda put-function-concurrency \
  --function-name turul-a2a-durable-single \
  --reserved-concurrent-executions 1
```

### 5. Function URL + SQS event source mapping

```bash
FN_URL=$(aws lambda create-function-url-config \
  --function-name turul-a2a-durable-single \
  --auth-type NONE \
  --cors '{"AllowOrigins":["*"],"AllowMethods":["*"],"AllowHeaders":["*"]}' \
  --query 'FunctionUrl' --output text)

aws lambda add-permission \
  --function-name turul-a2a-durable-single \
  --statement-id allow-function-url \
  --action lambda:InvokeFunctionUrl \
  --principal '*' \
  --function-url-auth-type NONE >/dev/null

aws lambda create-event-source-mapping \
  --function-name turul-a2a-durable-single \
  --event-source-arn "$QUEUE_ARN" \
  --batch-size 10 \
  --function-response-types ReportBatchItemFailures

echo "FN_URL=$FN_URL"
```

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

# 2. Wait for the SQS trigger + executor run.
sleep 8

# 3. Verify the payload survived HTTP → SQS → dequeue → executor:
#    state=COMPLETED, artifact echoes the probe text, task_id, context_id, and metadata keys.
curl -sS "$FN_URL/tasks/$TASK_ID" -H 'a2a-version: 1.0' \
  | jq '{state: .task.status.state, artifact: .task.artifacts[0].parts[0].text}'
# → {
#     "state": "TASK_STATE_COMPLETED",
#     "artifact": "echoed from durable executor\n  text: payload-survival-probe-…\n  task_id: … context_id: …\n  metadata_keys: [attempt, trigger_id]"
#   }

# 4. Assert that the artifact text contains the probe + task_id + context_id +
#    metadata KEYS (values must not leak — see ADR-018 follow-on guardrail).
ART=$(curl -sS "$FN_URL/tasks/$TASK_ID" -H 'a2a-version: 1.0' | jq -r '.task.artifacts[0].parts[0].text')
echo "$ART" | grep -q "$PROBE"      && echo "OK probe text"    || echo "MISSING probe text"
echo "$ART" | grep -q "$TASK_ID"    && echo "OK task_id"       || echo "MISSING task_id"
echo "$ART" | grep -q "$CONTEXT_ID" && echo "OK context_id"    || echo "MISSING context_id"
echo "$ART" | grep -q "trigger_id"  && echo "OK metadata keys" || echo "MISSING metadata keys"
echo "$ART" | grep -q "trig-"       && echo "LEAK value!"      || echo "OK values not echoed"

# 4. Zero ERRORs in the log:
aws logs tail /aws/lambda/turul-a2a-durable-single --since 3m --format short
```

## Tear down

```bash
MAPPING_UUID=$(aws lambda list-event-source-mappings --function-name turul-a2a-durable-single --query 'EventSourceMappings[0].UUID' --output text)
aws lambda delete-event-source-mapping --uuid "$MAPPING_UUID"
aws lambda delete-function --function-name turul-a2a-durable-single
aws logs delete-log-group --log-group-name /aws/lambda/turul-a2a-durable-single 2>/dev/null
aws sqs delete-queue --queue-url "$QUEUE_URL"
aws sqs delete-queue --queue-url "$DLQ_URL"
aws iam detach-role-policy --role-name turul-a2a-demo-lambda-role --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole
aws iam detach-role-policy --role-name turul-a2a-demo-lambda-role --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaSQSQueueExecutionRole
aws iam delete-role-policy --role-name turul-a2a-demo-lambda-role --policy-name SqsProducer
aws iam delete-role --role-name turul-a2a-demo-lambda-role
```

## Why ReservedConcurrency=1?

Lambda uses separate containers for concurrent invocations. Without a concurrency pin, the HTTP invocation and the SQS-triggered invocation are likely to hit different containers, and each container has its own `InMemoryA2aStorage` — the worker wouldn't see the task.

`ReservedConcurrency=1` caps the function at one inflight invocation. The HTTP call finishes first, the container goes warm, then the SQS-triggered invocation reuses that same warm container and its in-memory state. This is why the demo works without a shared backend.

**For production** — where you want horizontal scale and can't pin to one container — swap `InMemoryA2aStorage` for `DynamoDbA2aStorage` (or your shared backend of choice) and remove the concurrency reservation. That's exactly what the `lambda-durable-agent` + `lambda-durable-worker` example does.

## See also

- [ADR-018](../../docs/adr/ADR-018-lambda-durable-executor-sqs.md) — the design this example demonstrates.
- [`examples/lambda-durable-agent`](../lambda-durable-agent) + [`examples/lambda-durable-worker`](../lambda-durable-worker) — production shape with DynamoDB + horizontal scale.
- [`examples/lambda-infra`](../lambda-infra) — DynamoDB tables IaC if you outgrow the in-memory demo.
