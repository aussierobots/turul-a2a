# ADR-003: Storage Trait Design with Tenant and Owner Scoping

- **Status:** Accepted (amended 2026-10-22: persisted state names, ListTasks ordering and pagination)
- **Date:** 2026-04-10

## Context

A2A tasks need persistence with multi-tenancy (proto `tenant` field on most requests) and owner isolation (from auth `sub` claim). The proto `Task` message has no tenant or owner fields -- these are routing/auth concerns, not part of the task data model. The turul-mcp-framework uses a similar pattern with `session_id` binding.

## Decision

Storage traits (`A2aTaskStorage`, `A2aPushNotificationStorage`) take `tenant: &str` and `owner: &str` as explicit parameters on every method, not embedded in the Task wrapper. The in-memory backend stores `(tenant, task_id)` as the map key with owner as metadata. `get_task` returns `None` (not error) for wrong tenant/owner -- the server maps this to 404, preventing information leakage.

State machine transitions are enforced at the storage layer by delegating to `turul_a2a_types::state_machine::validate_transition`. Parity tests verify identical behavior across backends. Feature-gated backends (in-memory default, sqlite/postgres/dynamodb deferred).

**Persisted task state.** Every backend persists the task state as its proto wire name (`TASK_STATE_WORKING`, ...). The single owner of that vocabulary is `turul_a2a_types::TaskState::wire_name` / `TaskState::from_wire_name` (derived from the generated proto enum names); the terminal set is `turul_a2a_types::state_machine::TERMINAL_STATES`, and the SQL terminal-write CAS (`status_state NOT IN (...)`) and the DynamoDB `ConditionExpression` values are built from it. No backend formats or parses state names itself. Stores written before this rule (Rust `Debug` names such as `Completed`) are not migrated or read: operators re-create persistent stores.

**ListTasks ordering and pagination (A2A v1.0.1 §3.1.4).** Tasks are listed by status timestamp, most recent first, with `task_id` descending as the tie-break. The status timestamp is `Task.status.timestamp`: every write that carries a status without one is stamped at commit with the commit time (microsecond precision), and status-update events committed in the same atomic write receive the same timestamp. The single owner is `storage::status_time` (`stamp_status`, `sort_key`, `encode_cursor`/`decode_cursor`); every backend calls it. Artifact and history writes keep the status, so they never move a task in the listing. The page token is `<sort key>|<task_id>` of the previous page's last task; a token in any other form is an invalid request. The final page MUST carry an empty `nextPageToken`, including when the total is an exact multiple of the page size (SQL backends fetch `page_size + 1` rows). `statusTimestampAfter` keeps tasks whose status timestamp is greater than or equal to the bound, on every backend and every transport. `pageSize` is 1..=100 with a default of 50 (`TaskFilter::resolved_page_size`); a value outside that range is rejected as an invalid request, not clamped. SQL backends persist the sort key in a `status_timestamp` column (fixed-width UTC text, so lexical and chronological order agree; `COLLATE "C"` on PostgreSQL). The in-memory and DynamoDB backends derive it from the stored task, so DynamoDB needs no new attribute or index. `updated_at` remains the last-write time used only for retention (ADR-026).

**Exception:** Push notification config storage uses raw `turul_a2a_proto::TaskPushNotificationConfig` directly -- intentional exception documented in CLAUDE.md. Push configs are simple CRUD with no state machine or invariants warranting a wrapper.

## Consequences

- **Handler code never touches storage without providing tenant+owner.** Multi-tenant isolation is enforced at the persistence layer, not at the handler level.
- **Adding backends is mechanical.** Implement the traits, run parity tests. The trait surface is small enough that a new backend is a single file.
- **Wrong-tenant/owner reads return `None`, not an error.** The server layer maps this uniformly to 404, eliminating information leakage about task existence across tenant boundaries.
- **State machine enforcement lives in storage, not handlers.** Invalid transitions are rejected regardless of which handler or code path triggers them.
- **The push config exception keeps raw proto types isolated to a narrow boundary.** No wrapper overhead for a type with no invariants to enforce.
- **One state vocabulary on disk and on the wire.** The persisted state and the CAS predicates use the same names as the JSON wire, from one owner; parity tests read the raw column/attribute to pin it.
- **Listing order follows the spec's status timestamp, not write activity.** A task streaming artifacts keeps its place; a status change moves it to the top.

## Revision log

- 2026-10-22 — Persisted task state is the proto wire name owned by `TaskState::wire_name` (no migration; persistent stores are re-created). ListTasks orders by status timestamp DESC with a `<status timestamp>|<task_id>` cursor, honours `statusTimestampAfter`, returns an empty final-page token on exact multiples, and rejects `pageSize` outside 1..=100 instead of clamping. Status writes stamp `TaskStatus.timestamp` through one owner.
