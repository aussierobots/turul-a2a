//! Executor-facing `EventSink` — ADR-010 §2.
//!
//! The sink is the **sole executor-side write boundary** for task lifecycle
//! events. Every call routes through [`crate::storage::A2aAtomicStore`]:
//!
//! - Status transitions (working / interrupted / terminal) go through
//!   [`A2aAtomicStore::update_task_status_with_events`] — CAS-guarded per
//!   ADR-010 §7.1 so concurrent terminal writes resolve to exactly one
//!   winner.
//! - Artifact emits go through [`A2aAtomicStore::update_task_with_events`].
//!   An artifact append is not a state transition, so it does not contend
//!   on the terminal-CAS path that status emits use. Two correctness
//!   guarantees protect the artifact read-mutate-write:
//!   (i) per-sink serialisation via an async `commit_lock`, which
//!   prevents two cloned-sink emits from racing and losing each other's
//!   merges; and (ii) the atomic store's terminal-preservation
//!   ConditionExpression / `WHERE` clause, which rejects the write if a
//!   terminal was committed between the read and the commit. Together
//!   they close both the lost-update and terminal-rollback windows
//!   without adding a new atomic primitive.
//!
//! # Invariants
//!
//! 1. **No `update_task_with_events` terminal bypass.** Terminal emits
//!    (`complete`, `fail`, `cancelled`, `reject`) ALWAYS go through
//!    `update_task_status_with_events`. If a future refactor wants to
//!    bundle artifacts + terminal into one write, it must extend the
//!    terminal-CAS contract to cover that path.
//!
//! 2. **Yielded fires only on durable commit.** The sink calls
//!    [`crate::server::in_flight::InFlightHandle::fire_yielded`] only
//!    after the atomic store returns `Ok`. A commit that fails (CAS loss,
//!    storage error) never wakes the blocking-send awaiter, so the
//!    awaiter observes the actual persisted terminal or its own timeout.
//!
//! 3. **Sink closes on terminal commit.** After the first successful
//!    terminal commit, `is_closed()` returns true and subsequent emits
//!    return `A2aError::InvalidRequest { message: "EventSink is closed..." }`.
//!    A CAS loss (another writer committed first) also closes the sink.
//!
//! 4. **Emits on a single sink are serialised.** All `emit_*` and
//!    `set_status` methods acquire an async mutex on the sink's
//!    internal state for the read-mutate-write critical section
//!    that commits to storage. This eliminates the lost-update window
//!    in `emit_artifact`'s full-task replacement: two concurrent
//!    artifact emits on the same task from cloned sink handles
//!    serialise instead of racing, so each emit sees the post-commit
//!    snapshot of its predecessor. The lock is per-sink — concurrent
//!    sinks on different tasks do not contend.
//!
//! 5. **Detached sink for tests / anonymous contexts.** `EventSink::detached`
//!    creates a no-runtime handle whose emits always fail with `Internal`.
//!    This keeps `ExecutionContext::anonymous` workable in executor unit
//!    tests without requiring them to stand up storage.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Mutex as AsyncMutex;
use turul_a2a_types::{Artifact, Message, TaskState, TaskStatus};

use crate::error::A2aError;
use crate::server::in_flight::InFlightHandle;
use crate::storage::{A2aAtomicStore, A2aStorageError, A2aTaskStorage};
use crate::streaming::{ArtifactUpdatePayload, StatusUpdatePayload, StreamEvent, TaskEventBroker};

/// Executor-side handle for emitting task lifecycle events durably.
///
/// See the module-level docs for invariants. Cloneable — clones share the
/// same closed-flag and in-flight handle. The typical shape is one sink
/// per spawned executor, handed to the executor via
/// [`crate::executor::ExecutionContext::events`].
#[derive(Clone)]
pub struct EventSink {
    inner: Option<Arc<EventSinkInner>>,
}

