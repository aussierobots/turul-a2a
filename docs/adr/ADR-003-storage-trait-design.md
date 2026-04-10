# ADR-003: Storage Trait Design with Tenant and Owner Scoping

- **Status:** Accepted
- **Date:** 2026-04-10

## Context

A2A tasks need persistence with multi-tenancy (proto `tenant` field on most requests) and owner isolation (from auth `sub` claim). The proto `Task` message has no tenant or owner fields -- these are routing/auth concerns, not part of the task data model. The turul-mcp-framework uses a similar pattern with `session_id` binding.

## Decision

Storage traits (`A2aTaskStorage`, `A2aPushNotificationStorage`) take `tenant: &str` and `owner: &str` as explicit parameters on every method, not embedded in the Task wrapper. The in-memory backend stores `(tenant, task_id)` as the map key with owner as metadata. `get_task` returns `None` (not error) for wrong tenant/owner -- the server maps this to 404, preventing information leakage.

State machine transitions are enforced at the storage layer by delegating to `turul_a2a_types::state_machine::validate_transition`. Parity tests verify identical behavior across backends. Feature-gated backends (in-memory default, sqlite/postgres/dynamodb deferred).

**Exception:** Push notification config storage uses raw `turul_a2a_proto::TaskPushNotificationConfig` directly -- intentional exception documented in CLAUDE.md. Push configs are simple CRUD with no state machine or invariants warranting a wrapper.

## Consequences

- **Handler code never touches storage without providing tenant+owner.** Multi-tenant isolation is enforced at the persistence layer, not at the handler level.
- **Adding backends is mechanical.** Implement the traits, run parity tests. The trait surface is small enough that a new backend is a single file.
- **Wrong-tenant/owner reads return `None`, not an error.** The server layer maps this uniformly to 404, eliminating information leakage about task existence across tenant boundaries.
- **State machine enforcement lives in storage, not handlers.** Invalid transitions are rejected regardless of which handler or code path triggers them.
- **The push config exception keeps raw proto types isolated to a narrow boundary.** No wrapper overhead for a type with no invariants to enforce.
