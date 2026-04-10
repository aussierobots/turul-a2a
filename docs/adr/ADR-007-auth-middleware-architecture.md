# ADR-007: Auth Middleware Architecture

- **Status:** Proposed
- **Date:** 2026-04-11

## Context

turul-a2a v0.1 hardcodes `DEFAULT_OWNER = "anonymous"` in all handlers. The A2A proto defines `security_schemes` and `security_requirements` on `AgentCard`, and `security_requirements` on `AgentSkill`. GPS Trust deployments need JWT validation, but turul-a2a must remain usable by anyone without GPS Trust code coupling.

The turul-mcp-framework provides a reference pattern: generic middleware trait in the server crate, optional auth crate with JWT validator, middleware wired via builder.

## Decision

### 1. Transport-Level Auth via Tower Layer

Auth middleware runs as a Tower layer wrapping the axum Router. It intercepts raw HTTP requests BEFORE any handler or JSON-RPC dispatch. Auth failures produce HTTP 401/403 responses directly — never JSON-RPC error objects, never `A2aError` variants.

- `/.well-known/agent-card.json` is excluded from auth (public discovery).
- `/extendedAgentCard` requires authentication: gated on `ctx.identity.is_authenticated()`, not on claims presence. API key callers (no claims) are valid authenticated callers.
- `A2aError::Unauthenticated` is removed. If a request passes the Tower layer, it is authenticated (or no auth is configured). Handlers do not make auth decisions.

### 2. A2aMiddleware Trait and SecurityContribution

The `A2aMiddleware` trait lives in the `turul-a2a` server crate. Each middleware returns a `SecurityContribution` with both scheme definitions and requirement groups:

```rust
pub struct SecurityContribution {
    pub schemes: Vec<(String, SecurityScheme)>,
    pub requirements: Vec<SecurityRequirement>,
}
```

Middleware contributions are merged at `build()` time and auto-populated into the AgentCard.

**Merge rules:**
- `AnyOfMiddleware`: children's requirements become separate entries (OR semantics)
- Stacked middleware: Cartesian product of requirements (AND semantics), scopes unioned within same scheme
- Same scheme name + semantically equivalent definition: silent dedup
- Same scheme name + different definition: **configuration error at `build()` time** (not a warning)

Semantic equality compares normalized structured values (sorted/deduped scope lists, field-by-field), not serialized bytes.

### 3. Principal Extraction

Both Bearer and API key paths enforce non-empty owner identity symmetrically:
- Bearer: extracts configurable claim (default `sub`). Rejects empty, missing, or whitespace-only values.
- API key: `ApiKeyMiddleware` rejects `Some("")` from lookup. `None` = invalid key.

`AuthIdentity` carries owner and optional claims. `is_authenticated()` is an explicit method (returns `owner != "anonymous"`), not scattered string comparisons.

### 4. AnyOfMiddleware Failure Precedence

When all children fail: Internal short-circuits (fatal), then Forbidden(403) > HttpChallenge(401+WWW-Authenticate) > Unauthenticated(401). Ties broken by registration order. WWW-Authenticate headers merged from all HttpChallenge errors.

### 5. Local JWT Validator

`crates/turul-jwt-validator/` is a local workspace crate. Design sourced from `turul-mcp-oauth::jwt` (read-only reference). Supports RS256+ES256, JWKS caching, kid-miss refetch. No cross-repo dependencies or changes.

**Provenance note:** This crate's design originates from `turul-mcp-framework/crates/turul-mcp-oauth/src/jwt.rs`. Future extraction to a shared crate used by both MCP and A2A is a separate follow-up ADR.

### 6. v0.2 Auth Scope

- **Implemented:** Agent-level `security_schemes` and `security_requirements` from middleware. Bearer/JWT and API Key. Principal extraction with configurable claim.
- **Deferred:** Skill-level `security_requirements` (proto `AgentSkill.security_requirements`). mTLS. OIDC-specific discovery (uses generic JWT path).

## Consequences

- GPS Trust auth is configuration (JWKS URI + issuer + audience), not a code dependency.
- Any A2A consumer can implement `A2aMiddleware` for custom auth.
- Existing 202 tests pass unchanged (empty middleware stack = anonymous access).
- AgentCard always reflects actual auth configuration — scheme definitions match enforcement.
- No transport auth leaks into the protocol error model.
- The local JWT validator is an explicit fork boundary with documented provenance. Security patches must be applied here independently until a shared crate is extracted.
