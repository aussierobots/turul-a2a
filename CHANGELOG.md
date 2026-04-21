# Changelog

All notable changes to the `turul-a2a` workspace are documented here.
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Format inspired by [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.6] — 2026-04-21

### Fixed
- **DynamoDB cold-start read-after-write race.** A request Lambda on a
  fresh container could write a task via `create_task_with_events`
  and then immediately fail its own follow-up
  `update_task_status_with_events` (the Submitted → Working
  transition) with `TaskNotFound`, because the atomic store's
  internal pre-transaction read used DynamoDB's default
  eventually-consistent `GetItem`. The handler surfaced the error as
  HTTP 404 to the caller even though the task row existed, and the
  executor never ran. Classic symptom: a task stuck in `Submitted`
  state with a ~240 ms Lambda invocation and a 404 response body on
  the first call to a just-warmed container. Fix flips
  `ConsistentRead=true` on three read paths in
  `crates/turul-a2a/src/storage/dynamodb.rs`: `get_task`'s
  `GetItem`, `query_latest_event_sequence`'s `GetItem` (feeds the
  ADR-013 §6.3 monotonic sequence invariant), and
  `query_max_sequence`'s `Query` (picks the next event seq for the
  transact write). Cost: 2× RCU on these specific reads; the right
  trade for A2A's state-machine contract. No changes to other
  backends — SQLite, PostgreSQL and in-memory already satisfy
  read-your-writes by construction.

### Added
- **Read-your-writes requirement is now normative in the storage trait
  docs.** `A2aTaskStorage` grew a top-level "Read-your-writes
  requirement" section spelling out that a read issued after a
  successful write on the same instance MUST observe that write, the
  concrete failure mode when the contract is violated, and which
  backends satisfy it natively vs. which (DynamoDB) require an
  explicit `ConsistentRead=true`. `A2aAtomicStore` grew a companion
  "Read-your-writes across traits" clause making the cross-trait
  visibility guarantee explicit. Future backend authors have a
  durable anchor to point at.
- **Parity test `test_read_your_writes_across_traits`** walks the
  exact four-step send-message sequence
  (`create_task_with_events` → `get_task` →
  `update_task_status_with_events` → `get_task`) and asserts
  visibility at each step. Wired into all four backend test modules
  (in-memory, SQLite, PostgreSQL, DynamoDB). Against DynamoDB-Local
  (synchronous, in-memory) the test cannot empirically reproduce the
  cold-start race, but it documents the contract as executable code
  and will catch any future regression (e.g. an unintended read cache
  in front of `get_task`).
- **Example `README.md` files** for all five examples
  (`echo-agent`, `auth-agent`, `lambda-agent`, `lambda-stream-worker`,
  `lambda-scheduled-worker`) covering purpose, run/deploy steps, and
  how the three Lambda examples compose into an ADR-013 deployment.
  GitHub now renders a README on each example directory.

### Internal
- **Pre-existing clippy cleanups** surfaced by the publish gate:
  `rows.sort_by(|a, b| b.cmp(&a))` in `storage/dynamodb.rs` switched
  to `sort_by_key(|r| Reverse(r))`; unused symmetric encoder
  `claim_status_to_str` in `storage/sqlite.rs` marked
  `#[allow(dead_code)]` with a comment explaining parity with the
  PostgreSQL and DynamoDB analogs.

## [0.1.5] — 2026-04-20

### Fixed
- **`.storage()` no longer auto-wires push delivery (ADR-013 §4.3
  errata).** 0.1.4 required the backend passed to `.storage()` to
  implement `A2aPushDeliveryStore` and automatically assigned it as
  the push delivery store. Because all four first-party backends
  implement every storage trait on the same struct (ADR-009
  same-backend requirement), **any non-push adopter calling
  `.storage(storage)` was silently enrolled into push delivery** and
  forced to call `with_push_dispatch_enabled(true)` to pass the
  §4.3 consistency check — even when they never registered a push
  config.

  This was a conflation of "implements the trait" with "is wired for
  use". Fixed in both `turul_a2a::server::A2aServerBuilder::storage`
  and `turul_a2a_aws_lambda::LambdaA2aServerBuilder::storage`: the
  `A2aPushDeliveryStore` bound is removed, and no auto-assignment
  occurs. Push delivery is now strict opt-in via the existing
  `.push_delivery_store(storage.clone())` setter, which must be
  paired with `with_push_dispatch_enabled(true)` on the storage
  instance (unchanged consistency check).

  **Migration:**
  - Non-push deployments: no changes required. `.storage(storage)`
    works unchanged.
  - Push-using deployments: add `.push_delivery_store(storage.clone())`
    explicitly before `.build()`. This was already the documented
    path; it is now the only path.

