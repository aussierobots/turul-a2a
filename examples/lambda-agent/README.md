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
# Local invoke loop via cargo-lambda
cargo lambda watch -p lambda-agent

# In another terminal
cargo lambda invoke lambda-agent \
  --data-ascii '{"httpMethod":"GET","path":"/.well-known/agent-card.json"}'
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

- `examples/lambda-infra` — reference IaC for the five DynamoDB tables.
- `examples/lambda-stream-worker` — DynamoDB Stream push-recovery worker.
- `examples/lambda-scheduled-worker` — EventBridge backstop.
