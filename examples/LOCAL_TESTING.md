# Local testing + mixed environments — Lambda examples

The Lambda adapter is infrastructure-agnostic: the binary talks AWS
SDK to whatever endpoint `aws_config` points at (real AWS, LocalStack,
stub). `cargo lambda watch` emulates the Lambda runtime locally. The
only AWS control-plane piece with no local equivalent is the **event
source mapping** that polls SQS and invokes your function — that gap
is worked around per topology below.

## Test-coverage matrix

| Scenario | Request Lambda | SQS | Worker Lambda | Storage | Set-up cost |
|---|---|---|---|---|---|
| **Unit / in-process** | `#[tokio::test]` | `FakeQueue` | `#[tokio::test]` | `InMemoryA2aStorage` | zero — `cargo test` |
| **Local HTTP only** | `cargo lambda watch` | n/a | n/a | in-memory | ~60s cold compile |
| **Local worker dispatch** | n/a | n/a | `cargo lambda watch` + `cargo lambda invoke --data-file <SqsEvent>.json` | any backend AWS SDK can reach | ~60s + synthetic event JSON |
| **Hybrid** | `cargo lambda watch` | **real AWS** | `cargo lambda watch` + manual poll-invoke | **real AWS** | AWS creds + queue / tables |
| **LocalStack** | `cargo lambda watch` | LocalStack | `cargo lambda watch` + LocalStack ESM | LocalStack | Docker + LocalStack container |
| **Full cloud** | deployed | AWS | deployed (AWS ESM) | AWS | README deploy walk-through |

## 1. In-process (automated)

```bash
cargo test --workspace                       # 59 result groups, 0 failures
cargo test -p turul-a2a-aws-lambda --features sqs
cargo test -p turul-a2a durable_path        # payload-survival
```

The crate-level test
`durable_path_preserves_text_task_id_and_context_id_across_enqueue_dequeue`
drives `core_send_message` → `FakeQueue` (captures the enqueued
`QueuedExecutorJob`) → `run_queued_executor_job` (direct call) →
asserts probe text + task id + context id + metadata keys reach the
terminal artifact. ~100ms, no infra.

## 2. Local HTTP only

Runs `lambda-agent` under the Lambda runtime emulator. Proves the
full lambda_runtime envelope conversion + axum router + executor
dispatch works locally.

```bash
A2A_PUBLIC_URL=http://localhost:9000 cargo lambda watch -p lambda-agent &

curl http://localhost:9000/.well-known/agent-card.json
curl -X POST http://localhost:9000/message:send \
  -H 'a2a-version: 1.0' \
  -H 'content-type: application/json' \
  -d '{"message":{"messageId":"m1","role":"ROLE_USER","parts":[{"text":"probe"}]}}'
```

## 3. Local worker dispatch (synthetic SQS event)

Drives the worker's `handle_sqs` path without a real SQS queue. A
hand-authored `SqsEvent` JSON containing a real `QueuedExecutorJob`
body.

```bash
cat > /tmp/sqs-event.json <<EOF
{
  "Records": [
    {
      "messageId": "probe-1",
      "receiptHandle": "local-receipt",
      "body": "{\\"version\\":1,\\"tenant\\":\\"default\\",\\"owner\\":\\"anonymous\\",\\"task_id\\":\\"<uuid-v7>\\",\\"context_id\\":\\"<uuid-v7>\\",\\"message\\":{\\"messageId\\":\\"m1\\",\\"role\\":\\"ROLE_USER\\",\\"content\\":[{\\"text\\":{\\"text\\":\\"local-probe\\"}}]},\\"claims\\":null,\\"enqueued_at_micros\\":0}",
      "attributes": {},
      "messageAttributes": {},
      "md5OfBody": "x",
      "eventSource": "aws:sqs",
      "eventSourceARN": "arn:aws:sqs:ap-southeast-2:000000000000:fake",
      "awsRegion": "ap-southeast-2"
    }
  ]
}
EOF

cd examples/lambda-durable-worker
AWS_REGION=ap-southeast-2 cargo lambda watch &
cargo lambda invoke bootstrap --data-file /tmp/sqs-event.json
```

If the task in the event body doesn't exist in the storage backend
the worker returns `batchItemFailures` for that record and logs
`get_task failed on SQS dequeue ... ResourceNotFoundException`
(DynamoDB) or `task not found on SQS dequeue` (any backend). That's
the documented DLQ path and proves the dispatcher wiring end-to-end.

To pre-seed a task: run the request-side flow first against the same
backend.

## 4. Hybrid — local Lambda + real AWS SQS/DynamoDB

The Lambda binary's `aws_sdk_sqs::Client` and `aws_sdk_dynamodb::Client`
accept any endpoint. Point them at real AWS by supplying credentials
and the region, and run the binary locally via `cargo lambda watch`.

```bash
export AWS_REGION=ap-southeast-2
export A2A_EXECUTOR_QUEUE_URL=https://sqs.ap-southeast-2.amazonaws.com/<acct>/<queue>
export A2A_PUBLIC_URL=http://localhost:9000
# DynamoDB tables already provisioned per examples/lambda-infra/cloudformation.yaml

cargo lambda watch -p lambda-durable-agent
```

HTTP requests hit your local binary; your local binary enqueues to
real SQS; real DynamoDB accepts the task row. To close the loop
locally, run a poll-and-invoke shim:

```bash
while true; do
  MSG=$(aws sqs receive-message --queue-url "$A2A_EXECUTOR_QUEUE_URL" \
    --wait-time-seconds 20 --max-number-of-messages 1)
  [ -z "$MSG" ] && continue
  BODY=$(echo "$MSG" | jq -r '.Messages[0].Body')
  HANDLE=$(echo "$MSG" | jq -r '.Messages[0].ReceiptHandle')
  ID=$(echo "$MSG" | jq -r '.Messages[0].MessageId')
  # wrap into SqsEvent
  jq -n --arg body "$BODY" --arg id "$ID" '{Records:[{messageId:$id,receiptHandle:"x",body:$body,attributes:{},messageAttributes:{},md5OfBody:"x",eventSource:"aws:sqs",eventSourceARN:"arn:aws:sqs:ap-southeast-2:000000000000:fake",awsRegion:"ap-southeast-2"}]}' > /tmp/sqs-event.json
  ( cd examples/lambda-durable-worker && cargo lambda invoke bootstrap --data-file /tmp/sqs-event.json )
  aws sqs delete-message --queue-url "$A2A_EXECUTOR_QUEUE_URL" --receipt-handle "$HANDLE"
done
```

That 30-line bash shim is your local replacement for AWS's event
source mapping. It is not framework code — the framework does not
ship a polling runner because the topology is adopter-specific.

## 5. LocalStack — full local loop

Runs SQS, DynamoDB, and the ESM control plane in a Docker container.
The Lambda binary runs locally via `cargo lambda watch`.

```bash
docker run --rm -p 4566:4566 localstack/localstack
export AWS_ENDPOINT_URL=http://localhost:4566
export AWS_REGION=us-east-1
export AWS_ACCESS_KEY_ID=test
export AWS_SECRET_ACCESS_KEY=test
# provision queue + tables + ESM in LocalStack; point cargo lambda watch at LocalStack endpoints.
```

This is the closest-to-production local setup. Pay the container
overhead, get full fidelity. No framework changes required — the AWS
SDK clients inside the Lambda binary talk to LocalStack the same way
they talk to AWS.

## 6. Full cloud

See `examples/lambda-durable-agent/README.md` and
`examples/lambda-durable-single/README.md` for the AWS-deploy
walk-through.
