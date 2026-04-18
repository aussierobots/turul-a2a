# Implementation Plan — 0.1.4 Advanced Task Lifecycle

- **Status:** Accepted (2026-04-18, after one review cycle)
- **Date:** 2026-04-18
- **Covers ADRs:** 010 (EventSink + long-running), 011 (push delivery), 012 (cancellation propagation)
- **Release target:** 0.1.4 (additive, no breaking trait changes)

## Intent

Deliver the advanced task lifecycle features — long-running tasks, executor-emitted progress, cancellation propagation, push notification delivery — without rebuilding shared runtime primitives three times.

The original phase numbering (phase 1 cancellation → phase 2 EventSink → phase 3 push) is wrong: cancellation, EventSink, and push delivery all depend on the same substrate (`InFlightRegistry`, `SupervisorSentinel`, terminal-CAS contract, `Secret<String>`). Building them out in that order would design the substrate in phase 1, redesign it in phase 2 once EventSink's requirements land, and redesign it again in phase 3 when push delivery's claim/dispatcher machinery arrives. That's 2× the work with real reconciliation risk.

Correct ordering: **shared runtime substrate first**, then **atomic store CAS**, then the three vertical feature slices (cancellation, EventSink, push delivery), then **E2E compliance reference**, then **release**.

## Cross-Phase Policies (resolved before phase A)

### Transitional feature flags

Every landing-time flag is short-lived with a defined lifecycle:

1. **During active development** — flag exists, defaults to **off**. Implementation lives behind `#[cfg(feature = "…")]`. Default builds and default test runs see zero behavior change.
2. **During acceptance-testing** — the acceptance tests for that phase run with the flag on (via `cargo test --features …`). The old behavior path is still available.
3. **Once acceptance criteria are met** — a single commit flips the flag default **on**. Default builds now exercise the new path. Legacy path may remain for one additional commit for audit.
4. **Before phase G** — the flag is **removed**. `#[cfg]` gates deleted; legacy path deleted; feature no longer listed in `Cargo.toml`.

**No transitional flag ships to crates.io.** The 0.1.4 release artifact has no `strict-terminal-cas`, no `cancel-propagation`, no `event-sink`, no `push-delivery`. These are internal landing aids, not supported API surface. The only user-visible feature flags in 0.1.4 are the pre-existing ones (`in-memory`, `sqlite`, `postgres`, `dynamodb`, `compat-v03`).

### E2E release gate

`#[ignore]` on a lifecycle E2E test is a **development-only** escape hatch. Phase G is blocked until:

- All E2E tests in `crates/turul-a2a/tests/e2e_lifecycle.rs` are enabled (no `#[ignore]`).
- All E2E tests are green on default features.
- The reference agent (`examples/reference-agent/`) builds cleanly and passes its own `cargo check`.

If the reference agent is too broken to fix within phase F's budget, phase G delays until phase F is reworked. The framework **does not ship 0.1.4 without the lifecycle compliance proof**. Phase F is a release gate, not an optional example.

### Observability contract

0.1.4 uses **structured tracing events** via `tracing` crate, already a workspace dep. No `metrics-rs`, no `opentelemetry-sdk`. Decision defers a metrics backend to 0.2.x when real production consumers emerge.

**Event pattern**: framework emits structured events with stable `target = "turul_a2a::…"`, stable `event = "…"` field names, and typed fields (`task_id`, `tenant`, `config_id`, etc.). Adopters who need metrics wire a `tracing-subscriber` layer that translates events to their backend.

**Test pattern**: tests use `tracing-subscriber` with a capturing layer (custom or via `tracing-test` crate) to intercept events and assert on field values. No counter increments — assertions match event occurrence + fields. Example: phase A's supervisor-panic test asserts that *an* event with `target = "turul_a2a::supervisor_panic"` and field `task_id = …` fired, rather than `framework.supervisor_panics` incremented by 1.

The mental model everywhere in this plan: when the plan says "metric X incremented," read it as "a structured event X was emitted." The wire of observation is tracing events.

### ExecutionContext evolution policy