struct EventSinkInner {
    tenant: String,
    task_id: String,
    context_id: String,
    owner: String,
    atomic_store: Arc<dyn A2aAtomicStore>,
    task_storage: Arc<dyn A2aTaskStorage>,
    event_broker: TaskEventBroker,
    handle: Arc<InFlightHandle>,
    is_closed: AtomicBool,
    /// Serialises every `commit_*` path on this sink so concurrent
    /// calls from cloned handles cannot race. Without it,
    /// `commit_artifact`'s read-mutate-write full-replacement path
    /// could lose a concurrently-committed non-terminal update, and
    /// two artifact emits targeting the same task could discard each
    /// other's merges. The lock is per-sink; sinks for different
    /// tasks do not contend.
    commit_lock: AsyncMutex<()>,
    /// Push-delivery dispatcher (ADR-011 §2). `None` on deployments
    /// that don't run push delivery. When set, the sink calls
    /// `dispatch` after every successful status commit — terminal
    /// events fan out to registered push configs; non-terminal events
    /// are filtered out inside the dispatcher.
    push_dispatcher: Option<Arc<crate::push::PushDispatcher>>,
}

impl EventSink {
    /// Construct a live sink bound to a spawned executor. Called
    /// internally by the framework's tracked-executor spawn path — not
    /// part of the public API for agent authors, who receive a
    /// pre-wired sink via [`crate::executor::ExecutionContext::events`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        tenant: String,
        task_id: String,
        context_id: String,
        owner: String,
        atomic_store: Arc<dyn A2aAtomicStore>,
        task_storage: Arc<dyn A2aTaskStorage>,
        event_broker: TaskEventBroker,
        handle: Arc<InFlightHandle>,
        push_dispatcher: Option<Arc<crate::push::PushDispatcher>>,
    ) -> Self {
        Self {
            inner: Some(Arc::new(EventSinkInner {
                tenant,
                task_id,
                context_id,
                owner,
                atomic_store,
                task_storage,
                event_broker,
                handle,
                is_closed: AtomicBool::new(false),
                commit_lock: AsyncMutex::new(()),
                push_dispatcher,
            })),
        }
    }

    /// A sink with no backing runtime. Emits return `A2aError::Internal`.
    ///
    /// Used by [`crate::executor::ExecutionContext::anonymous`] so executor
    /// unit tests that construct a context without a server still get a
    /// `Clone`-able, `Send`-safe placeholder. Production paths never hand a
    /// detached sink to an executor.
    pub fn detached() -> Self {
        Self { inner: None }
    }

    /// True after the first terminal commit (own or lost-CAS).
    pub fn is_closed(&self) -> bool {
        match &self.inner {
            Some(inner) => inner.is_closed.load(Ordering::Acquire),
            None => true,
        }
    }

    /// Emit a status transition and persist a `StatusUpdate` event.
    ///
    /// Rejects terminal states — use [`Self::complete`], [`Self::fail`],
    /// [`Self::cancelled`], or [`Self::reject`] instead. Use this method
    /// for non-terminal transitions including `WORKING`, `INPUT_REQUIRED`,
    /// and `AUTH_REQUIRED`.
    pub async fn set_status(
        &self,
        state: TaskState,
        message: Option<Message>,
    ) -> Result<u64, A2aError> {
        if turul_a2a_types::state_machine::is_terminal(state) {
            return Err(A2aError::InvalidRequest {
                message: format!(
                    "set_status does not accept terminal state {state:?}; use complete/fail/cancelled/reject"
                ),
            });
        }
        self.inner()?.commit_status(state, message).await
    }

    /// Emit an artifact. `append=true` appends to an existing artifact's
    /// parts; `append=false` creates or replaces. `last_chunk=true` signals
    /// the final chunk for streaming transports (ADR-006 transport-level).
    pub async fn emit_artifact(
        &self,
        artifact: Artifact,
        append: bool,
        last_chunk: bool,
    ) -> Result<u64, A2aError> {
        self.inner()?
            .commit_artifact(artifact, append, last_chunk)
            .await
    }

    /// Emit terminal `COMPLETED` and close the sink.
    pub async fn complete(&self, final_message: Option<Message>) -> Result<u64, A2aError> {
        self.inner()?
            .commit_status(TaskState::Completed, final_message)
            .await
    }

    /// Emit terminal `FAILED` with an optional reason. The reason, if any,
    /// is wrapped as an agent-authored text message on the task status
    /// (framework telemetry is distinct from `fail` triggered by framework
    /// timeout — see ADR-010 §4.1 and ADR-012 §8).
    pub async fn fail(&self, reason: Option<String>) -> Result<u64, A2aError> {
        let message = reason.map(text_message_from_agent);
        self.inner()?
            .commit_status(TaskState::Failed, message)
            .await
    }

    /// Emit terminal `CANCELED` with an optional reason. Usually called by
    /// executors that cooperatively observe `ctx.cancellation.is_cancelled()`.
    pub async fn cancelled(&self, reason: Option<String>) -> Result<u64, A2aError> {
        let message = reason.map(text_message_from_agent);
        self.inner()?
            .commit_status(TaskState::Canceled, message)
            .await
    }

    /// Emit terminal `REJECTED` (policy refusal / guardrail) with a reason.
    /// Distinct from `FAILED` on the wire per ADR-010 §1.
    pub async fn reject(&self, reason: Option<String>) -> Result<u64, A2aError> {
        let message = reason.map(text_message_from_agent);
        self.inner()?
            .commit_status(TaskState::Rejected, message)
            .await
    }

    /// Emit interrupted `INPUT_REQUIRED`. The sink stays open; the client
    /// supplies another message on the same task to resume.
    pub async fn require_input(&self, prompt: Option<Message>) -> Result<u64, A2aError> {
        self.inner()?
            .commit_status(TaskState::InputRequired, prompt)
            .await
    }

    /// Emit interrupted `AUTH_REQUIRED`.
    pub async fn require_auth(&self, challenge: Option<Message>) -> Result<u64, A2aError> {
        self.inner()?
            .commit_status(TaskState::AuthRequired, challenge)
            .await
    }

    /// Framework-internal commit helper. Bypasses the public API's
    /// terminal rejection on `set_status` so the framework's
    /// post-execute detection rule can commit whatever final state the
    /// executor left on `&mut Task`, whether that is a terminal, an
    /// interrupted state, or a framework-forced FAILED. Goes through
    /// the same CAS-guarded path as the public terminal methods.
    /// Not part of the executor-facing surface.
    pub(crate) async fn commit_state_internal(
        &self,
        state: TaskState,
        message: Option<Message>,
    ) -> Result<u64, A2aError> {
        self.inner()?.commit_status(state, message).await
    }

    fn inner(&self) -> Result<&Arc<EventSinkInner>, A2aError> {
        self.inner.as_ref().ok_or_else(|| {
            A2aError::Internal(
                "EventSink is detached (no framework runtime); \
                 construct via A2aServer or LambdaA2aHandler"
                    .into(),
            )
        })
    }
}

