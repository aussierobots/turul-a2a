# Implementation Plan — 0.1.4 Advanced Task Lifecycle

- **Status:** Proposed (awaiting review before code)
- **Date:** 2026-04-18
- **Covers ADRs:** 010 (EventSink + long-running), 011 (push delivery), 012 (cancellation propagation)
- **Release target:** 0.1.4 (additive, no breaking trait changes)

## Intent

Deliver the advanced task lifecycle features — long-running tasks, executor-emitted progress, cancellation propagation, push notification delivery — without rebuilding shared runtime primitives three times.

The original phase numbering (phase 1 cancellation → phase 2 EventSink → phase 3 push) is wrong: cancellation, EventSink, and push delivery all depend on the same substrate (`InFlightRegistry`, `SupervisorSentinel`, terminal-CAS contract, `Secret<String>`). Building them out in that order would design the substrate in phase 1, redesign it in phase 2 once EventSink's requirements land, and redesign it again in phase 3 when push delivery's claim/dispatcher machinery arrives. That's 2× the work with real reconciliation risk.

Correct ordering: **shared runtime substrate first**, then **atomic store CAS**, then the three vertical feature slices (cancellation, EventSink, push delivery), then **E2E compliance reference**, then **release**.

## Phase Summary

| Phase | Name | Estimated | Blocks | Feature-flaggable |
|---|---|---|---|---|
| **A** | Shared Runtime Substrate | 1 day | B, C, D, E | No — additive |
| **B** | Atomic Store Terminal CAS | 1 day | C, D, E | No — additive error variant |
| **C** | Cancellation Vertical Slice | 1.5 days | D, E | Yes (transition) |
| **D** | EventSink + Long-Running | 3 days | E, F | Yes (`event-sink` flag) |
| **E** | Push Delivery | 2 days | F | Yes (`push-delivery` flag) |
| **F** | E2E Compliance Reference Agent | 1 day | G | No |
| **G** | Release 0.1.4 | 0.5 day | — | N/A |

Total estimated effort: ~10 focused days. Phases A and B ship no user-visible feature; they're invariants. Phases C/D/E are the features.

---

## Phase A — Shared Runtime Substrate

### Goal

Scaffold `InFlightRegistry`, `SupervisorSentinel`, `ExecutionContext` evolution, builder config surface, and `Secret<String>`. Make the runtime shape exist so phases C/D/E plug in without redesigning it.

### Files touched

- **New**:
  - `crates/turul-a2a/src/server/in_flight.rs` — `InFlightRegistry`, `InFlightHandle`, `SupervisorSentinel` drop-guard
  - `crates/turul-a2a/src/secret.rs` — `Secret<String>` newtype with redacting `Debug` / `Display`
  - `crates/turul-a2a/src/metrics.rs` — metric constant declarations (names only, emissions land per phase)
  - `crates/turul-a2a/tests/runtime_substrate_tests.rs` — 6 tests below
- **Modified**:
  - `crates/turul-a2a/src/executor.rs` — `ExecutionContext.events: EventSink` field added; `EventSink` is a stub type in phase A (real methods land in phase D)
  - `crates/turul-a2a/src/server/app_state.rs` — new fields: `in_flight: Arc<InFlightRegistry>`, placeholders for `cancellation_supervisor` and `push_delivery_store` (wired to stubs)
  - `crates/turul-a2a/src/server/builder.rs` — new config fields (all optional, all documented with default and rationale): `blocking_task_timeout`, `timeout_abort_grace`, `cancel_handler_grace`, `cancel_handler_poll_interval`, `cross_instance_cancel_poll_interval`, `push_max_attempts`, `push_backoff_base`, `push_backoff_cap`, `push_backoff_jitter`, `push_request_timeout`, `push_connect_timeout`, `push_read_timeout`, `push_claim_expiry`, `push_config_cache_ttl`, `push_failed_delivery_retention`, `push_max_payload_bytes`, `allow_insecure_push_urls`, `outbound_url_validator`

### Tests (all internal, no wire behavior)

