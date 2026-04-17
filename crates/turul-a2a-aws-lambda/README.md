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

See
[ADR-008](https://github.com/aussierobots/turul-a2a/blob/main/docs/adr/ADR-008-aws-lambda-adapter.md)
for the adapter design and constraints.

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
