# Changelog

All notable changes to the `turul-a2a` workspace are documented here.
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Format inspired by [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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

[0.1.3]: https://github.com/aussierobots/turul-a2a/releases/tag/v0.1.3