1. `in_flight_registry_insert_remove_roundtrip` — basic entry lifecycle.
2. `supervisor_sentinel_drops_on_normal_exit_removes_entry_and_joinhandle` — happy path.
3. `supervisor_sentinel_drops_on_panic_still_cleans_up` — uses `tokio::spawn` that panics; assert JoinHandle aborted, entry removed.
4. `supervisor_sentinel_panic_increments_metric` — `framework.supervisor_panics` counter visible to test.
5. `yielded_oneshot_fires_once_under_concurrent_triggers` — spawn 10 tasks all trying to fire yielded; exactly one send succeeds via CAS on `yielded_fired`.
6. `secret_newtype_never_leaks_in_debug_or_display` — `format!("{:?}", secret)` contains `[REDACTED]`, does NOT contain the underlying string.

### ADR acceptance tests satisfied

None directly — this is scaffolding. Partial structure for ADR-010 §4.4 and ADR-012 §3. Phase A's tests prove the sentinel invariants hold, which later phases rely on.

### Intentionally deferred to later phases

- `EventSink` method bodies (phase D).
- Cancellation token trip on `:cancel` (phase C).
- Cross-instance poll loop in supervisor (phase C).
- Push delivery worker (phase E).
- Metric emissions (registered names only; emissions land in the phase that owns them).

### Rollback boundary

Phase A can be committed and released as-is without changing any externally-observable behavior. `ExecutionContext.events` exists but executors that ignore it (the entire existing test suite) continue to work. No trait changes, no migration, no wire impact.

**Kill-switch if needed**: revert the commit. Nothing else depends on it yet.

---

## Phase B — Atomic Store Terminal CAS

### Goal

Enforce `TerminalStateAlreadySet` on `A2aAtomicStore::update_task_status_with_events` across all four backends. Parity tests for concurrent terminal writes.

### Files touched

- **Modified**:
  - `crates/turul-a2a/src/storage/error.rs` — add `TerminalStateAlreadySet { task_id, current_state }` variant
  - `crates/turul-a2a/src/storage/traits.rs` — docstring contract clause on `update_task_status_with_events` (normative)
  - `crates/turul-a2a/src/storage/memory.rs` — check current state under write lock, return error on terminal-to-terminal
  - `crates/turul-a2a/src/storage/sqlite.rs` — `UPDATE … WHERE status_state NOT IN (terminals)` conditional
  - `crates/turul-a2a/src/storage/postgres.rs` — same pattern
  - `crates/turul-a2a/src/storage/dynamodb.rs` — `TransactWriteItems` with `ConditionExpression` on `statusState`
  - `crates/turul-a2a/src/storage/parity_tests.rs` — 4 new parity tests below
