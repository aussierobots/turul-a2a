//! Builder wiring tests for ADR-012 same-backend requirements on the
//! Lambda adapter. Proves that:
//!
//! - `.storage()` requires `A2aCancellationSupervisor` on the bundled
//!   backend (bound at the type-system level).
//! - `build()` rejects configurations where the cancellation supervisor
//!   is omitted — no silent fallback to a different backend.
//! - `build()` rejects same-backend mismatches that cross the supervisor
//!   boundary.
//!
//! Without these guarantees a production Lambda deployment would write
//! cancel markers to DynamoDB/Postgres while the supervisor reads from
//! an unrelated in-memory store, silently breaking cross-instance
//! cancellation (ADR-012).

use async_trait::async_trait;

use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::storage::{
    A2aAtomicStore, A2aCancellationSupervisor, A2aEventStore, A2aPushNotificationStorage,
    A2aStorageError, A2aTaskStorage, InMemoryA2aStorage,
};
use turul_a2a_types::{Message, Task};

use crate::LambdaA2aHandler;

struct DummyExecutor;

#[async_trait]
impl AgentExecutor for DummyExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        _ctx: &turul_a2a::executor::ExecutionContext,
    ) -> Result<(), A2aError> {
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

/// Stand-in second-backend type used to exercise same-backend enforcement.
/// Implements every storage trait so we can plug it into the individual
/// setters while reporting a distinct `backend_name`.
#[derive(Clone, Default)]
struct FakeBackend;

#[async_trait]
impl A2aTaskStorage for FakeBackend {
    fn backend_name(&self) -> &'static str {
        "fake-backend"
    }
    async fn create_task(
        &self,
        _t: &str,
        _o: &str,
        task: Task,
    ) -> Result<Task, A2aStorageError> {
        Ok(task)
    }
    async fn get_task(
        &self,
        _t: &str,
        _tid: &str,
        _o: &str,
        _h: Option<i32>,
    ) -> Result<Option<Task>, A2aStorageError> {
        Ok(None)
    }
    async fn update_task(
        &self,
        _t: &str,
        _o: &str,
        _task: Task,
    ) -> Result<(), A2aStorageError> {
        Ok(())
    }
    async fn delete_task(
        &self,
        _t: &str,
        _tid: &str,
        _o: &str,
    ) -> Result<bool, A2aStorageError> {
        Ok(false)
    }
    async fn list_tasks(
        &self,
        _f: turul_a2a::storage::TaskFilter,
    ) -> Result<turul_a2a::storage::TaskListPage, A2aStorageError> {
        Ok(turul_a2a::storage::TaskListPage {
            tasks: vec![],
            next_page_token: String::new(),
            page_size: 0,
            total_size: 0,
        })
    }
    async fn update_task_status(
        &self,
        _t: &str,
        _tid: &str,
        _o: &str,
        _s: turul_a2a_types::TaskStatus,
    ) -> Result<Task, A2aStorageError> {
        unimplemented!()
    }
    async fn append_message(
        &self,
        _t: &str,
        _tid: &str,
        _o: &str,
        _m: Message,
    ) -> Result<(), A2aStorageError> {
        Ok(())
    }
    async fn append_artifact(
        &self,
        _t: &str,
        _tid: &str,
        _o: &str,
        _a: turul_a2a_types::Artifact,
        _append: bool,
        _last: bool,
    ) -> Result<(), A2aStorageError> {
        Ok(())
    }
    async fn task_count(&self) -> Result<usize, A2aStorageError> {
        Ok(0)
    }
    async fn maintenance(&self) -> Result<(), A2aStorageError> {
        Ok(())
    }
    async fn set_cancel_requested(
        &self,
        _t: &str,
        _tid: &str,
        _o: &str,
    ) -> Result<(), A2aStorageError> {
        Ok(())
    }
}

#[async_trait]
impl A2aPushNotificationStorage for FakeBackend {
    fn backend_name(&self) -> &'static str {
        "fake-backend"
    }
    async fn create_config(
        &self,
        _t: &str,
        c: turul_a2a_proto::TaskPushNotificationConfig,
    ) -> Result<turul_a2a_proto::TaskPushNotificationConfig, A2aStorageError> {
        Ok(c)
    }
    async fn get_config(
        &self,
        _t: &str,
        _tid: &str,
        _c: &str,
    ) -> Result<Option<turul_a2a_proto::TaskPushNotificationConfig>, A2aStorageError> {
        Ok(None)
    }
    async fn list_configs(
        &self,
        _t: &str,
        _tid: &str,
        _p: Option<&str>,
        _ps: Option<i32>,
    ) -> Result<turul_a2a::storage::PushConfigListPage, A2aStorageError> {
        Ok(turul_a2a::storage::PushConfigListPage {
            configs: vec![],
            next_page_token: String::new(),
        })
    }
    async fn delete_config(
        &self,
        _t: &str,
        _tid: &str,
        _c: &str,
    ) -> Result<(), A2aStorageError> {
        Ok(())
    }
}

#[async_trait]
impl A2aEventStore for FakeBackend {
    fn backend_name(&self) -> &'static str {
        "fake-backend"
    }
    async fn append_event(
        &self,
        _t: &str,
        _tid: &str,
        _e: turul_a2a::streaming::StreamEvent,
    ) -> Result<u64, A2aStorageError> {
        Ok(0)
    }
    async fn get_events_after(
        &self,
        _t: &str,
        _tid: &str,
        _s: u64,
    ) -> Result<Vec<(u64, turul_a2a::streaming::StreamEvent)>, A2aStorageError> {
        Ok(vec![])
    }
    async fn latest_sequence(&self, _t: &str, _tid: &str) -> Result<u64, A2aStorageError> {
        Ok(0)
    }
    async fn cleanup_expired(&self) -> Result<u64, A2aStorageError> {
        Ok(0)
    }
}

