# lambda-agent

The **request Lambda** in an ADR-013 Lambda deployment — handles A2A HTTP and
JSON-RPC traffic (`/message:send`, `/tasks/{id}`, `/jsonrpc`, etc.) via
`LambdaA2aServerBuilder`, which reuses the same router as the binary server
under a `lambda_http` adapter.

This example is **one of three Lambdas** in a complete push-capable deployment:

1. `lambda-agent` *(this crate)* — request Lambda, writes pending-dispatch markers.
2. `lambda-stream-worker` — DynamoDB Stream trigger, fast-path push recovery.
3. `lambda-scheduled-worker` — EventBridge Scheduler trigger, mandatory backstop.

See ADR-008 (Lambda adapter) and ADR-013 (Lambda push-delivery parity).

## Build & run locally

```bash
# Local invoke loop via cargo-lambda. The watch process listens on
# http://localhost:9000 and auto-wraps plain HTTP requests into the
# Lambda event the function expects.
cargo lambda watch -p lambda-agent

# In another terminal — talk to it like any A2A HTTP server:
curl -s http://localhost:9000/.well-known/agent-card.json \
  -H 'a2a-version: 1.0' | jq '{name, skills: [.skills[].id]}'
# { "name": "Lambda Echo Agent", "skills": ["lambda-echo"] }

curl -s -X POST http://localhost:9000/message:send \
  -H 'a2a-version: 1.0' -H 'content-type: application/json' \
  -d '{"message":{"messageId":"m1","role":"ROLE_USER","parts":[{"text":"probe"}]}}' \
  | jq '.task | {state: .status.state, artifact: .artifacts[0].parts[0].text}'
# { "state": "TASK_STATE_COMPLETED", "artifact": "Hello from Lambda!" }
```

## Deploy

```bash
cargo lambda build --release -p lambda-agent
# Zip at target/lambda/lambda-agent/bootstrap.zip ready for `aws lambda create-function`.
```

Front this Lambda with API Gateway HTTP API (or an ALB/Lambda URL) — the
adapter handles both `APIGatewayProxyRequest` and `LambdaFunctionUrlRequest`
payloads.

## Streaming and push caveats

- **Streaming (`/message:stream`, `:subscribe`) is not supported in this adapter
  (ADR-008).** The Lambda request/response model has no long-lived connection;
  the server returns an `UnsupportedOperationError` for streaming surfaces.
- **Push delivery is external.** `.with_push_dispatch_enabled(true)` tells the
  storage to write the atomic `a2a_push_pending_dispatches` marker on task
  terminals. The request Lambda does NOT call the webhook — the two worker
  Lambdas do. See ADR-013 §4.4 for why `tokio::spawn` continuations are
  unsafe after the Lambda response returns.

## Storage

This example uses `InMemoryA2aStorage` to make `cargo check` / `cargo lambda
watch` work without AWS credentials. In a real deployment swap in the shared
backend (DynamoDB for production Lambda, or Postgres if you're behind a VPC).
ADR-009's same-backend requirement means all three Lambdas must point at the
same store.

The DynamoDB backend does **not** auto-create tables — see
`examples/lambda-infra` for CloudFormation, Terraform, and an `aws` CLI
script that provisions the five tables and enables the
`a2a_push_pending_dispatches` stream.

## See also

- `examples/lambda-durable-single` — ADR-018 durable executor continuation, single-Lambda simplest demo (in-memory + ReservedConcurrency=1).
- `examples/lambda-durable-agent` + `examples/lambda-durable-worker` — ADR-018 production-shape demo (two Lambdas + shared DynamoDB).
- `examples/lambda-infra` — reference IaC for the five DynamoDB tables.
- `examples/lambda-stream-worker` — DynamoDB Stream push-recovery worker.
- `examples/lambda-scheduled-worker` — EventBridge backstop.