- **Existing call sites audited**:
  - `router.rs::core_cancel_task` — already writes terminal via atomic store; now must handle `TerminalStateAlreadySet` (but phase B doesn't change its behavior yet; phase C rewrites it).
  - Any other terminal writers in `router.rs` and `streaming/mod.rs` — audit and handle or document TODO for phase C/D.

### Tests

1. `single_terminal_writer_parity_in_memory` — 10 concurrent racers writing different terminals (`COMPLETED`, `FAILED`, `CANCELED`, `REJECTED`). Exactly one succeeds; 9 receive `TerminalStateAlreadySet`. Event store has exactly one terminal event. Persisted state matches winner.
2. `single_terminal_writer_parity_sqlite` — feature-gated, same assertions.
3. `single_terminal_writer_parity_postgres` — feature-gated, same assertions.
4. `single_terminal_writer_parity_dynamodb` — feature-gated, same assertions (may use DynamoDB Local).
5. `state_machine_violation_distinct_from_terminal_already_set` — attempt an illegal transition (e.g., `SUBMITTED → INPUT_REQUIRED` from unrelated angle). Assert error is `InvalidStateTransition`, not `TerminalStateAlreadySet`. Callers can distinguish "you lost the race" from "you tried an illegal transition."

### ADR acceptance tests satisfied

- **ADR-010 §9 test #12** (single-terminal-writer parity across backends) — fully covered.
- **ADR-010 §7.1** contract clause — implementation evidence.

### Intentionally deferred

- EventSink consumption of the CAS error — phase D wires `EventSink::complete/fail/cancelled/reject` to translate `TerminalStateAlreadySet` into `"EventSink is closed: terminal already set"`.
- Cancel handler consumption of the CAS error — phase C's rewritten `core_cancel_task` uses the race-aware loser pattern.

### Rollback boundary

Phase B can ship alone. Existing non-terminal writes are unchanged. Existing terminal writes that used to silently succeed on re-write now surface `TerminalStateAlreadySet`. **Audit required**: no existing test or production path relies on silently-successful terminal-to-terminal writes. Verify before merge.

**Kill-switch**: add a temporary `#[cfg(feature = "strict-terminal-cas")]` feature flag that gates the new error path; default off during audit, flip on once all callers are clean. Can be removed in phase C.

---

## Phase C — Cancellation Vertical Slice

### Goal

`ctx.cancellation` becomes a real signal tripped by `:cancel`. Cross-instance propagation via storage marker. Blocking handler with race-aware fallback.

### Files touched

- **New**:
  - `crates/turul-a2a/tests/cancellation_tests.rs` — 14 tests
- **Modified**:
  - `crates/turul-a2a/src/storage/traits.rs` — add `A2aTaskStorage::set_cancel_requested`; new trait `A2aCancellationSupervisor` with `supervisor_get_cancel_requested`, `supervisor_list_cancel_requested`
  - `crates/turul-a2a/src/storage/{memory,sqlite,postgres,dynamodb}.rs` — implement both traits. Add `cancel_requested` column/attribute. SQL backends: additive migration.
  - `crates/turul-a2a/src/storage/parity_tests.rs` — marker-related parity tests
  - `crates/turul-a2a/src/router.rs::core_cancel_task` — rewritten per ADR-012 §2: validate → write marker → local token trip → grace wait with poll → race-aware force-commit via phase B's CAS
  - `crates/turul-a2a/src/server/in_flight.rs` — supervisor task gains a cross-instance poll loop calling `supervisor_list_cancel_requested` every `cross_instance_cancel_poll_interval`
  - `crates/turul-a2a/src/server/app_state.rs` — wire `cancellation_supervisor: Arc<dyn A2aCancellationSupervisor>` from builder
  - `crates/turul-a2a/src/server/builder.rs` — activate the cancel-related config fields (declared in phase A)

### Tests (maps 1:1 to ADR-012 §11)

1. `same_instance_cancel_trips_executor_token`
2. `cross_instance_cancel_via_storage_marker`
3. `cancel_on_orphaned_task`
4. `cancel_vs_complete_race` (both sub-cases: cancel-wins, complete-wins)
5. `cancel_vs_cancel_idempotency`
6. `cancel_on_terminal_returns_409` (preserves existing behavior)
7. `cancel_by_wrong_owner_returns_404`
8. `batch_supervisor_list_cancel_requested_parity` (across all 4 backends)
9. `marker_write_conditional_on_non_terminal`
10. `blocking_send_and_cancel_interop`
11. `streaming_subscriber_sees_terminal_canceled`
12. `cross_instance_poll_load_under_n_in_flight`
13. `supervisor_panic_cleanup_via_sentinel` — builds on phase A sentinel tests; verifies the supervisor's cross-instance poll loop cleans up correctly after induced panic
14. `supervisor_get_cancel_requested_unreachable_via_taskstorage` — compile-time negative test (trybuild or API-surface inspection)

### ADR acceptance tests satisfied

- **ADR-012 §11** fully satisfied.
- Terminal-commit race resolution from **ADR-010 §7.1** exercised via real cancel-vs-complete tests.

### Intentionally deferred

- Cross-instance broker-based fast path (§3 of ADR-012) — 0.1.x uses polling only; broker-based wake-up is an optimization for later.
- Admin API for "list requested cancels" — operator visibility for stuck cancels.

### Rollback boundary

New `core_cancel_task` can be gated behind a transient feature flag (`cancel-propagation`) for the landing window. With the flag off, the handler falls back to the current direct-atomic-write behavior. Once all tests pass with the flag on by default, delete the flag and the legacy code.

**Feature-flag removal criterion**: all 14 tests green on default features; at least one integration run on PostgreSQL and DynamoDB backends.

---

## Phase D — EventSink + Long-Running Tasks

### Goal

Executor can emit progress, interrupted, and terminal events via `ctx.events`. Three send modes work (blocking, non-blocking, streaming). Blocking send returns on task-visible state, not executor exit.

### Files touched

- **New**:
  - `crates/turul-a2a/src/event_sink.rs` — implementation of `EventSink` public methods (phase A declared the type; phase D fills in bodies)
  - `crates/turul-a2a/tests/long_running_tests.rs` — 15 tests
- **Modified**:
  - `crates/turul-a2a/src/executor.rs` — `EventSink` API: `set_status`, `emit_artifact`, `complete`, `fail`, `cancelled`, `reject`, `require_input`, `require_auth`, `is_closed`
  - `crates/turul-a2a/src/server/in_flight.rs` — atomic-store commit hook fires `InFlightHandle.yielded` on first terminal/interrupted write
  - `crates/turul-a2a/src/router.rs` — rewrite `message:send` (blocking and `return_immediately = true` paths), `message:stream`, using the in-flight registry and spawn-and-supervise pattern
  - `crates/turul-a2a/src/streaming/mod.rs` — stream events from durable store include executor-emitted sink events (via existing ADR-009 infrastructure; no new channel)
  - `crates/turul-a2a/src/server/builder.rs` — activate `blocking_task_timeout`, `timeout_abort_grace` knobs
  - Legacy-executor-detection rule (ADR-010 §7.2) in the router: if `execute()` returns with task state terminal AND sink was never used, synthesize the corresponding event

### Tests (maps 1:1 to ADR-010 §9)

1. `long_running_executor_completes_after_delay` — 1.5s total wall-clock, 3 events in store.
2. `client_polling_reaches_terminal` — non-blocking send, poll loop.
3. `streaming_receives_executor_progress_in_order` — strict ordering, increasing sequence.
4. `subscribe_while_running_replays_prior_sink_events` — gated-emit executor; replay + live.
5. `cancellation_token_reaches_running_executor` — reuses phase C mechanics.
6. `cancel_vs_complete_race_deterministic` — uses phase B CAS.
7. `multiple_subscribers_receive_identical_ordered_events`
8. `cross_instance_executor_progress`
9. `framework_default_fallback_for_legacy_executors` — echo-agent and existing tests continue to pass unchanged.
10. `failed_executor_emits_failed_terminal`
11. `blocking_timeout_cancellation_and_abort` (three sub-cases: 11a cooperative, 11b abort-fallback, 11c race-aware-loser).
12. `single_terminal_writer_parity_via_eventsink` — verifies phase B's CAS works through the sink path.
13. `subscribe_on_terminal_task_returns_unsupported_operation` — validates ADR-010 §4.3 supersession of ADR-009 §12.
14. `executor_rejected_via_eventsink_reject` — proto enum value 7 verified on wire.
15. `eventsink_methods_reject_writes_after_terminal`

### ADR acceptance tests satisfied

- **ADR-010 §9** fully satisfied.

### Intentionally deferred

- Custom event types outside proto variants (ADR-010 §8 — proto-only for spec compliance).
- Batched sink emits (each sink call is one transaction).
- Slow-subscriber back-pressure (events commit to store regardless; slow subscribers catch up via replay per ADR-009).
- Artifact chunk streaming within a single artifact (existing ADR-006 `last_chunk` semantics unchanged).

### Rollback boundary

`event-sink` feature flag (default on) gates the new send-mode handlers and EventSink method bodies. With flag off, router falls back to current synchronous execute-then-write behavior. Legacy executors work regardless (the detection rule in ADR-010 §7.2 handles both paths).

**Feature-flag removal criterion**: all 15 tests green; reference agent in phase F uses EventSink end-to-end; echo-agent still works unchanged.

---

## Phase E — Push Delivery

### Goal

Webhooks actually fire. Background dispatcher + per-config delivery tasks. At-least-once semantics. Retry with 3-minute horizon. SSRF protections. Secret-redaction everywhere.

### Files touched

- **New**:
  - `crates/turul-a2a/src/push/mod.rs` — module root
  - `crates/turul-a2a/src/push/delivery.rs` — `PushDeliveryWorker`, per-config delivery task, retry loop
  - `crates/turul-a2a/src/push/claim.rs` — `A2aPushDeliveryStore` trait, `DeliveryClaim`, `ClaimStatus`, `DeliveryOutcome`, `FailedDelivery`, `DeliveryErrorClass`
  - `crates/turul-a2a/src/push/ssrf.rs` — IP blocklist, DNS-resolve-once resolver, outbound validator hook
  - `crates/turul-a2a/tests/push_delivery_tests.rs` — 23 tests
- **Modified**:
  - `crates/turul-a2a/src/storage/{memory,sqlite,postgres,dynamodb}.rs` — implement `A2aPushDeliveryStore`
  - `crates/turul-a2a/src/storage/parity_tests.rs` — delivery claim parity tests
  - `crates/turul-a2a/src/server/app_state.rs` — wire `push_delivery_store`
  - `crates/turul-a2a/src/server/builder.rs` — activate push config knobs; startup spawns `PushDeliveryWorker` if enabled; validate `push_claim_expiry > retry_horizon`
  - `crates/turul-a2a/src/secret.rs` — `Secret<String>` used for `AuthenticationInfo.credentials` and `TaskPushNotificationConfig.token` at the consumption site
  - CRUD handlers for push configs — no behavior change, but tests assert that credentials do not leak in list responses' logs (§4a)

### Tests (maps 1:1 to ADR-011 §13)

1. `basic_delivery_on_terminal_event`
2. `multiple_configs_fire_independently`
3. `retry_on_5xx`
4. `giveup_after_max_attempts` (CI uses short-horizon config for speed)
5. `no_retry_on_4xx_non_408_429`
6. `429_respects_retry_after`
7. `config_deleted_mid_retry_abandons_cleanly`
8. `cross_instance_claim_dedup`
9. `claim_expiry_repickup_after_crash`
10. `task_deletion_between_event_and_delivery_abandons`
11. `artifact_events_not_delivered_0_1_x_scope`
12. `authentication_variants_bearer_basic_apikey`
13. `framework_committed_canceled_triggers_delivery` — uses phase C's CANCELED commit.
14. `push_config_crud_parity_unchanged`
15. `dispatcher_panic_recovery` — uses phase A sentinel.
16. `secret_redaction_coverage` — sentinel credentials never in logs/metrics/traces/errors/records.
17. `ssrf_blocklist_rejects_private_destinations` — 127/8, 10/8, 169.254.169.254, 192.168/16, ::1, fe80::1234.
18. `dns_rebinding_defense` — resolver returns public IP first, private second; framework pins to first.
19. `outbound_allowlist_hook` — deployment validator rejects non-allowlisted host.
20. `claim_contention_metrics_in_two_instance` — both instances attempt, one wins, metrics reflect.
21. `at_least_once_redelivery_after_crash` — claim-then-crash → re-pickup → second POST.
22. `retry_horizon_covers_receiver_restart` — 90s outage → delivery succeeds on attempt 6/7.
23. `failed_delivery_record_inspectable_and_secret_free` — `list_failed_deliveries` returns expected record; no credential sentinels.

### ADR acceptance tests satisfied

- **ADR-011 §13** fully satisfied.

### Intentionally deferred

- Circuit-breaker (future ADR).
- Artifact event delivery opt-in (requires proto coordination).
- DLQ / admin retry API.
- Sharded dispatcher.
- OAuth2 client for credential refresh.
- `AgentCapabilities.push_notifications = false` rejection at CRUD (ADR-011 §2 — future tightening).

### Rollback boundary

`push-delivery` feature flag (default on) gates the `PushDeliveryWorker` spawn. With flag off, configs are stored but never fire — current 0.1.3 behavior. CRUD continues to work either way.

**Feature-flag removal criterion**: all 23 tests green; secret redaction test verified; cross-instance dedup demonstrated on PostgreSQL or DynamoDB.

---

## Phase F — E2E Compliance Reference Agent

### Goal

Single reference executor that exercises the full lifecycle matrix. Wire-format tests that drive it end-to-end via real HTTP. Assertions against proto JSON, not internal Rust state.

### Files touched

- **New**:
  - `examples/reference-agent/Cargo.toml` — new example crate, workspace member
  - `examples/reference-agent/src/main.rs` — executor with skills for each lifecycle path
  - `crates/turul-a2a/tests/e2e_lifecycle.rs` — 10 wire-format E2E tests
- **Modified**:
  - Workspace `Cargo.toml` adds the example crate to members list

### Reference-agent skills

The reference agent implements a single `AgentExecutor` with routing-by-first-message to different behaviors:

- `long-running` — emits `WORKING` via sink, sleeps 500ms, emits `ArtifactUpdate`, sleeps 500ms, emits `COMPLETED`.
- `artifact-stream` — emits 3 `ArtifactUpdate` events, then `COMPLETED`.
- `require-input` — emits `INPUT_REQUIRED` with a prompt message; second turn resumes and emits `COMPLETED`.
- `require-auth` — emits `AUTH_REQUIRED`.
- `cooperative-cancel` — loops checking `ctx.cancellation`; on trip, emits `sink.cancelled(reason)`.
- `reject` — emits `sink.reject("policy: not permitted")`.
- `fail` — emits `sink.fail(A2aError::Internal { message: "simulated failure" })`.
- `instant-complete` — legacy-style: mutates `&mut task` and returns; no sink use. Validates §7.2 detection rule.

### Tests

1. `e2e_long_running_via_polling` — client polls `GetTask` until terminal.
2. `e2e_long_running_via_streaming` — SSE observer receives ordered events.
3. `e2e_cancel_mid_execution` — cooperative-cancel skill + `CancelTask` → terminal CANCELED observed.
4. `e2e_push_notification_delivery` — wiremock webhook receives Task JSON with correct Authorization header.
5. `e2e_executor_progress_streaming` — artifact-stream skill + SSE client receives 4 events.
6. `e2e_crash_recovery_replay` — start stream, simulated disconnect + reconnect with Last-Event-ID.
7. `e2e_rejected_terminal_wire_format` — reject skill produces `TASK_STATE_REJECTED` (proto enum 7) on wire.
8. `e2e_blocking_timeout_behavior` — unresponsive executor with short timeout; wire response is FAILED.
9. `e2e_ssrf_blocks_private_webhook` — register push config with 127.0.0.1 URL; delivery never leaves machine.
10. `e2e_at_least_once_delivery_redundancy` — claim-then-crash → second POST arrives.

### ADR acceptance tests satisfied

- Cross-ADR wire-format validation. Every assertion is against camelCase JSON keys, proto enum wire names, proto HTTP routes, `google.rpc.ErrorInfo` fields — never against internal Rust types or private API surface.
- The compliance agent reviews every E2E test diff before merge.

### Intentionally deferred

- Performance / load testing (phase F validates correctness, not throughput).
- Multi-tenant E2E (existing tenant tests cover the auth-isolation path; phase F focuses on lifecycle).

### Rollback boundary

Reference agent is a separate example crate. If it's buggy, it does not affect the framework crates. Worst case: delete `examples/reference-agent/` and move on. The E2E tests in `crates/turul-a2a/tests/e2e_lifecycle.rs` depend on the reference agent being built; they can be `#[ignore]`d if the example is broken while still shipping the framework.

---

## Phase G — Release 0.1.4

### Goal

Publish 0.1.4 to crates.io following the CLAUDE.md-documented workflow.

### Checklist

1. **Update CHANGELOG.md** with all phase-A–F deliverables.
2. **Bump** `workspace.package.version` 0.1.3 → 0.1.4.
3. **Remove transition feature flags** (`cancel-propagation`, `event-sink`, `push-delivery`) — they were transition-only; defaults are "on" after feature-flag-removal criteria met per phase.
4. **Pre-publish gate** (per CLAUDE.md "Release & Publish"):
   - `cargo test --workspace` green
   - `cargo test --workspace --features compat-v03` green
   - `cargo test --workspace --features sqlite,postgres,dynamodb` green (with test services up)
   - `cargo package -p <each> --no-verify --allow-dirty` warning-free
   - `cargo doc --no-deps` warning-free
5. **Explicit user authorization** per `cargo publish` call. Publish in dep order: proto → types → jwt-validator → a2a → {client, auth, aws-lambda}. Respect crates.io rate limit.
6. **Tag** `v0.1.4` annotated; `git push origin v0.1.4`.
7. **Update project memory** (`project_a2a_overview.md`) with 0.1.4 delivery summary.

### Intentionally deferred to 0.2.0 or later

- gRPC transport (ADR-001 item).
- Skill-level `security_requirements` (agent-level only today).
- Shared `turul-jwt-validator` extraction (ADR-007 deferred).
- Push delivery artifact events (would require proto field addition).
- Push delivery circuit-breaker.
- Push delivery DLQ / admin retry.
- OAuth2 client for push delivery.
- Admin APIs for cancel inspection, failed deliveries, trigger-push.
- Cross-instance broker pub/sub (optimization over polling).

---

## Dependency Graph

```
           ┌──────────────────────┐
           │  A: Runtime Substrate│
           │   registry, sentinel,│
           │   Secret, metrics    │
           └──────────┬───────────┘
                      │
           ┌──────────▼───────────┐
           │  B: Terminal CAS     │
           │   atomic store contract│
           └──────────┬───────────┘
                      │
        ┌─────────────┼─────────────┐
        │             │             │
┌───────▼────┐ ┌──────▼──────┐ ┌────▼────────┐
│ C: Cancel  │ │ D: EventSink│ │ (E waits for│
│            │ │             │ │  C + D)     │
└──────┬─────┘ └──────┬──────┘ └────┬────────┘
       │              │              │
       └──────┬───────┘              │
              │                      │
       ┌──────▼───────────────────▼────────┐
       │  E: Push Delivery                  │
       │  (uses B CAS, D events, A sentinel)│
       └──────────────┬─────────────────────┘
                      │
           ┌──────────▼───────────┐
           │  F: E2E Reference    │
           └──────────┬───────────┘
                      │
           ┌──────────▼───────────┐
           │  G: Release 0.1.4    │
           └──────────────────────┘
```

- **C and D can run in parallel** once B lands, if two contributors. Sequentially they're fine too.
- **E strictly after B** (needs CAS) and **preferably after D** (so push can deliver executor-emitted events in its E2E tests).

## Rollback Boundaries (summary)

| Phase | If phase is broken mid-merge | Smallest rollback unit |
|---|---|---|
| A | Revert commit — no one depends yet. | One commit. |
| B | Feature-flag `strict-terminal-cas`, default off. | Feature flag. |
| C | Feature-flag `cancel-propagation`, default off. | Feature flag. |
| D | Feature-flag `event-sink`, default off. | Feature flag. |
| E | Feature-flag `push-delivery`, default off. | Feature flag. |
| F | Mark E2E tests `#[ignore]`; framework ships without them. | Test annotations. |
| G | Don't publish. Tag can be deleted before push. | No-op. |

## Open Questions (to surface before starting phase A)

1. **Metric backend**: 0.1.x has no committed metric library. Structured logs for now, or commit to a specific metrics crate (metrics-rs, opentelemetry-sdk)? Impacts phase A module structure.
2. **`Secret<String>` crate choice**: `secrecy` crate (small, well-known) vs in-crate newtype? `secrecy` adds a dep; in-crate is more code. Recommend `secrecy` for less code and better ecosystem signaling.
3. **Claim table migration strategy for existing SQL deployments**: additive migrations are safe but need explicit migration SQL. Ship migration files in the crate or require adopters to apply manually? Recommend crate-embedded migrations via sqlx-migrate (already in-graph via sqlx).
4. **Test isolation between phases**: phase B parity tests verify concurrent writes. Phase C builds on those. If phase B parity tests flake (e.g., DynamoDB Local clock skew), do phase C's tests block? Recommend treating flakes as phase B regressions, not phase C problems.
5. **`outbound_url_validator` API ergonomics**: currently typed as `Arc<dyn Fn(&url::Url) -> Result<(), String> + Send + Sync>`. Workable but verbose. Consider a thin wrapper trait `OutboundUrlValidator` for discoverability. Recommend trait wrapper.

## Review Gate

Do not start phase A until this plan is reviewed and accepted. Specifically, confirm:

- [ ] Phase-ordering rationale is sound (substrate + CAS before vertical features).
- [ ] Rollback boundaries are acceptable.
- [ ] Feature-flag strategy for transitional phases is acceptable.
- [ ] Deferred items list is accurate and nothing essential has slipped.
- [ ] Open questions have answers or are acceptable as-is.

After acceptance, phase A begins.
