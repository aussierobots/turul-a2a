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

See the [workspace README](https://github.com/aussierobots/turul-a2a#readme)
for the full project overview.

## License

Licensed under either [MIT](../../LICENSE-MIT) or
[Apache 2.0](../../LICENSE-APACHE) at your option.