impl std::fmt::Debug for EventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            Some(inner) => f
                .debug_struct("EventSink")
                .field("tenant", &inner.tenant)
                .field("task_id", &inner.task_id)
                .field("is_closed", &inner.is_closed.load(Ordering::Relaxed))
                .finish(),
            None => f.write_str("EventSink(detached)"),
        }
    }
}

impl EventSinkInner {
    async fn commit_status(
        &self,
        state: TaskState,
        message: Option<Message>,
    ) -> Result<u64, A2aError> {
        // Serialise commits on this sink — see `EventSinkInner::commit_lock`.
        let _guard = self.commit_lock.lock().await;

        if self.is_closed.load(Ordering::Acquire) {
            return Err(A2aError::InvalidRequest {
                message: "EventSink is closed".into(),
            });
        }

        let mut status = TaskStatus::new(state);
        if let Some(m) = message {
            status = status.with_message(m);
        }

        let event = StreamEvent::StatusUpdate {
            status_update: StatusUpdatePayload {
                task_id: self.task_id.clone(),
                context_id: self.context_id.clone(),
                status: serde_json::to_value(&status).unwrap_or_default(),
            },
        };

        let result = self
            .atomic_store
            .update_task_status_with_events(
                &self.tenant,
                &self.task_id,
                &self.owner,
                status,
                vec![event],
            )
            .await;

        match result {
            Ok((task, seqs)) => {
                let seq = seqs.first().copied().unwrap_or(0);
                let is_terminal = turul_a2a_types::state_machine::is_terminal(state);
                let is_interrupted =
                    matches!(state, TaskState::InputRequired | TaskState::AuthRequired);

                // Push delivery fan-out (ADR-011 §2). The dispatcher
                // filters for terminal status events internally; we
                // can call it on every status commit without worrying
                // about misfires on non-terminals.
                if let Some(dispatcher) = &self.push_dispatcher {
                    // Re-synthesise the single committed event so the
                    // dispatcher can inspect it. We already hold
                    // `status` + `state`, so serialisation cannot
                    // fail.
                    let ev = StreamEvent::StatusUpdate {
                        status_update: StatusUpdatePayload {
                            task_id: self.task_id.clone(),
                            context_id: self.context_id.clone(),
                            status: serde_json::to_value(turul_a2a_types::TaskStatus::new(state))
                                .unwrap_or_default(),
                        },
                    };
                    dispatcher.dispatch(
                        self.tenant.clone(),
                        self.owner.clone(),
                        task.clone(),
                        vec![(seq, ev)],
                    );
                }

                if is_terminal {
                    self.is_closed.store(true, Ordering::Release);
                }
                if is_terminal || is_interrupted {
                    self.handle.fire_yielded(task);
                }
                self.event_broker.notify(&self.task_id).await;
                Ok(seq)
            }
            Err(A2aStorageError::TerminalStateAlreadySet { current_state, .. }) => {
                self.is_closed.store(true, Ordering::Release);
                Err(A2aError::InvalidRequest {
                    message: format!("EventSink is closed: terminal already set ({current_state})"),
                })
            }
            Err(other) => Err(A2aError::from(other)),
        }
    }

