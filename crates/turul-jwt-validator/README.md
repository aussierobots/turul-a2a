# turul-jwt-validator

Generic JWT validator with JWKS caching — local to the `turul-a2a`
workspace, with design sourced from `turul-mcp-oauth`.

Features:

- RS256 / ES256 / ES384 verification via `jsonwebtoken`.
- JWKS fetching with an in-memory cache, configurable TTL, and `kid`-miss
  refetch.
- Audience and issuer claim enforcement.
- Clock-skew tolerance and expiration checks.
- Extra-claims extraction via `serde_json::Value`.

This crate is consumed by
[`turul-a2a-auth`](https://crates.io/crates/turul-a2a-auth) for Bearer /
JWT middleware in A2A agents, but it has no A2A-specific dependencies and
can be used in any Rust project that needs a no-fuss JWT validator.

See the [workspace README](https://github.com/aussierobots/turul-a2a#readme)
for the full project overview.

## License

Licensed under either [MIT](../../LICENSE-MIT) or
[Apache 2.0](../../LICENSE-APACHE) at your option.
