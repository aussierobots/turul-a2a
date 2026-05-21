# ADR-020: Auth Middleware Contract Cleanup (deferred to 0.2.0)

- **Status:** Proposed
- **Date:** 2026-05-21

## Context

The 0.1.20 release closes two security defects in the auth middleware (Bearer scope enforcement, gRPC auth wire-string parity with HTTP) and ships non-breaking ergonomics (`RequestContext::from_headers`, `MiddlewareStack::empty`, dedup of bypass-path constants, `A2aMiddleware` documentation clarification).

A review of the middleware module surfaced five additional cleanup candidates that are **not** patch-release material because they change public API or deployment contracts:

1. `RequestContext::bearer_token: Option<String>` is a public field that duplicates `headers["Authorization"]`. The only consumer is `BearerMiddleware::before_request`; the field is extracted in two transport layers (HTTP and gRPC) and stuffed onto the struct, then read once. Two sources of truth for the same value.
2. `RequestContext::extensions: HashMap<String, serde_json::Value>` is a public field with **zero non-test consumers** across the workspace. The only writers are `AnyOfMiddleware`'s per-attempt clone (`stack.rs:67`, `:74`) and the bare-init paths in transport layers. `AnyOfMiddleware` clones the map for every child attempt, paying allocation cost for data nobody reads. Adopters who want to thread typed data through have to serde-roundtrip into `Value`; the standard Rust pattern (`http::Extensions` / TypeMap) is better.
3. `MiddlewareError::Internal(String)` drops the originating error chain. No `source()`, no boxed `dyn Error`. An internal middleware failure (JWKS fetch died, DB lookup panicked) becomes a stringified message; the wire surface collapses it to `"internal_error"` (correct per ADR-016), but the *logs* lose the chain too.
4. The `A2aMiddleware` trait name reads as generic but the contract is specifically auth-oriented (`before_request` only, failure surface shaped for 401/403 + `WWW-Authenticate`). The 0.1.20 documentation tightening makes this explicit in prose; the type name still suggests broader applicability.
5. The `compat-v03` Cargo feature gates the A2A-Version header requirement at compile time (`transport.rs`). A runtime `RuntimeConfig` switch would let one binary serve mixed-client deployments without recompilation.

Item 5 has additional context that came in during the 0.1.20 cycle: downstream consumers depend on `compat-v03` as a *feature flag* because (a) compile-time guarantees prevent runtime drift across deployed stages, and (b) the feature is load-bearing for Python `a2a-sdk` / Strands client interop. Promoting it to runtime config is a deployment-contract change that downstream consumers explicitly do not want.

## Downstream impact survey

Conducted across three first-party consumer repos for the 0.1.20 release planning:

- **sw-authoriser / sw-station-monitor / sw-device-monitor** — auth delegated to APIGW Lambda authoriser; `BearerMiddleware` is not mounted; `InboundCredentialMiddleware` implements `A2aMiddleware` for credential forwarding per ADR-0005 / ADR-0035.
- **sv-receiver-server** — uses `LambdaAuthorizerMiddleware`; implements custom `A2aMiddleware`; `MiddlewareError` used as opaque return type only.
- **Agentic RAG / KG (ASX A2A)** — depends on `turul-a2a = "0.1"` for executor/storage/client surfaces only; no middleware wiring.

Per-item impact:

| Item | Impacted repos | Acceptable to ship as breaking change? |
|---|---|---|
| #1 Drop `bearer_token` field | none | yes |
| #2 Drop `extensions` field | none | yes |
| #3 Reshape `Internal` variant | none | yes |
| #4 Rename `A2aMiddleware` | sw-station-monitor, sw-device-monitor, sv-receiver-server (all implement the trait) | **no — defer indefinitely** |
| #5 `compat-v03` feature → runtime config | sw-* and sv-receiver-server depend on the feature flag | **no — off the table** |

## Decision

### Included in 0.2.0

