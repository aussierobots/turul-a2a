# ADR-019: Least-Privilege Storage Bootstrap (`assume_schema_initialized`)

- **Status:** Accepted
- **Date:** 2026-05-18

## Context

`PostgresA2aStorage::new()` and `SqliteA2aStorage::new()` unconditionally call `create_tables()` on every process start (`crates/turul-a2a/src/storage/postgres.rs:43-56`, `crates/turul-a2a/src/storage/sqlite.rs:42-56`). `create_tables()` runs `CREATE TABLE IF NOT EXISTS` × 5 plus `ALTER TABLE … ADD COLUMN IF NOT EXISTS` × 3 (Postgres) on every boot.

Two consequences:

1. **Privilege over-grant on Postgres.** The runtime role must own (or have `CREATE` on) the target schema and the `a2a_*` tables, even though steady-state operation only needs item-level `INSERT/UPDATE/DELETE/SELECT`. In managed multi-tenant clusters (RDS, Cloud SQL, shared self-hosted Postgres) this means the agent process can create arbitrary tables in its schema if the credential is compromised. An adopter reported this concretely: granting `CREATE ON SCHEMA public` to the runtime role solely so `PostgresA2aStorage::new()` succeeds.
2. **Silent migration on upgrade.** The `let _ = sqlx::query("ALTER TABLE … ADD COLUMN IF NOT EXISTS …")` calls swallow `permission denied`. With a least-privilege role today the schema migration becomes a no-op without any signal.

DynamoDB does not have this issue: `DynamoDbA2aStorage::new()` (`crates/turul-a2a/src/storage/dynamodb.rs:82-90`) is a no-op for DDL — tables are provisioned out-of-band (Terraform / CDK / CloudFormation), and the runtime IAM principal needs only item-level grants. The `create_table()` call sites in `dynamodb.rs` are inside `#[cfg(test)] mod tests` (line 2828+) and are test fixtures, not production code.

InMemory is N/A (process-local).

## Decision

Add an opt-in field `assume_schema_initialized: bool` (default `false`) to `PostgresConfig` and `SqliteConfig`. When `true`, `new()` skips `create_tables()` entirely — no `CREATE TABLE`, no `ALTER TABLE`, no `CREATE INDEX`. Default-false preserves existing behavior bit-for-bit; no adopter sees a change unless they opt in.

When `assume_schema_initialized: true`:

- **Operator owns provisioning.** Tables must exist with the schema shipped by the matching `turul-a2a` release **before** the process starts. The canonical schema is `create_tables()` in the respective backend module. (A separate, checked-in migration directory is a follow-up — see "Follow-ups" below.)
- **Operator owns forward migrations.** The runtime no longer silently `ALTER TABLE`s on upgrade. Adopters running with this flag MUST review the CHANGELOG of every release that bumps `turul-a2a-types` or storage internals, and apply any schema deltas manually with a higher-privilege role before rolling the new binary.
- **Runtime role can be narrowed.** Postgres: `INSERT, UPDATE, DELETE, SELECT` on the six `a2a_*` tables; no `CREATE`, `ALTER`, or `DROP` on the schema. SQLite: filesystem read+write on the database file; the file can be owned by a different user that performs the bootstrap.

SQLite is included for symmetry, not for the privilege case (SQLite's authorization model is filesystem-based, not role-based). The flag is useful there for read-replica-style setups and to keep config shapes uniform across backends.

DynamoDB requires no change. The least-privilege story is already correct by construction — `new()` does no DDL, and IAM policies for the runtime principal can omit `dynamodb:CreateTable` / `UpdateTable` / `DeleteTable`. This is documented in the CHANGELOG entry alongside the new flag rather than encoded in code.

## Wire and contract impact

None. This change touches the `PostgresConfig` / `SqliteConfig` struct shape (additive field with default), the body of `PostgresA2aStorage::new()` / `SqliteA2aStorage::new()`, and nothing else. No A2A protocol surface, no storage trait, no handler, no transport. Existing adopters who do not set the flag observe identical behavior to 0.1.17.

The struct field is a regular `pub` field, matching the existing `PostgresConfig { database_url, max_connections }` style. Adopters set it positionally or via `..Default::default()`:

```rust
PostgresConfig {
    database_url: "postgres://app_role@host/a2a".into(),
    max_connections: 10,
    assume_schema_initialized: true,
}
```

## Rejected alternatives

- **Make the flag the default.** Would break every adopter that currently relies on `new()` provisioning the schema. The whole point of the existing behavior is that "the framework just works" on first boot. Opt-in is the only acceptable shape for an additive change inside a 0.1.x cycle.
- **Use a separate constructor (`PostgresA2aStorage::new_without_schema_init()`).** Doubles the surface and creates a fork point that has to be maintained in parallel with `new()`. A config field is cheaper, fits the existing builder shape, and falls naturally into the existing `..Default::default()` pattern.
- **Extract DDL into a shipped SQL file and run it from a separate binary.** Correct end-state, but a larger surface (file ownership, hashing/versioning, optional migration framework dep). Deferred — see follow-ups.
- **Detect the privilege at runtime and skip DDL transparently.** Implicit behavior. Adopters who *want* the framework to provision and silently get the least-privilege path because credentials happened to be narrow would see "first boot succeeded, first INSERT failed" with no signal. Explicit opt-in is honest.

## Follow-ups (not in this slice)

- **Ship canonical migration files** under `crates/turul-a2a/migrations/{postgres,sqlite}/` so operators have an authoritative source for the schema without reading Rust source. Open question: do we adopt a migration framework (sqlx-migrate, refinery) or hand-rolled numbered SQL files? Decision deferred until at least one adopter requests it.
- **Reconsider the `let _ = …` swallow of `ALTER TABLE` errors** in `create_tables()`. With the new flag this is mostly moot for the least-privilege case (those callers don't enter `create_tables()`), but for adopters who stay on default behavior with narrowed creds, silent migration is still a footgun. Out of scope for ADR-019.

## Verification

- Unit tests in `storage::postgres::tests` and `storage::sqlite::tests`: construct each backend with `assume_schema_initialized: true` against a database with the `a2a_*` tables absent, assert `create_task` fails with a relation-not-exist error (proves DDL was skipped). With the flag unset, all existing parity tests continue to pass unchanged.
- No new transport tests (no wire surface change).