`ExecutionContext` changes **exactly once** in this release, in phase D, when `EventSink` gets its full API simultaneously. Phase A does **not** add the `events` field. Rationale: exposing a stub field in phase A leaks a not-yet-stable contract — every executor author who reads the struct would see the field and might attempt to use it. Deferring to phase D ensures the field and its API land together.

### Cancellation + EventSink integration gate

Phase C (cancellation) and phase D (EventSink) may be developed in parallel branches **after phase A+B land on `main`**. They MUST NOT be merged independently. The integration gate is:

- All ADR-010 §9 race tests (single-terminal-writer, cancel-vs-complete, cancel-vs-reject, etc.) pass on a branch that includes both C and D.
- All ADR-012 §11 race tests (cancel-vs-cancel, orphan, cross-instance, cancel-vs-complete) pass on the same combined branch.
- No test flake tolerated — all race tests must be green on three consecutive runs.

**Why**: C and D both write terminal states. Merging C with passing tests, then finding D breaks the terminal CAS invariant the tests asserted, is exactly the re-design-twice failure mode this plan exists to prevent. Combined integration proves the substrate holds under both callers simultaneously.

## Phase Summary

| Phase | Name | Estimated | Blocks | Landing flag (short-lived, removed before G) |
|---|---|---|---|---|
| **A** | Shared Runtime Substrate | 1 day | B, C, D, E | None — additive-only to internal APIs |
| **B** | Atomic Store Terminal CAS | 1 day | C, D, E | None — landing commit series contains audit + flip in one PR |
| **C** | Cancellation Vertical Slice | 1.5 days | D/E integration | `cancel-propagation` (dev-time only) |
| **D** | EventSink + Long-Running | 3 days | C/D integration | `event-sink` (dev-time only) |
| **E** | Push Delivery | 2 days | F | `push-delivery` (dev-time only) |
| **F** | E2E Compliance Reference Agent | 1 day | G | None; `#[ignore]` permitted only during F development |
| **G** | Release 0.1.4 | 0.5 day | — | All transitional flags **deleted** before publish |

Total estimated effort: ~10 focused days. Phases A and B ship no user-visible feature; they're invariants. Phases C/D/E are the features.

---

## Phase A — Shared Runtime Substrate

### Goal

Scaffold `InFlightRegistry`, `SupervisorSentinel`, `ExecutionContext` evolution, builder config surface, and `Secret<String>`. Make the runtime shape exist so phases C/D/E plug in without redesigning it.

### Files touched

- **New**:
  - `crates/turul-a2a/src/server/in_flight.rs` — `InFlightRegistry`, `InFlightHandle` (with `cancellation`, `yielded`, `yielded_fired`, `spawned`), `SupervisorSentinel` drop-guard
  - `crates/turul-a2a/src/server/obs.rs` — tracing-event constants (`target` strings, `event` field names) for supervisor-panic and future framework events. Pure declarations — no emitter logic.
  - `crates/turul-a2a/tests/runtime_substrate_tests.rs` — 6 tests below (using a capturing tracing subscriber)
- **Modified**:
  - `crates/turul-a2a/Cargo.toml` — add `secrecy = "0.10"` dependency (NOT workspace dep — stays in `turul-a2a` only so `turul-a2a-types` remains lightweight)
  - `crates/turul-a2a/src/lib.rs` — add `pub use secrecy::{Secret, ExposeSecret}` re-export for internal consumers + shallow wrapper docs
  - `crates/turul-a2a/src/server/app_state.rs` — new field: `in_flight: Arc<InFlightRegistry>`. Placeholders for `cancellation_supervisor: Arc<dyn A2aCancellationSupervisor>` and `push_delivery_store: Arc<dyn A2aPushDeliveryStore>` are NOT added yet — those fields land with their respective phases (C and E) so `AppState`'s public shape doesn't accrete stub fields
  - `crates/turul-a2a/src/server/builder.rs` — new config fields (all optional, all documented with default and rationale): `blocking_task_timeout`, `timeout_abort_grace`, `cancel_handler_grace`, `cancel_handler_poll_interval`, `cross_instance_cancel_poll_interval`, `push_max_attempts`, `push_backoff_base`, `push_backoff_cap`, `push_backoff_jitter`, `push_request_timeout`, `push_connect_timeout`, `push_read_timeout`, `push_claim_expiry`, `push_config_cache_ttl`, `push_failed_delivery_retention`, `push_max_payload_bytes`, `allow_insecure_push_urls`, `outbound_url_validator` (typed as `Arc<dyn OutboundUrlValidator + Send + Sync>` — trait, not closure; see phase E for trait definition).
  - **`ExecutionContext` is NOT modified in phase A**. The `events: EventSink` field is added in phase D along with the full `EventSink` API. This avoids exposing a stub contract.