#[async_trait]
impl A2aAtomicStore for FakeBackend {
    fn backend_name(&self) -> &'static str {
        "fake-backend"
    }
    async fn create_task_with_events(
        &self,
        _t: &str,
        _o: &str,
        task: Task,
        _e: Vec<turul_a2a::streaming::StreamEvent>,
    ) -> Result<(Task, Vec<u64>), A2aStorageError> {
        Ok((task, vec![]))
    }
    async fn update_task_status_with_events(
        &self,
        _t: &str,
        _tid: &str,
        _o: &str,
        _s: turul_a2a_types::TaskStatus,
        _e: Vec<turul_a2a::streaming::StreamEvent>,
    ) -> Result<(Task, Vec<u64>), A2aStorageError> {
        unimplemented!()
    }
    async fn update_task_with_events(
        &self,
        _t: &str,
        _o: &str,
        _task: Task,
        _e: Vec<turul_a2a::streaming::StreamEvent>,
    ) -> Result<Vec<u64>, A2aStorageError> {
        Ok(vec![])
    }
}

#[async_trait]
impl A2aCancellationSupervisor for FakeBackend {
    fn backend_name(&self) -> &'static str {
        "fake-backend"
    }
    async fn supervisor_get_cancel_requested(
        &self,
        _t: &str,
        _tid: &str,
    ) -> Result<bool, A2aStorageError> {
        Ok(false)
    }
    async fn supervisor_list_cancel_requested(
        &self,
        _t: &str,
        _tids: &[String],
    ) -> Result<Vec<String>, A2aStorageError> {
        Ok(vec![])
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

/// Unified `.storage()` with InMemoryA2aStorage — fully wires the
/// cancellation supervisor from the same backend. Builds cleanly.
#[test]
fn storage_bundle_requires_cancellation_supervisor_trait_bound() {
    // Compile-time check: .storage() accepts InMemoryA2aStorage which now
    // implements A2aCancellationSupervisor. If the trait bound on
    // `.storage()` were relaxed, a backend lacking the supervisor would
    // be accepted and silently wired — the builder test below proves
    // the runtime rejection.
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .storage(InMemoryA2aStorage::new())
        .build();
    assert!(result.is_ok(), "unified storage bundle should build");
}

/// Omitting `cancellation_supervisor` while still supplying all other
/// stores individually MUST fail at build(). No silent in-memory fallback.
#[test]
fn build_rejects_missing_cancellation_supervisor() {
    let storage = InMemoryA2aStorage::new();
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage)
        // NOTE: no .cancellation_supervisor() — this is what we're testing.
        .build();
    match result {
        Err(A2aError::Internal(msg)) => {
            assert!(
                msg.contains("cancellation_supervisor"),
                "error message should mention cancellation_supervisor: {msg}"
            );
        }
        Ok(_) => panic!("expected Internal error about missing cancellation_supervisor, got Ok(handler)"),
        Err(other) => panic!("expected Internal error, got different error: {other}"),
    }
}

/// Mismatched backend on cancellation_supervisor MUST be rejected at
/// build() with a message mentioning the mismatch. Prevents the
/// "DynamoDB marker write, in-memory supervisor read" silent failure mode.
#[test]
fn build_rejects_cancellation_supervisor_backend_mismatch() {
    let storage = InMemoryA2aStorage::new();
    let wrong_supervisor = FakeBackend;
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage)
        .cancellation_supervisor(wrong_supervisor)
        .build();
    match result {
        Err(A2aError::Internal(msg)) => {
            assert!(
                msg.contains("backend mismatch") || msg.contains("Storage backend mismatch"),
                "error should mention backend mismatch: {msg}"
            );
            assert!(
                msg.contains("cancellation_supervisor"),
                "error should mention which field mismatched: {msg}"
            );
        }
        Ok(_) => panic!("expected same-backend rejection, got Ok(handler)"),
        Err(other) => panic!("expected Internal mismatch error, got different error: {other}"),
    }
}

/// Positive test: individual setters including `.cancellation_supervisor()`
/// on the same backend accept the combination and produce a handler
/// without error.
///
/// Scope: this test proves only what `build()` returns — i.e., that the
/// same-backend check accepts the matching supervisor and that the
/// required-field validation is satisfied. It does NOT inspect
/// `AppState` directly (the `LambdaA2aHandler` wraps the router with no
/// test-only accessor). The corresponding AppState-wiring coverage lives
/// in `crates/turul-a2a/src/server/mod.rs::tests::runtime_config_setters_survive_build`
/// for the main server builder, and in `tests/cancellation_tests.rs`
/// which exercises the supervisor via the full cancel flow.
/// Together those give end-to-end proof that the Arc reaches the
/// router's handler state; this test is the compile-time + build-time
/// slice for the Lambda builder's setter surface.
#[test]
fn build_succeeds_with_explicit_cancellation_supervisor_same_backend() {
    let storage = InMemoryA2aStorage::new();
    let result = LambdaA2aHandler::builder()
        .executor(DummyExecutor)
        .task_storage(storage.clone())
        .push_storage(storage.clone())
        .event_store(storage.clone())
        .atomic_store(storage.clone())
        .cancellation_supervisor(storage)
        .build();
    assert!(
        result.is_ok(),
        "same-backend individual setters including cancellation_supervisor should build: {:?}",
        result.err()
    );
}