    async fn commit_artifact(
        &self,
        artifact: Artifact,
        append: bool,
        last_chunk: bool,
    ) -> Result<u64, A2aError> {
        // Serialise commits on this sink so the read-mutate-write
        // sequence below cannot interleave with another emit on a
        // cloned handle — see `EventSinkInner::commit_lock`.
        let _guard = self.commit_lock.lock().await;

        if self.is_closed.load(Ordering::Acquire) {
            return Err(A2aError::InvalidRequest {
                message: "EventSink is closed".into(),
            });
        }

        // Read-mutate-write: pull current task, merge the artifact, commit
        // via update_task_with_events. The atomic store enforces the
        // terminal-preservation CAS (ADR-010 §7.1 extension) — if a
        // concurrent writer committed a terminal between the read and
        // this write, the commit rejects with `TerminalStateAlreadySet`
        // and no state is mutated. Lost-update safety against
        // non-terminal writes comes from `commit_lock`.
        let mut task = self
            .task_storage
            .get_task(&self.tenant, &self.task_id, &self.owner, None)
            .await
            .map_err(A2aError::from)?
            .ok_or_else(|| A2aError::TaskNotFound {
                task_id: self.task_id.clone(),
            })?;

        task.merge_artifact(artifact.clone(), append, last_chunk);

        let event = StreamEvent::ArtifactUpdate {
            artifact_update: ArtifactUpdatePayload {
                task_id: self.task_id.clone(),
                context_id: self.context_id.clone(),
                artifact: serde_json::to_value(&artifact).unwrap_or_default(),
                append,
                last_chunk,
            },
        };

        let result = self
            .atomic_store
            .update_task_with_events(&self.tenant, &self.owner, task, vec![event])
            .await;

        match result {
            Ok(seqs) => {
                let seq = seqs.first().copied().unwrap_or(0);
                self.event_broker.notify(&self.task_id).await;
                Ok(seq)
            }
            Err(A2aStorageError::TerminalStateAlreadySet { current_state, .. }) => {
                // Race: a concurrent writer committed a terminal between
                // our get_task and our update_task_with_events. Close the
                // sink and surface a consistent "sink closed" error
                // rather than the generic TaskNotCancelable that the
                // From impl uses for cancel-handler callers.
                self.is_closed.store(true, Ordering::Release);
                Err(A2aError::InvalidRequest {
                    message: format!("EventSink is closed: terminal already set ({current_state})"),
                })
            }
            Err(other) => Err(A2aError::from(other)),
        }
    }
}