### Tests (all internal, no wire behavior)

1. `in_flight_registry_insert_remove_roundtrip` — basic entry lifecycle.
2. `supervisor_sentinel_drops_on_normal_exit_removes_entry_and_joinhandle` — happy path.
3. `supervisor_sentinel_drops_on_panic_still_cleans_up` — uses `tokio::spawn` that panics; assert JoinHandle aborted, entry removed.
4. `supervisor_panic_emits_structured_event` — install a capturing `tracing-subscriber` layer. Trigger a supervisor panic. Assert: one event emitted with `target = "turul_a2a::supervisor_panic"`, `level = ERROR`, field `task_id = <expected>`, field `tenant = <expected>`. This is the observability-contract test — future phases reuse the same capturing-layer helper for their own event assertions.
5. `yielded_oneshot_fires_once_under_concurrent_triggers` — spawn 10 tasks all trying to fire yielded; exactly one send succeeds via CAS on `yielded_fired`.
6. `secrecy_newtype_never_leaks_in_debug_or_display` — using `secrecy::Secret<String>`, assert `format!("{:?}", secret)` contains `[REDACTED]` (secrecy's default) and does NOT contain the underlying string. Verifies the secrecy crate's redaction works as documented and our consumption pattern is correct.

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

Phase B lands as a **single atomic PR** — no transitional feature flag. Rationale: the change is an additive error variant + a docstring clause + parity tests; the per-backend conditional-write implementations are self-contained. Risk of breaking existing behavior is localized to terminal-to-terminal writes, which the audit below covers.

**Pre-merge audit** (mandatory, per CLAUDE.md TDD discipline):
1. `git grep update_task_status_with_events crates/` — enumerate every caller.
2. For each, verify: either the caller only writes from non-terminal → terminal states, OR the caller already handles/expects a failure on the second write.
3. Audit output committed as a brief note in the phase B PR description.

**Kill-switch if post-merge breakage surfaces**: revert the PR. The changeset is small enough to clean-revert without dragging uninvolved code. No in-tree feature flag is retained.

**Dependencies on later phases**: phases C, D, and E all rely on `TerminalStateAlreadySet` being the distinct error class. If phase B is reverted, phases C/D/E cannot proceed.

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

Landing sequence per the cross-phase transitional-flag policy:

1. **Development commits**: new `core_cancel_task` lives behind `#[cfg(feature = "cancel-propagation")]`. Flag defaults **off**. Legacy synchronous-cancel path remains default. Phase C acceptance tests run with `cargo test --features cancel-propagation`.
2. **Flip commit**: once all 14 tests green on `--features cancel-propagation`, a single commit flips the flag to default-on. CI runs both configurations to verify nothing regressed.
3. **Cleanup commit** (before phase G): remove the `#[cfg]` gates, delete legacy `core_cancel_task` code, remove the `cancel-propagation` feature from `Cargo.toml`.

**Flip-to-default-on criterion**: all 14 tests green on default features; integration run passes on PostgreSQL AND DynamoDB (matching phase B discipline). Three consecutive CI runs green, no flakes.

**C/D integration**: this phase is landed on a feature branch. Merging to `main` requires the combined C+D integration gate (see cross-phase policies): combined branch passes all ADR-010 §9 race tests AND all ADR-012 §11 race tests.

---

## Phase D — EventSink + Long-Running Tasks

### Goal

Executor can emit progress, interrupted, and terminal events via `ctx.events`. Three send modes work (blocking, non-blocking, streaming). Blocking send returns on task-visible state, not executor exit.

### Files touched

- **New**:
  - `crates/turul-a2a/src/event_sink.rs` — `EventSink` type and full public API introduced here (nothing in phase A)
  - `crates/turul-a2a/tests/long_running_tests.rs` — 15 tests
- **Modified**:
  - `crates/turul-a2a/src/executor.rs` — introduces `ExecutionContext.events: EventSink` field (first and only change to `ExecutionContext` in this release). `EventSink` API: `set_status`, `emit_artifact`, `complete`, `fail`, `cancelled`, `reject`, `require_input`, `require_auth`, `is_closed`
  - `crates/turul-a2a/src/server/in_flight.rs` — atomic-store commit hook fires `InFlightHandle.yielded` on first terminal/interrupted write
  - `crates/turul-a2a/src/router.rs` — rewrite `message:send` (blocking and `return_immediately = true` paths), `message:stream`, using the in-flight registry and spawn-and-supervise pattern
  - `crates/turul-a2a/src/streaming/mod.rs` — stream events from durable store include executor-emitted sink events (via existing ADR-009 infrastructure; no new channel)
  - `crates/turul-a2a/src/server/builder.rs` — activate `blocking_task_timeout`, `timeout_abort_grace` knobs
  - **Direct task-mutation executor compatibility** (ADR-010 §7.2). The existing `AgentExecutor::execute(&mut Task, ...)` signature where executors mutate the task in place and return continues to work through phase D's send-mode rewrite. This is the **pre-EventSink direct task-mutation path** — not "legacy" (we have no installed base yet; this is active dev). Phase D defines how that path coexists with `EventSink`-driven executors.
  - **Terminal state from every path MUST go through the same CAS boundary** (non-negotiable architectural invariant, not a compatibility carveout). Phase D enforces this by detecting when `execute()` returns with the task in a terminal or interrupted state AND the executor did not use `ctx.events`, and routing the resulting event through `A2aAtomicStore::update_task_status_with_events` (CAS-guarded). **Do NOT use `update_task_with_events`** for terminal persistence; full-task replacement bypasses the single-terminal-writer invariant.
    - Preferred phase D implementation: detect the status transition produced by the direct-mutation path and synthesize the `new_status` + corresponding event, then call `update_task_status_with_events`. Reserves `update_task_with_events` for non-status mutations (history appends, artifact metadata) that cannot affect terminal ownership.
  - **Phase D guard test** (required): race framework `CANCELED` (committed by `:cancel` at T=0) vs an executor-produced `COMPLETED` via the direct task-mutation path (returning `task.state = Completed` at T=1ms). Assert: exactly one terminal persists, the losing side surfaces `TerminalStateAlreadySet` (or the phase D sink-equivalent), only the winner's terminal event lands in the store.

### Tests (maps 1:1 to ADR-010 §9)

1. `long_running_executor_completes_after_delay` — 1.5s total wall-clock, 3 events in store.
2. `client_polling_reaches_terminal` — non-blocking send, poll loop.
3. `streaming_receives_executor_progress_in_order` — strict ordering, increasing sequence.
4. `subscribe_while_running_replays_prior_sink_events` — gated-emit executor; replay + live.
5. `cancellation_token_reaches_running_executor` — reuses phase C mechanics.
6. `cancel_vs_complete_race_deterministic` — uses phase B CAS.
7. `multiple_subscribers_receive_identical_ordered_events`
8. `cross_instance_executor_progress`
9. `direct_task_mutation_executor_still_works` — echo-agent and other pre-EventSink executors that mutate `&mut task` continue to pass unchanged under the new router. Validates the §7.2 detection rule.
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

Landing sequence (same transitional-flag policy as phase C):

1. **Development commits**: `ExecutionContext.events` field, new `message:send` / `message:stream` handlers, and `EventSink` API all live behind `#[cfg(feature = "event-sink")]`. Flag defaults **off**. Pre-EventSink direct task-mutation path is default. Phase D tests run with `--features event-sink`.
2. **Flip commit**: flag defaults **on**. Pre-EventSink path remains reachable as the fall-through for executors that don't use `ctx.events` (detected per ADR-010 §7.2).
3. **Cleanup commit** (before phase G): remove `#[cfg]` gates, delete any handler paths that bypassed `InFlightRegistry` during development, remove `event-sink` feature from `Cargo.toml`. The direct task-mutation path itself is NOT removed — it's a supported executor authoring style, not a compatibility shim.

**Flip-to-default-on criterion**: all 15 tests green on default features; direct task-mutation executors (echo-agent, existing test executors) still work unchanged; at least one manual smoke run of `cargo run -p echo-agent` confirms the binary.

**C/D integration**: this phase and phase C share the combined C+D integration gate: merged branch must pass all ADR-010 §9 + ADR-012 §11 race tests together, on three consecutive CI runs.

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

Landing sequence:

1. **Development commits**: `PushDeliveryWorker` spawn, `A2aPushDeliveryStore` trait, `push/` module, and SSRF guards all behind `#[cfg(feature = "push-delivery")]`. Flag defaults **off**. Configs are stored but never fire — exactly 0.1.3 behavior. Tests run with `--features push-delivery`.
2. **Flip commit**: flag defaults **on**. Worker spawns at startup by default.
3. **Cleanup commit** (before phase G): remove `#[cfg]` gates, remove `push-delivery` feature from `Cargo.toml`.

**Flip-to-default-on criterion**: all 23 tests green on default features; secret-redaction test (#16) passes with sentinel credentials never appearing in any captured output; cross-instance dedup (#20) verified on PostgreSQL or DynamoDB; retry horizon (#22) exercised at least once end-to-end (slow CI may use short-horizon config and scale-test separately).

**Dependency on D**: push delivery's framework-committed-CANCELED delivery test (#13) depends on the cancellation fallback path from phase C, and the event-commit hook from phase D. Phase E lands strictly **after** the combined C+D merge to `main`.

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
- `instant-complete` — direct task-mutation path: mutates `&mut task` and returns; no sink use. Validates the §7.2 detection rule.

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

Phase F is a **release gate for 0.1.4**, not an optional polish pass. The E2E tests ARE the compliance proof for the new lifecycle behavior — without them green, we have no end-to-end evidence that ADR-010/011/012's contracts hold under real HTTP + real storage + real client.

**Permitted during active F development**: individual E2E tests MAY carry `#[ignore]` if a specific test is blocking on incidental harness work (e.g., wiremock setup for a SSRF test). Ignored tests are expected to be temporary and tracked in the phase F PR description.

**Not permitted at phase G entry**: zero `#[ignore]` annotations on any E2E test in `crates/turul-a2a/tests/e2e_lifecycle.rs`. Zero. Phase G entry is gated on:

- All 10 E2E tests enabled and green on default features.
- `examples/reference-agent/` builds with `cargo check --all-features` clean.
- At least one manual `cargo run -p reference-agent` smoke confirming the binary starts.

If phase F budget is exhausted before these gates are met, phase G is **delayed**. The framework does NOT ship 0.1.4 without its compliance proof. Precedent: this matches how phase C/D's integration gate blocks single-phase merges — the E2E suite is 0.1.4's cross-phase gate.

**Escape hatch only for emergency releases**: if a critical CVE or data-loss bug is found in 0.1.3 requiring an immediate release, 0.1.4 can skip phase F (but must still include B at minimum) and the release notes explicitly call out the reduced compliance coverage. This is a last-resort option, not a normal path.

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

| Phase | Landing strategy | If broken post-merge |
|---|---|---|
| A | Single atomic PR. | Revert commit — no downstream dependencies yet. |
| B | Single atomic PR with pre-merge audit (every `update_task_status_with_events` caller verified). | Revert PR. Phases C/D/E cannot proceed until re-landed. |
| C | Landing behind `cancel-propagation` (default off) → flip on → cleanup. Three commit series. | Revert the flip commit; flag-off restores legacy path. Full revert if deeper issue. |
| D | Landing behind `event-sink` (default off) → flip on → cleanup. Three commit series. Combined C+D integration gate before merge to `main`. | Revert the flip commit; `ExecutionContext.events` field remains but is inert. Full revert if deeper issue. |
| E | Landing behind `push-delivery` (default off) → flip on → cleanup. Three commit series. | Revert the flip commit; worker doesn't spawn. Full revert if deeper issue. |
| F | E2E tests MUST be un-ignored at phase G entry (release gate). | If reference agent is broken, phase G delays until fixed. |
| G | Pre-publish checks in CLAUDE.md; transitional flags deleted; publish per dep order. | Don't push the tag. Unpublished commits can be amended. |

**Note on transitional flags**: `cancel-propagation`, `event-sink`, `push-delivery` are **never shipped to crates.io**. All three are deleted from `Cargo.toml` in the phase G pre-publish cleanup. The 0.1.4 release artifact's `[features]` section is identical to 0.1.3's: `in-memory`, `sqlite`, `postgres`, `dynamodb`, `compat-v03`.

## Resolved Design Decisions (decided before phase A)

1. **Observability backend**: structured `tracing` events. **NOT** `metrics-rs`, **NOT** `opentelemetry-sdk`. Tests assert event occurrence + fields via capturing `tracing-subscriber` layer. A metrics-crate migration can happen in 0.2.x when real production consumers emerge. See "Observability contract" in cross-phase policies.

2. **Secret newtype**: `secrecy` crate as a `turul-a2a`-only dependency (NOT added to `turul-a2a-types` — protocol/types remain lightweight). Rust ecosystem already recognizes `secrecy::Secret<T>` as the canonical redacting wrapper; writing our own just for this use case would be reinventing. Dependency footprint is small (no transitive deps beyond `zeroize`).

3. **SQL migrations**: **embedded via `sqlx-migrate`** (sqlx is already in-graph for SQLite/PostgreSQL backends) **plus committed `.sql` artifacts** under `crates/turul-a2a/migrations/`. Embedded migrations run automatically at first connection; `.sql` files are source-of-truth that adopters can inspect, run manually, or include in their own migration tooling. Both paths describe the same DDL.

4. **DynamoDB parity flake policy**: phase B parity tests are **blockers**. CAS is a foundational invariant, not a best-effort behavior. If DynamoDB Local is unreliable for concurrent-write tests, CI runs them against real DynamoDB (test account, single-digit-dollar monthly cost) rather than accepting flakes. Phase C/D/E race tests that depend on CAS semantics are hard-blocked by phase B parity stability.

5. **`outbound_url_validator` API**: **trait**, not raw closure. Defined as:

   ```rust
   pub trait OutboundUrlValidator: Send + Sync + std::fmt::Debug {
       fn validate(&self, url: &url::Url) -> Result<(), String>;
   }
   ```

   Builder accepts `Arc<dyn OutboundUrlValidator>`. Rationale (per review): easier to document, mock in tests, extend with DNS/allowlist state, and produce meaningful `Debug` output in diagnostic dumps. Closure shorthand (`|url| …`) can be offered as a blanket impl over `Fn` if ergonomics demand it, but the trait is the canonical API.

## Review Gate — Accepted

Reviewed and accepted 2026-04-18. Four P2 concerns raised in review:

- **P2-a — feature-flag inconsistency**: fixed. Unified "cross-phase transitional flags" policy added; every flag is short-lived, default-off during landing, default-on after acceptance tests, deleted before phase G. No transitional flag ships to crates.io.
- **P2-b — ignored E2E as release path**: fixed. Phase F rewritten to make lifecycle E2E tests a release gate. `#[ignore]` permitted only during active F development, zero tolerated at phase G entry. Emergency-release escape hatch explicitly narrow.
- **P2-c — metric visibility before backend decision**: fixed. Observability contract decided upfront (structured tracing events, no metrics crate). Phase A's supervisor-panic test rewritten to assert on a captured tracing event, not a counter increment.
- **P2-d — stub `ExecutionContext.events` leaks contract**: fixed. Phase A no longer touches `ExecutionContext`. Field added atomically in phase D alongside the full `EventSink` API.

Plus one additional gate adopted from the review:

- **C/D integration gate**: cancellation and EventSink branches may be developed in parallel but merged together. Combined branch must pass all ADR-010 §9 + ADR-012 §11 race tests on three consecutive CI runs.

Five open questions resolved:

- [x] Observability backend → structured tracing events.
- [x] Secret newtype → `secrecy` crate, in `turul-a2a` only.
- [x] SQL migrations → embedded via `sqlx-migrate` + committed `.sql` artifacts.
- [x] DynamoDB parity flakes → blockers; escalate to real DynamoDB if Local unreliable.
- [x] Outbound URL validator → trait, not closure.

Phase A may begin.
