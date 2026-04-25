# turul-a2a-auth

Authentication middleware for the `turul-a2a` server framework.

Two ready-made middlewares implementing the `A2aMiddleware` trait:

- **`BearerMiddleware`** — JWT verification with JWKS caching, backed by
  [`turul-jwt-validator`](https://crates.io/crates/turul-jwt-validator).
- **`ApiKeyMiddleware`** — Static API key enforcement on a configurable
  header (default `X-API-Key`).

Both populate `RequestContext.identity` with `AuthIdentity::Authenticated`,
which the framework surfaces to executors via `ExecutionContext`. Both
also publish a `SecurityContribution` so the framework merges scheme
declarations into the agent card automatically.

## API key

```rust
use std::collections::HashMap;
use std::sync::Arc;
use turul_a2a::A2aServer;
use turul_a2a_auth::{ApiKeyMiddleware, RedactedApiKeyLookup};

let mut keys = HashMap::new();
keys.insert("ak_live_abc123".into(), "alice".into());
keys.insert("ak_live_def456".into(), "bob".into());

let lookup = Arc::new(RedactedApiKeyLookup::new(keys));
let auth = ApiKeyMiddleware::new(lookup, "X-API-Key");

A2aServer::builder()
    .executor(MyAgent)
    .middleware(Arc::new(auth))
    .build()?;
```

`RedactedApiKeyLookup` is a reference implementation whose `Debug`
output never contains key material — important if you ever log the
middleware. Plug in your own `ApiKeyLookup` for database-backed
resolution.

```rust
use async_trait::async_trait;
use turul_a2a_auth::ApiKeyLookup;

struct DbKeyLookup { /* ... */ }

#[async_trait]
impl ApiKeyLookup for DbKeyLookup {
    async fn lookup(&self, key: &str) -> Option<String> {
        // Resolve key → owner string, or None if invalid.
        // None triggers a 401 with no information leak about which key was tried.
        unimplemented!()
    }
}
```

## Bearer JWT

```rust
use std::sync::Arc;
use turul_a2a::A2aServer;
use turul_a2a_auth::BearerMiddleware;
use turul_jwt_validator::JwtValidator;

let validator = Arc::new(
    JwtValidator::new(/* issuer, JWKS URL, audience, … */)
);

let auth = BearerMiddleware::new(validator)
    .with_principal_claim("sub")              // default
    .with_required_scopes(vec!["a2a.read".into()]);

A2aServer::builder()
    .executor(MyAgent)
    .middleware(Arc::new(auth))
    .build()?;
```

JWKS is fetched and cached by `JwtValidator`. The middleware extracts
the principal from the configured claim (default `sub`) and rejects
empty/whitespace values.

Validator failures collapse to `InvalidToken` deliberately — the
underlying error never reaches the response, to avoid leaking JWKS
URLs or token fragments via `error_description`.

## Reading identity in your executor

```rust
use turul_a2a::prelude::*;
use turul_a2a::middleware::AuthIdentity;

#[async_trait::async_trait]
impl AgentExecutor for MyAgent {
    async fn execute(&self, task: &mut Task, message: &Message, ctx: &ExecutionContext)
        -> Result<(), A2aError>
    {
        let owner = match &ctx.identity {
            AuthIdentity::Authenticated { owner, .. } => owner.as_str(),
            AuthIdentity::Anonymous => "anonymous",
        };
        task.push_text_artifact("ok", "Reply", format!("hi {owner}"));
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("My Agent", "1.0.0").build().unwrap()
    }
}
```

For Bearer, the full claims set is also available as
`AuthIdentity::Authenticated { claims, .. }` (a `serde_json::Value`).

## Security declarations on the agent card

Each middleware publishes a `SecurityContribution` so the framework's
agent card auto-includes the matching `SecurityScheme` and any
required scopes. Adopters do not need to hand-write
`security_schemes` on the card; the framework merges contributions at
build time.

## Failure modes

| Cause | HTTP status | Auth header behaviour |
|---|---|---|
| No `Authorization` / API key header | 401 | `WWW-Authenticate` challenge |
| Invalid token / unknown key | 401 | `error="invalid_token"` (Bearer) or no detail (API key) |
| Empty principal claim | 401 | `error="invalid_token"` |
| Required scope missing (Bearer) | 403 | `error="insufficient_scope"` |

Errors are emitted at the transport layer (outside `A2aError`) so they
never leak through the JSON-RPC envelope.

## Examples

- `examples/auth-agent` — runnable agent with API key middleware.
- `examples/skill-security-agent` — declares per-skill security requirements on the card.

See the [workspace README](../../README.md) for the project overview
and crate map.

## License

Licensed under either [MIT](../../LICENSE-MIT) or
[Apache 2.0](../../LICENSE-APACHE) at your option.
