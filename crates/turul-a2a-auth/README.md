# turul-a2a-auth

Authentication middleware for the `turul-a2a` server framework.

Provides two Tower-layer middlewares:

- **`BearerMiddleware`** — JWT verification with JWKS caching, backed by
  [`turul-jwt-validator`](https://crates.io/crates/turul-jwt-validator).
  Injects claims into the request's `AuthIdentity`.
- **`ApiKeyMiddleware`** — Static API key enforcement on a configurable
  header (default `X-API-Key`).

Both middlewares integrate with the `AuthIdentity` enum surfaced to
executors via `ExecutionContext`, and with the `SecurityContribution`
pattern for agent card security declarations. See
[ADR-007](https://github.com/aussierobots/turul-a2a/blob/main/docs/adr/ADR-007-auth-middleware.md).

See the [workspace README](https://github.com/aussierobots/turul-a2a#readme)
for the full project overview.

## License

Licensed under either [MIT](../../LICENSE-MIT) or
[Apache 2.0](../../LICENSE-APACHE) at your option.
