# ADR-016: Stable Auth-Failure Wire Surface

- **Status**: Accepted
- **Date**: 2026-04-24
- **Supersedes / Amends**: complements ADR-004 (A2A protocol error model) and
  ADR-007 (auth middleware). Transport auth failures remain outside the
  `A2aError` / `google.rpc.ErrorInfo` model per ADR-007 — this ADR tightens
  the already-transport-only surface, it does not change which layer owns it.

## 1. Context

The transport auth path sits in `crates/turul-a2a/src/middleware/layer.rs`
and emits HTTP 401/403 directly when middleware rejects a request. This
path is intentionally **outside** the A2A error model (ADR-004): the
module-level comment and the body-builder comment both state this
explicitly. Three concrete issues exist today in 0.1.11:

### 1.1 JSON body is Debug-formatted internal enum text

`layer.rs:131-157`, `middleware_error_to_response`:

```rust
let body = serde_json::json!({
    "error": {
        "code": err.http_status(),
        "message": format!("{err:?}"),
    }
});
```

- `{err:?}` is `Debug` of `MiddlewareError`. Variant text (including
  inner strings like `Unauthenticated("Missing X-API-Key header")`)
  leaks into the wire.
- The output shape is coupled to internal enum names. Renaming a
  variant silently changes the public 401 body.
- Not a stable contract for adopters pattern-matching on response
  bodies.

### 1.2 Bearer WWW-Authenticate leaks validator internals

`crates/turul-a2a-auth/src/bearer.rs:58-63`:

```rust
.map_err(|e| MiddlewareError::HttpChallenge {
    status: 401,
    www_authenticate: format!(
        "Bearer realm=\"a2a\", error=\"invalid_token\", error_description=\"{e}\""
    ),
})?;
```

`e` is `turul_jwt_validator::JwtValidationError`. Its Display output:

- Can include internal JWKS endpoint URLs (`JwksFetchError(String)`).
- Can echo token header fragments (`DecodingError(String)`,
  `InvalidToken(String)`).
- Changes when the validator's internal error wording changes — so it
  is a non-stable public-surface contract.