1. **Drop `RequestContext::bearer_token`.** Re-extract from `headers["Authorization"]` via the existing `extract_bearer_token` helper inside `BearerMiddleware::before_request`. Eliminates the two-sources-of-truth issue. `RequestContext::from_headers` (shipped in 0.1.20) becomes a four-field struct.
2. **Drop `RequestContext::extensions`.** Field has zero non-test consumers. Adopter middleware that needs typed cross-layer state can attach it via `Request::extensions_mut()` (`http::Extensions`, TypeMap-typed) in a sibling Tower layer.
3. **Reshape `MiddlewareError::Internal`.** Change variant from `Internal(String)` to `Internal(Box<dyn std::error::Error + Send + Sync>)`. Wire body remains `"internal_error"` per ADR-016 — the inner error is for logs / `source()` chains only, never on the wire.

### Explicitly deferred or off the table

4. **`A2aMiddleware` trait rename.** Deferred indefinitely. The trait is implemented by ADR-mandated patterns in downstream repos (`InboundCredentialMiddleware` for credential forwarding); a rename would break load-bearing infrastructure across multiple consumers. Revisit only if a concrete non-auth middleware adopter appears whose use case the current name actively misleads. The 0.1.20 documentation tightening on the trait and module satisfies the truth-in-naming concern for now.
5. **`compat-v03` feature → runtime config.** Off the table. Downstream deployment-contract dependency on the feature flag (compile-time guarantee, Python `a2a-sdk` / Strands interop) outweighs the runtime-flexibility benefit. The feature stays as a Cargo feature for the foreseeable future.

### Migration guidance for 0.2.0

For adopters that construct `RequestContext` directly via struct-literal:

```rust
// 0.1.x
let ctx = RequestContext {
    bearer_token: None,
    headers,
    identity: AuthIdentity::Anonymous,
    extensions: Default::default(),
};

// 0.2.0
let ctx = RequestContext::from_headers(headers);
```

For adopters that read `ctx.bearer_token` in custom middleware: re-extract via `extract_bearer_token(headers["Authorization"]…)` — or, preferably, accept the token via your own configured header name.

For adopters that read or write `ctx.extensions`: migrate to `Request::extensions_mut()` in a sibling Tower layer composed outside `A2aMiddleware`.

For adopters that construct `MiddlewareError::Internal(String)`: wrap the source error in `Box<dyn std::error::Error + Send + Sync>` — typically `Box::new(my_error)` or `e.into()`.

## Rejected alternatives

- **Ship the cleanup incrementally across 0.1.x patches.** Rejected because each removal is a SemVer-breaking change to the public wrapper contract; per the project versioning rule, breaking changes require a minor bump (0.x in 0.x.0 numbering).
- **Force the rename and the compat-v03 runtime switch as part of 0.2.0.** Rejected on the downstream impact survey — these would force coordinated migration across at least three first-party consumer repos for benefits that are aesthetic (rename) or net-negative (loss of compile-time deployment guarantee for compat-v03).
- **Add a second `RequestContext` constructor in 0.1.20 to preserve strict behavioral equivalence on bypass paths.** Considered during code review of `from_headers`. Rejected: `bearer_token` is only consumed by `BearerMiddleware` (which doesn't run on bypass paths or no-middleware short-circuits), `Debug` is redacted, and the 0.1.20 trait documentation actively redirects non-auth observation outside `A2aMiddleware`. Adding a constructor would be defensive coding for a hypothetical telemetry consumer the docs now route elsewhere.

## Triggers for shipping 0.2.0

Three middleware-field deletions do not earn the cost of a minor release on their own. Ship 0.2.0 when at least one of the following accumulates alongside:

- Another wrapper-type breaking change (e.g. `Task` field reshape, `Artifact` semantics tightening).
- An executor or storage trait contract change.
- A new `#[non_exhaustive]` enum variant that prior 0.1.x consumers should be forced to handle explicitly.

Until then 0.1.x continues with non-breaking adds and runtime alignment fixes.

## Follow-ups

- When 0.2.0 ships, re-run the downstream impact survey to confirm the rename and `compat-v03` decisions still hold.
- If a non-auth `A2aMiddleware` adopter appears between now and 0.2.0, escalate item #4 from "deferred indefinitely" to "scheduled for 0.2.0 or a later minor" with explicit migration tooling.