/// Build an agent-authored text Message for framework-authored terminal
/// reasons (`fail`, `cancelled`, `reject`). The message is tagged with a
/// fresh UUIDv7 id; callers never need to construct one themselves.
fn text_message_from_agent(reason: String) -> Message {
    Message::new(
        uuid::Uuid::now_v7().to_string(),
        turul_a2a_types::Role::Agent,
        vec![turul_a2a_types::Part::text(reason)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detached_sink_is_closed_by_default() {
        let sink = EventSink::detached();
        assert!(sink.is_closed(), "detached sink reports closed");
    }

    #[tokio::test]
    async fn detached_sink_status_emit_returns_internal_error() {
        let sink = EventSink::detached();
        let err = sink
            .set_status(TaskState::Working, None)
            .await
            .expect_err("detached sink must not accept emits");
        assert!(matches!(err, A2aError::Internal(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn detached_sink_terminal_emit_returns_internal_error() {
        let sink = EventSink::detached();
        assert!(matches!(
            sink.complete(None).await,
            Err(A2aError::Internal(_))
        ));
        assert!(matches!(
            sink.fail(Some("boom".into())).await,
            Err(A2aError::Internal(_))
        ));
        assert!(matches!(
            sink.cancelled(None).await,
            Err(A2aError::Internal(_))
        ));
        assert!(matches!(
            sink.reject(Some("policy".into())).await,
            Err(A2aError::Internal(_))
        ));
    }

    #[tokio::test]
    async fn set_status_rejects_terminal_states() {
        // Use detached sink: the terminal-rejection path runs BEFORE the
        // inner() access, so detachment doesn't mask the validation.
        let sink = EventSink::detached();
        for terminal in [
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::Rejected,
        ] {
            let err = sink
                .set_status(terminal, None)
                .await
                .expect_err("terminal via set_status must be rejected");
            match err {
                A2aError::InvalidRequest { message } => {
                    assert!(
                        message.contains("does not accept terminal"),
                        "msg: {message}"
                    );
                }
                other => panic!("expected InvalidRequest, got {other:?}"),
            }
        }
    }

    #[test]
    fn text_message_from_agent_carries_reason() {
        let msg = text_message_from_agent("scheduled retry".into());
        assert_eq!(msg.text_parts(), vec!["scheduled retry"]);
        assert_eq!(msg.as_proto().role, i32::from(turul_a2a_proto::Role::Agent));
    }

    // =========================================================
    // Live-sink coverage — drives an EventSink against
    // InMemoryA2aStorage + a real InFlightHandle. Proves the
    // invariants tested in the review:
    // - successful terminal commit closes the sink
    // - yielded fires only after durable commit and carries the
    //   persisted task
    // - CAS loss closes the sink without firing yielded
    // - broker notification happens after commit
    // - artifact append semantics match storage parity
    // =========================================================

    use crate::server::in_flight::InFlightHandle;
    use crate::storage::InMemoryA2aStorage;
    use turul_a2a_types::{Artifact, Part, Task};

    /// Test harness wiring. Returns a live sink, the backing storage (for
    /// direct assertions), the in-flight handle (for yielded observation),
    /// and the yielded receiver.
    async fn harness() -> (
        EventSink,
        Arc<InMemoryA2aStorage>,
        Arc<InFlightHandle>,
        tokio::sync::oneshot::Receiver<Task>,
        TaskEventBroker,
    ) {
        let storage = Arc::new(InMemoryA2aStorage::new());
        let broker = TaskEventBroker::new();

        // Seed a WORKING task owned by "owner-1" in tenant "t-1".
        let task = Task::new("task-sink-1", TaskStatus::new(TaskState::Submitted))
            .with_context_id("ctx-sink-1");
        storage
            .create_task_with_events("t-1", "owner-1", task, vec![])
            .await
            .expect("seed task");
        storage
            .update_task_status_with_events(
                "t-1",
                "task-sink-1",
                "owner-1",
                TaskStatus::new(TaskState::Working),
                vec![],
            )
            .await
            .expect("advance to WORKING");

        // Build an InFlightHandle with a fresh yielded oneshot. The
        // spawned JoinHandle here is a detached noop — the sink never
        // touches `spawned`, so a synthetic one is fine.
        let (yielded_tx, yielded_rx) = tokio::sync::oneshot::channel::<Task>();
        let noop_join = tokio::spawn(async {});
        let handle = Arc::new(InFlightHandle::new(
            tokio_util::sync::CancellationToken::new(),
            yielded_tx,
            noop_join,
        ));

        let task_storage: Arc<dyn crate::storage::A2aTaskStorage> = storage.clone();
        let atomic_store: Arc<dyn A2aAtomicStore> = storage.clone();
        let sink = EventSink::new(
            "t-1".into(),
            "task-sink-1".into(),
            "ctx-sink-1".into(),
            "owner-1".into(),
            atomic_store,
            task_storage,
            broker.clone(),
            handle.clone(),
            None,
        );

        (sink, storage, handle, yielded_rx, broker)
    }

    #[tokio::test]
    async fn live_sink_complete_closes_sink_fires_yielded_and_notifies_broker() {
        let (sink, storage, handle, yielded_rx, broker) = harness().await;

        // Attach a broker subscriber before emitting.
        let mut wake_rx = broker.subscribe("task-sink-1").await;

        assert!(!sink.is_closed(), "pre-emit sink is open");
        assert!(!handle.yielded_fired(), "yielded not fired yet");

        let seq = sink
            .complete(Some(Message::new(
                "m-done",
                turul_a2a_types::Role::Agent,
                vec![Part::text("ok")],
            )))
            .await
            .expect("complete must succeed against fresh WORKING task");
        assert!(seq > 0, "terminal commit returned a sequence number");

        assert!(sink.is_closed(), "complete closes the sink");
        assert!(handle.yielded_fired(), "yielded fires on terminal commit");

        // The yielded receiver got the persisted task — its state is
        // COMPLETED, matching what we just committed.
        let yielded_task = tokio::time::timeout(std::time::Duration::from_millis(200), yielded_rx)
            .await
            .expect("yielded fires promptly")
            .expect("yielded sender not dropped");
        assert_eq!(
            yielded_task.status().unwrap().state().unwrap(),
            TaskState::Completed,
            "yielded carries the persisted terminal"
        );

        // Broker wake-up arrived after the commit.
        tokio::time::timeout(std::time::Duration::from_millis(200), wake_rx.recv())
            .await
            .expect("broker notify should arrive after commit")
            .expect("broker channel not closed");

        // Storage confirms the persisted state.
        let persisted = storage
            .get_task("t-1", "task-sink-1", "owner-1", None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            persisted.status().unwrap().state().unwrap(),
            TaskState::Completed
        );
    }

    #[tokio::test]
    async fn live_sink_emit_after_terminal_is_rejected_in_memory() {
        let (sink, _storage, _handle, _yielded_rx, _broker) = harness().await;
        sink.complete(None).await.expect("first terminal succeeds");

        // A subsequent artifact emit must short-circuit on the in-memory
        // closed flag — no storage round-trip needed.
        let err = sink
            .emit_artifact(Artifact::new("a-1", vec![Part::text("late")]), false, true)
            .await
            .expect_err("emits after terminal must fail");
        match err {
            A2aError::InvalidRequest { message } => {
                assert!(message.contains("EventSink is closed"), "msg: {message}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }

        // A subsequent set_status also fails closed.
        let err = sink
            .set_status(TaskState::Working, None)
            .await
            .expect_err("set_status after terminal must fail");
        assert!(matches!(err, A2aError::InvalidRequest { .. }));
    }

    #[tokio::test]
    async fn live_sink_terminal_cas_loss_closes_sink_without_firing_yielded() {
        let (sink, storage, handle, mut yielded_rx, _broker) = harness().await;

        // Simulate another writer winning the terminal CAS first — e.g.,
        // the framework's cancel handler committing CANCELED.
        storage
            .update_task_status_with_events(
                "t-1",
                "task-sink-1",
                "owner-1",
                TaskStatus::new(TaskState::Canceled),
                vec![],
            )
            .await
            .expect("framework cancel commit wins");

        assert!(!handle.yielded_fired(), "pre-sink yielded not fired");

        // Now the executor tries to emit COMPLETED. The atomic store
        // returns TerminalStateAlreadySet; the sink maps it to
        // InvalidRequest and closes WITHOUT firing yielded.
        let err = sink
            .complete(None)
            .await
            .expect_err("losing-CAS terminal must not Ok()");
        match err {
            A2aError::InvalidRequest { message } => {
                assert!(message.contains("terminal already set"), "msg: {message}");
                assert!(
                    message.contains("TASK_STATE_CANCELED"),
                    "error reports persisted terminal: {message}"
                );
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }

        assert!(sink.is_closed(), "CAS loss closes the sink");
        assert!(
            !handle.yielded_fired(),
            "yielded must NOT fire on CAS loss — the awaiter is driven by \
             the winning writer's own commit hook"
        );

        // yielded_rx is still live; immediately polling it returns empty.
        match yielded_rx.try_recv() {
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            other => panic!("yielded must remain un-fired on CAS loss, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn live_sink_emit_artifact_append_merges_by_id() {
        let (sink, storage, _handle, _yielded_rx, _broker) = harness().await;

        // First chunk — append=false establishes the artifact.
        sink.emit_artifact(
            Artifact::new("a-1", vec![Part::text("chunk-1 ")]),
            false,
            false,
        )
        .await
        .expect("first chunk");

        // Second chunk — append=true, same id, extends parts instead of
        // duplicating the artifact.
        sink.emit_artifact(
            Artifact::new("a-1", vec![Part::text("chunk-2")]),
            true,
            true,
        )
        .await
        .expect("second chunk");

        let persisted = storage
            .get_task("t-1", "task-sink-1", "owner-1", None)
            .await
            .unwrap()
            .unwrap();
        let artifacts = persisted.artifacts();
        assert_eq!(artifacts.len(), 1, "append=true must not duplicate by id");
        assert_eq!(
            artifacts[0].parts.len(),
            2,
            "append=true extends parts on same-id artifact"
        );
    }

    #[tokio::test]
    async fn live_sink_concurrent_artifact_emits_do_not_lose_updates() {
        // Two cloned sink handles each emit a distinct artifact
        // concurrently. Without the per-sink commit lock, the second
        // commit's full-task replacement can clobber the first
        // (read-mutate-write interleave). With the lock, emits
        // serialise and both artifacts persist.
        let (sink, storage, _handle, _yielded_rx, _broker) = harness().await;
        let sink_a = sink.clone();
        let sink_b = sink.clone();

        let emit_a = tokio::spawn(async move {
            sink_a
                .emit_artifact(
                    Artifact::new("a-A", vec![Part::text("payload-A")]),
                    false,
                    true,
                )
                .await
        });
        let emit_b = tokio::spawn(async move {
            sink_b
                .emit_artifact(
                    Artifact::new("a-B", vec![Part::text("payload-B")]),
                    false,
                    true,
                )
                .await
        });

        let (res_a, res_b) = tokio::try_join!(emit_a, emit_b).expect("join");
        res_a.expect("emit A succeeds");
        res_b.expect("emit B succeeds");

        let persisted = storage
            .get_task("t-1", "task-sink-1", "owner-1", None)
            .await
            .unwrap()
            .unwrap();
        let mut ids: Vec<String> = persisted
            .artifacts()
            .iter()
            .map(|a| a.artifact_id.clone())
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["a-A".to_string(), "a-B".to_string()],
            "both concurrent emits must persist — no lost update"
        );
    }

    #[tokio::test]
    async fn live_sink_emit_artifact_after_terminal_is_rejected_by_storage() {
        // Prove the storage-layer terminal-preservation CAS kicks in even
        // if the in-memory closed flag somehow missed the terminal (e.g.,
        // a fresh sink created AFTER the terminal was committed
        // externally — no prior commit on THIS sink instance).
        let (_seed_sink, storage, _handle, _yielded_rx, broker) = harness().await;

        // Commit CANCELED directly through storage.
        storage
            .update_task_status_with_events(
                "t-1",
                "task-sink-1",
                "owner-1",
                TaskStatus::new(TaskState::Canceled),
                vec![],
            )
            .await
            .expect("external cancel wins");

        // Construct a brand-new sink — its in-memory closed flag starts
        // false, so the check lands at the storage layer.
        let (yielded_tx2, _yielded_rx2) = tokio::sync::oneshot::channel::<Task>();
        let noop_join2 = tokio::spawn(async {});
        let handle2 = Arc::new(InFlightHandle::new(
            tokio_util::sync::CancellationToken::new(),
            yielded_tx2,
            noop_join2,
        ));
        let task_storage2: Arc<dyn crate::storage::A2aTaskStorage> = storage.clone();
        let atomic_store2: Arc<dyn A2aAtomicStore> = storage.clone();
        let late_sink = EventSink::new(
            "t-1".into(),
            "task-sink-1".into(),
            "ctx-sink-1".into(),
            "owner-1".into(),
            atomic_store2,
            task_storage2,
            broker,
            handle2.clone(),
            None,
        );
        assert!(!late_sink.is_closed());

        let err = late_sink
            .emit_artifact(Artifact::new("a-x", vec![Part::text("late")]), false, true)
            .await
            .expect_err("artifact emit against terminal task must fail");

        // `commit_artifact` explicitly maps `TerminalStateAlreadySet`
        // from the atomic store to `A2aError::InvalidRequest` with a
        // "EventSink is closed: terminal already set (<wire name>)"
        // message. Assert that exact error class and message — a
        // looser match could hide a regression where the generic
        // `From<A2aStorageError>` path (which maps to
        // `TaskNotCancelable`) sneaks back in.
        match err {
            A2aError::InvalidRequest { message } => {
                assert!(
                    message.contains("EventSink is closed: terminal already set"),
                    "message should be the sink-closed translation: {message}"
                );
                assert!(
                    message.contains("TASK_STATE_CANCELED"),
                    "message should name the persisted terminal in wire form: {message}"
                );
            }
            other => panic!(
                "artifact emit on a terminal task must surface as \
                 InvalidRequest (not the generic TaskNotCancelable), got {other:?}"
            ),
        }

        assert!(
            late_sink.is_closed(),
            "storage-layer terminal-preservation CAS must close the sink"
        );
        assert!(
            !handle2.yielded_fired(),
            "artifact emit never fires yielded"
        );
    }
}