- Is interpolated into `error_description="{e}"` with no escaping:
  RFC 6750 §3 restricts `error_description` characters to
  `%x20-21 / %x23-5B / %x5D-7E` (excludes `"` and `\`). If an error
  string ever contains either character, the emitted header is
  malformed.

### 1.3 Debug on credential-carrying types

`crates/turul-a2a/src/middleware/context.rs:43-49` — `RequestContext`
derives `Debug` and carries:

- `bearer_token: Option<String>` — the raw JWT.
- `headers: http::HeaderMap` — every request header including
  `Authorization`, `X-API-Key`, `Cookie`, and any adopter-defined
  credential header.
- `identity: AuthIdentity` — derives `Debug`, `Authenticated` variant
  carries `claims: Option<serde_json::Value>` (PII-class content).
- `extensions: HashMap<String, serde_json::Value>` — adopter-writable
  bag of arbitrary JSON.

The framework itself does not print `RequestContext` via tracing
today. But the Debug impl exists and is trivially invoked by anyone
writing an axum handler that logs request extensions, or by any future
framework code added without this invariant in mind. The fields are
named in a way that makes the leak trivially observable (the literal
name `bearer_token` sits next to `headers`).

## 2. Decision

Define a stable transport auth-failure wire surface, scheme-aware, and
redact `Debug` on the credential-carrying types.

### 2.1 Internal failure taxonomy — coarse by design

Introduce `AuthFailureKind` in `turul-a2a` (no new crate; it's
transport-internal plus adopter-observable metadata):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthFailureKind {
    /// No credential present where one was required
    /// (missing `Authorization` or configured API-key header).
    MissingCredential,
    /// Any Bearer token validation failure, regardless of inner
    /// reason (expired / wrong audience / wrong issuer / algorithm
    /// not in allowlist / JWKS unreachable / key not found / malformed).
    /// RFC 6750 §3.1 explicitly conflates these as `invalid_token`.
    InvalidToken,
    /// API-key lookup rejected the credential
    /// (unknown key or adopter-supplied lookup returned `None`).
    InvalidApiKey,
    /// Authenticated principal resolved to an empty/whitespace string.
    EmptyPrincipal,
    /// Reserved for future use. Not produced by any current code path
    /// — `BearerMiddleware::required_scopes` exists as a field but
    /// is not evaluated in `before_request`. Kept in the taxonomy so
    /// the wire mapping is stable when scope enforcement lands.
    InsufficientScope,
}
```

Five variants, each verified to map to (or intentionally reserve for)
an actual code path in 0.1.11. Deliberately coarse because:

- The richer discriminants (expired vs. wrong-audience vs. unreachable
  JWKS) exist only inside `turul_jwt_validator::JwtValidationError`,
  which is an external crate's public API. Framework code cannot
  freeze a wire taxonomy keyed on variant names it does not own.
- The current transport boundary (`bearer.rs:54-63`) collapses every
  validator error into a single `MiddlewareError::HttpChallenge` —
  the discriminant is already erased before the wire-mapping layer
  sees it. Preserving the discriminant would require a behavioural
  refactor of `MiddlewareError` and `BearerMiddleware` that is out of
  scope for a wire-tightening release.
- RFC 6750 §3.1 itself conflates all token-validation failures under
  `invalid_token` ("The access token provided is expired, revoked,
  malformed, or invalid for other reasons"). Emitting richer
  distinctions would leak framework internals without aligning with
  the spec the Bearer header already tracks.

Richer per-reason observability is addressable as a separate concern
(see §2.6 — adopter-scoped hook, not public wire).

Each variant maps to a stable wire-kind string (`snake_case`):
`missing_credential`, `invalid_token`, `invalid_api_key`,
`empty_principal`, `insufficient_scope`.

### 2.2 JSON body — all schemes, all failures

```json
{ "error": "<kind_wire_string>" }
```

The body is scheme-agnostic and stable across minor releases. Callers
parsing the body pattern-match on `error` string values from the fixed
taxonomy.

No `code` field (HTTP status is on the response status line). No
`message` field (stable kind string is the adopter-observable label;
human-readable text belongs in adopter UI, not in wire).

### 2.3 WWW-Authenticate — Bearer challenges only

RFC 6750 governs **Bearer** WWW-Authenticate syntax. API-key auth has
no canonical challenge scheme and MUST NOT emit a synthetic Bearer
header.

**Bearer failures** emit:

```
WWW-Authenticate: Bearer realm="a2a", error="<rfc6750-code>"
```

With the RFC 6750 §3 mapping:

| `AuthFailureKind` | `error=` value |
|---|---|
| `MissingCredential` (when Bearer expected) | `invalid_request` |
| `InvalidToken` | `invalid_token` |
| `EmptyPrincipal` | `invalid_token` |
| `InsufficientScope` | `insufficient_scope` |

`InvalidApiKey` never reaches a Bearer challenge path (§2.3
scheme-separation); no Bearer mapping applies.

`error_description` is **intentionally omitted** to prevent leaking
validator internals. Adopters that need richer failure detail must add
their own instrumentation (see §2.6).

**API-key failures** emit: JSON body only. No `WWW-Authenticate`.

**All emitted header values** are constrained to the RFC 6750 §3
charset by construction — since the strings come from a closed
enumeration, they cannot contain `"` or `\`.

### 2.4 Redact Debug on credential-carrying types

- `RequestContext::Debug` — manual impl:
  - `bearer_token`: `Some(<redacted>)` when present, `None` otherwise.
  - `headers`: **default-deny.** Header names are always printed.
    Header values are printed only when the header name is in a fixed
    allowlist of known-safe headers: `Content-Type`, `Content-Length`,
    `Accept`, `User-Agent`, `Host`. Every other header value is
    redacted to `<redacted>` — including `Authorization`, `Cookie`,
    and any adopter-defined credential header
    (`X-API-Key`, custom names). `RequestContext` has no knowledge of
    which header name an adopter configured on `ApiKeyMiddleware`; the
    default-deny posture covers the custom case without the formatter
    needing semantic awareness of the middleware configuration.
  - `identity`: delegates to `AuthIdentity::Debug`.
  - `extensions`: print map keys only; values redacted.
- `AuthIdentity::Debug` — manual impl:
  - `Anonymous` — prints as `Anonymous`.
  - `Authenticated { owner, claims }` — prints `owner` (non-sensitive
    principal id); `claims` redacted to `<redacted>`.
- `MiddlewareError::Debug` — no longer consumed on the wire (§2.2);
  keep derived Debug for internal tracing utility.

A type-level guard against accidental `#[derive(Debug)]` on
`ApiKeyMiddleware`, `BearerMiddleware`, and `StaticApiKeyLookup` is
specified in §4 test #5.

### 2.5 First-party secret-aware lookup helper

Add a first-party `ApiKeyLookup` implementation in `turul-a2a-auth`
(name and internals deliberately left to implementation) that
satisfies these invariants:

- Implements `ApiKeyLookup`.
- Its `Debug` impl never emits any key material — at most something
  like `XxxApiKeyLookup { len: <N> }`.
- Lookup compares the caller-supplied key against stored credentials
  without surfacing either as a bare `String` in intermediate values
  where a `Debug` or panic could stringify them.
- The container / key-wrapper type is an implementation choice during
  the 0.1.12 work. Candidates to evaluate at implementation time:
  - `HashMap<String, String>` with a custom redacted `Debug` impl on
    the enclosing struct (simplest; keys remain in plain `String`
    internally but are never printable via the public Debug surface).
  - A newtype around `String` that owns its own `Debug`/`Hash`/`Eq`
    impls (more invariants at the type level; more code).
  - A `secrecy`-based container, subject to verifying the chosen
    release's trait surface actually supports map-key use
    (`secrecy = "0.10"`'s `SecretString` intentionally does not
    implement `Hash` / `Eq` — direct use as a `HashMap` key does
    not compile without a newtype).

The ADR does not normatively prescribe any of these shapes. The
stable requirement is: a first-party secret-aware helper with the
Debug / storage / lookup properties above. The chosen shape is
documented in the implementation PR and (if non-obvious) noted in the
crate's README.

### 2.6 Observability hook (deferred)

Fix #6 from the revised fix list — an opt-in observability hook
(counter or tower layer adopters attach) for `AuthFailureKind` events
— is **out of scope** for this ADR. It is a separate decision about
adopter instrumentation shape. This ADR establishes the kind
taxonomy that such a hook would emit against; the hook itself can
land in a follow-up without re-opening this one.

### 2.7 Scope of non-goals

- Rate limiting, DDoS protection, retry-budget enforcement: explicitly
  not framework concerns. Belongs in LB / WAF / ingress.
- Private-network ingress: deployment-layer concern.
- Rate-limited logging of auth failures: intentionally not in this
  ADR. The framework stays quiet-by-default; adopters add their own
  instrumentation (see §2.6 future hook).
- `google.rpc.ErrorInfo` on auth failures: transport path is outside
  the A2A error model per ADR-007.

## 3. `ApiKeyLookup` trait documentation

Rustdoc on the trait gains an explicit clause:

> Implementors MUST ensure their lookup's storage does not leak keys
> via `Debug`, `Display`, `Serialize`, or any other conventional
> trait. A first-party reference implementation satisfying this
> contract ships in `turul-a2a-auth` (see ADR-016 §2.5); the exact
> type name and internals are fixed by the implementation PR, not by
> this ADR.

This cannot be enforced at the type level (negative trait assertions
on adopter impls are not expressible in Rust); it is a documented
contract.

## 4. Test obligations

1. **Body mapping**: unit test asserting every `AuthFailureKind`
   variant serializes to its documented wire string
   (`missing_credential`, `invalid_token`, `invalid_api_key`,
   `empty_principal`, `insufficient_scope`).
2. **WWW-Authenticate mapping**: unit test asserting each variant's
   RFC 6750 code when emitted through a Bearer challenge, or header
   absence when emitted through an API-key rejection. Assert no
   `error_description` component is present on any Bearer challenge.
   Assert that every validator error variant the framework exercises
   (expired / wrong audience / wrong issuer / unsupported alg /
   JWKS fetch / key not found / malformed) collapses to
   `InvalidToken` and emits `error="invalid_token"` — i.e. test the
   coarse-mapping contract rather than presuming we can distinguish
   them on the wire.
3. **RequestContext::Debug redaction**: unit test creating a
   `RequestContext` with `bearer_token = Some("eyJ…")`,
   `Authorization: Bearer eyJ…` header, `X-API-Key: abc` header,
   `Content-Type: application/json` header, and `extensions` carrying
   a secret-shaped value. Assert `{:?}` output redacts bearer_token,
   sensitive header values, and extensions values; allows
   `Content-Type: application/json` through; prints header names
   throughout.
4. **AuthIdentity::Debug redaction**: unit test asserting
   `Authenticated { owner, claims }` Debug shows `owner` but redacts
   `claims`.
5. **Type-level guard**: compile-fail or `static_assertions::assert_not_impl_all!`
   for `ApiKeyMiddleware`, `BearerMiddleware`, `StaticApiKeyLookup` not
   implementing `Debug`.
6. **End-to-end Bearer auth failure**: 401 with body
   `{"error": "invalid_token"}` and header
   `WWW-Authenticate: Bearer realm="a2a", error="invalid_token"`
   (no `error_description`, no validator internals).
7. **End-to-end API-key auth failure**: 401 with body
   `{"error": "invalid_api_key"}` and no `WWW-Authenticate` header.
8. **Charset safety**: property test that emitted `WWW-Authenticate`
   values never contain `"` or `\` (RFC 6750 §3 compliance).

## 5. Consequences

### 5.1 Migration impact

- The JSON body shape `{"error": {"code", "message": "<Debug>"}}`
  becomes `{"error": "<kind_string>"}`. Adopters pattern-matching on
  the old `message` text will break. That text was `format!("{err:?}")`
  of an internal enum and was never documented as stable. Migration
  is a grep-and-update.
- `WWW-Authenticate: Bearer realm="a2a", error="invalid_token", error_description="..."`
  becomes `WWW-Authenticate: Bearer realm="a2a", error="invalid_token"`.
  Adopters logging `error_description` for debugging lose it. That
  content was a leak, not a debugging channel.
- `Debug` output of `RequestContext` and `AuthIdentity` becomes
  redacted. Adopters using `{:?}` to debug auth state will see less.
  `tracing::debug!` call sites that were implicitly relying on
  token/claim visibility need to read `ctx.bearer_token`,
  `ctx.identity.owner()`, etc. explicitly.

### 5.2 Wire invariants after this ADR

- `AuthFailureKind` → body string: stable across minor releases.
- RFC 6750 mapping for Bearer: stable across minor releases.
- `RequestContext::Debug` never prints bearer tokens, sensitive
  header values, or extensions values.
- `AuthIdentity::Debug` never prints claims.

### 5.3 Release target

Ship as **0.1.12**. Technically the wire-format tightening changes
text adopters theoretically might parse, but the previous output was
`format!("{:?}")` of an internal enum — not a stable contract.
Behaviorally the 401 status, `Content-Type`, and scheme
challenge are unchanged. Treating this as a minor (patch in this
workspace's vocab per CLAUDE.md) bump is defensible and consistent
with the "no stable adopter parsed the old body" position.

A CHANGELOG entry explicitly enumerates the wire-format tightenings so
any affected adopter sees them.

## 6. Alternatives considered

1. **Keep current behavior**. Rejected — info leaks in both body and
   `WWW-Authenticate`, non-stable public surface, latent Debug leaks.
2. **Apply RFC 6750 vocabulary to all schemes uniformly**. Rejected
   per reviewer — `ApiKeyMiddleware` has no challenge header and
   forcing Bearer-scheme vocabulary onto a non-Bearer 401 is a
   category error. §2.3 scheme-maps correctly instead.
3. **Redact only `RequestContext.bearer_token`, leave `HeaderMap`
   Debug as-is**. Rejected per reviewer — `Authorization`,
   `X-API-Key`, `Cookie`, and adopter-defined headers leak through
   `HeaderMap::Debug`. The bigger vector is the headers field, not
   the named token field.
4. **Use `A2aError` / `google.rpc.ErrorInfo` for auth failures**.
   Rejected — transport-level by design (ADR-007). Auth runs before
   the protocol layer; `ErrorInfo` is for protocol errors.
5. **Sanitize `error_description` rather than omit it**. Rejected —
   sanitizing keeps the leak surface open (deciding what's safe to
   surface is error-prone) without gaining any stable adopter
   contract. Omitting is safer and no one documented the field as
   stable.
6. **Enforce negative-Debug on `ApiKeyLookup` implementors via
   compile-fail test**. Rejected per reviewer — cannot be enforced
   on foreign types. §3 documents the expectation instead.
7. **Fine-grained Bearer kind taxonomy (separate `ExpiredToken`,
   `InvalidAudience`, `InvalidIssuer`, `UnsupportedAlgorithm` etc.
   variants)**. Rejected per reviewer — (a) the richer discriminant
   lives inside `turul_jwt_validator::JwtValidationError`, which is
   an external public API; freezing wire-taxonomy on its variant
   names binds us to upstream stability we don't own, (b) the current
   transport boundary in `bearer.rs:54-63` already collapses every
   validator error into a single `MiddlewareError::HttpChallenge`, so
   preserving the discriminant requires a behavioural refactor out of
   scope for this wire-tightening ADR, (c) RFC 6750 §3.1 itself
   conflates these cases under `invalid_token`. The coarse taxonomy
   in §2.1 is the verifiable contract.
8. **Prescribe `HashMap<SecretString, String>` as the
   first-party lookup storage shape**. Rejected per reviewer —
   `secrecy = "0.10"`'s `SecretString` does not implement `Hash` or
   `Eq` (deliberately), so the prescribed shape does not compile
   without a newtype. More broadly, the ADR should state the
   invariants (redacted Debug, no key disclosure) and leave container
   choice to implementation. §2.5 does this.

## 7. Open questions

- **Allowlist of non-sensitive headers for Debug value emission**:
  the §2.4 proposed list is
  `Content-Type / Content-Length / Accept / User-Agent / Host`. Is
  that list complete? Should it be extensible by adopters? Proposal:
  ship a fixed list in 0.1.12, make it extensible in a later release
  if adopter feedback warrants.
- **Kind taxonomy completeness**: implementation review must verify
  every current `MiddlewareError`-producing path maps cleanly onto
  one of the five §2.1 variants. If any falls through without a
  natural kind, that is a signal to either (a) add a variant with a
  concrete code path (not a speculative one), or (b) fold it into an
  existing variant.
- **Richer per-reason observability**: if adopters demand being able
  to distinguish "expired token" from "wrong audience" at the 401
  boundary, the correct shape is the §2.6 observability hook (where
  the richer discriminant is adopter-scoped and doesn't freeze public
  wire), not widening the wire taxonomy. This ADR deliberately keeps
  wire coarse and signals the refactor path for richer internal
  discriminants in a follow-up.

## 8. References

- [RFC 6750](https://datatracker.ietf.org/doc/html/rfc6750) §3 — "The
  WWW-Authenticate Response Header Field".
- ADR-004 — Error model (A2A protocol error path; not applicable here).
- ADR-007 — Auth middleware; establishes transport-level auth path.
- `crates/turul-a2a/src/middleware/layer.rs`
- `crates/turul-a2a/src/middleware/context.rs`
- `crates/turul-a2a/src/middleware/error.rs`
- `crates/turul-a2a-auth/src/bearer.rs`
- `crates/turul-a2a-auth/src/api_key.rs`