## [0.1.4] — 2026-04-20

### Added
- **ADR-013: Lambda push-delivery parity.** Push fan-out now survives the
  request-Lambda returning before deliveries complete. A durable
  `a2a_push_pending_dispatches` marker is written atomically with the
  terminal task/event commit (opt-in via
  `InMemoryA2aStorage::with_push_dispatch_enabled(true)` and per-backend
  equivalents). Two recovery workers in `turul-a2a-aws-lambda`:
  - `LambdaStreamRecoveryHandler` — DynamoDB Streams trigger with
    `BatchItemFailures` partial-batch semantics.
  - `LambdaScheduledRecoveryHandler` — EventBridge Scheduler backstop that
    sweeps stale markers + reclaimable claims with bounded batch limits and
    a structured count summary for CloudWatch alerting.
  Two runnable examples: `examples/lambda-stream-worker` and
  `examples/lambda-scheduled-worker`.
- **Causal no-backfill for push configs** (ADR-013 §4.5 / §6). New column
  `a2a_tasks.latest_event_sequence` (+ `latestEventSequence` attribute on
  DynamoDB) is maintained by every atomic commit. `create_config` stamps
  `registered_after_event_sequence` under a CAS against the read sequence;
  `list_configs_eligible_at_event(seq)` enforces a strict-less-than
  eligibility filter so a config registered concurrently with the terminal
  commit is never retroactively delivered.
- **Server and Lambda builder consistency gates** (ADR-013 §4.3): both
  `A2aServer::builder()` and `LambdaA2aServerBuilder` reject any
  configuration where `push_delivery_store` is wired but
  `push_dispatch_enabled` is false (or vice versa), in either direction.
- **Per-crate license files.** Every publish crate now ships
  `LICENSE-APACHE` and `LICENSE-MIT` inline (symlinks in git; cargo
  package copies the real file content). Previously only the SPDX
  identifier was published.

### Changed
- **Release-scoped compliance wording.** `CLAUDE.md` §169 no longer
  asserts "No known spec compliance gaps remaining" as an absolute claim.
  The new wording scopes compliance to the tested A2A v1.0 HTTP+JSON and
  JSON-RPC server surfaces and names out-of-scope items separately:
  gRPC transport (deferred), deployment-layer TLS 1.2+ / gzip
  (reverse-proxy concern), and deeper per-skill security-scheme
  semantics (agent-level only). Re-verification of
  `proto/a2a.proto` SHA256 against
  `a2aproject/A2A:main/specification/a2a.proto` is now an explicit
  pre-release gate.
- **Upstream proto identity verified.** Vendored
  `crates/turul-a2a-proto/proto/a2a.proto` SHA256
  `4b74c0baa923ae0acb55474e548f1d6e5d3f83b80d757b65f8bf3e99a3c2257f` is
  byte-identical to upstream `a2aproject/A2A:main/specification/a2a.proto`
  as of the 0.1.4 release gate.
- **Workspace fmt sweep** under `rustfmt 1.9.0-stable` (2026-04-14). No
  semantic changes; purely cosmetic line-joining.

### Fixed
- **Package hygiene — tokio dev-dep dedup.** Per-crate dev-dependencies no
  longer redeclare `"full"` on top of the workspace-level base; the
  normalized `Cargo.toml` in every published `.crate` now ships
  `features = ["full", "test-util"]` (previously
  `["full", "full", "test-util"]`). No runtime effect.
- **Cargo-doc workspace build unblocked.** All three Lambda example
  crates declare `[[bin]] name = "bootstrap"` (required by cargo-lambda);
  added `doc = false` to the bin targets so
  `cargo doc --workspace --no-deps` no longer collides on output
  filenames.
- **Three rustdoc broken-link warnings** in `turul-a2a` resolved
  (`EventSinkInner` private-item link, `storage::parity_tests`
  `pub(crate)` link, `PushDispatcher::dispatch` unqualified path).
- **`InMemoryA2aStorage` rustdoc placement** restored after the clippy
  sweep had accidentally re-anchored the backend-level docs to a newly
  extracted type alias.

## [0.1.3] — 2026-04-17

First version published to crates.io.

### Added
- **A2A v0.3 compatibility layer** (opt-in `compat-v03` feature) for `a2a-sdk 0.3.x`
  and Strands Agents SDK clients. Provides JSON-RPC method normalization
  (`message/send` → `SendMessage`), role and task-state enum translation,
  optional `A2A-Version` header, a root `POST /` route, and additive agent card
  fields (`url`, `protocolVersion`, `additionalInterfaces`). Canonical v1.0
  builds remain strict.
- `Message::parse_first_data_or_text` for dual `Data`/`Text` part input parsing
  so wrapper-first executors work with both Turul and `a2a-sdk` clients.
- DynamoDB storage TTL support for tasks, events, and push configs (defaults:
  7 days / 24 hours / 7 days).
- Crate-level `//!` documentation on `turul-a2a` and `turul-a2a-types`.
- Workspace-level `CHANGELOG.md`.

### Changed
- Workspace version bumped from `0.1.2` (pre-publish) to `0.1.3`.
- Proto build enables `ignore_unknown_fields` so v1.0 clients tolerate additive
  v0.3 compat fields in remote agent cards (standard forward-compat behavior).
- All publishable crates now carry `repository`, `homepage`, `readme`,
  `keywords`, `categories`, and `rust-version` via workspace inheritance.
- Internal path-only dependencies (`turul-a2a`, `turul-jwt-validator`) now use
  `version + path` form so they can be published to crates.io.
- `bytes` dependency consolidated to workspace root.
- JWT E2E test switched to `rsa::rand_core::OsRng` to avoid `rand 0.10` /
  `rsa 0.9` version-skew (`rsa` still bridges to `rand_core 0.6`).

## [0.1.2] and earlier — unpublished

Pre-crates.io development. Highlights from the 0.1.x series (not published):

- **ADR-001 – ADR-009** accepted and implemented.
- **D3 durable event coordination** (ADR-009): atomic task + event writes via
  `A2aAtomicStore`; event store is the source of truth and the broker is a
  wake-up signal; terminal replay and `Last-Event-ID` reconnection verified;
  cross-instance streaming proven on shared backends.
- **Four parity-proven storage backends**: in-memory, SQLite, PostgreSQL,
  DynamoDB — all implementing `A2aTaskStorage`, `A2aEventStore`, and
  `A2aAtomicStore`.
- **Auth middleware** (`turul-a2a-auth`): Bearer/JWT via the local
  `turul-jwt-validator` (JWKS caching), and API Key. Transport-layer Tower
  layers with `AuthIdentity` injection (ADR-007).
- **AWS Lambda adapter** (`turul-a2a-aws-lambda`): thin wrapper over the same
  Axum `Router`; request/response only with anti-spoofing for authorizer
  identity (ADR-008).
- **Typed client** (`turul-a2a-client`): wrapper-first request/response types,
  SSE streaming with typed events, push-config CRUD.
- **Dual transport** (ADR-005): HTTP+JSON routes (`/message:send`,
  `/tasks/{id}`, …) and JSON-RPC (`POST /jsonrpc`, all 11 methods) share the
  same core handlers.
- **Spec-aligned error model** (ADR-004): `google.rpc.ErrorInfo` on all A2A
  errors, proto-normative HTTP/JSON-RPC status mapping.
- Proto-first architecture (ADR-001): types generated from normative
  `proto/a2a.proto`, wrapped in ergonomic Rust types with state-machine
  enforcement (ADR-002).

[0.1.6]: https://github.com/aussierobots/turul-a2a/releases/tag/v0.1.6
[0.1.5]: https://github.com/aussierobots/turul-a2a/releases/tag/v0.1.5
[0.1.4]: https://github.com/aussierobots/turul-a2a/releases/tag/v0.1.4
[0.1.3]: https://github.com/aussierobots/turul-a2a/releases/tag/v0.1.3
