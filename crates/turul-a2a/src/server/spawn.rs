//! Executor spawn-and-track machinery (ADR-010 §4).
//!
//! Single entry point [`spawn_tracked_executor`] that:
//!
//! 1. Builds an [`InFlightHandle`] with a placeholder JoinHandle so the
//!    executor body can capture `Arc<InFlightHandle>` via its EventSink.
//! 2. Registers the handle in [`InFlightRegistry`] before the executor
//!    runs, so the `:cancel` handler on this instance sees the live
//!    task immediately.
//! 3. Spawns the executor body on a tokio task and installs its real
//!    JoinHandle via [`InFlightHandle::set_spawned`].
//! 4. Spawns a supervisor task that awaits the executor JoinHandle and
//!    owns a [`SupervisorSentinel`] drop-guard. The sentinel removes
//!    the registry entry and aborts a still-live executor on every
//!    exit path (normal, error, panic).
//! 5. Hands back the yielded oneshot receiver to the caller, which
//!    blocking send awaits with the two-deadline timeout.
//!
//! # Post-execute detection rule (ADR-010 §7.2)
//!
//! After the executor's `execute()` returns, [`commit_post_execute`]
//! routes the final outcome through the CAS-guarded atomic store:
//!
//! - If the executor used `ctx.events` and committed a terminal, the
//!   sink is closed — nothing more to do.
//! - Otherwise, any artifacts the executor appended to `&mut Task`
//!   during execution (direct-task-mutation path) are published via
//!   the sink so they reach the durable event store.
//! - The executor's final status on `&mut Task` drives the framework
//!   commit: terminals → `sink.commit_state_internal(terminal, …)`
//!   (CAS-guarded); interrupted states → the same helper; non-terminal
//!   non-interrupted → the framework forces FAILED with a specific
//!   reason so clients don't hang on a silently-exited executor.
//! - If the executor returned `Err`, the framework commits FAILED
//!   with the error message.
//!
//! This is the invariant: every terminal transition flows through
//! `A2aAtomicStore::update_task_status_with_events`, regardless of
//! whether the executor used the sink directly or mutated `&mut Task`.

use std::sync::Arc;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use turul_a2a_types::{Artifact, Message, Task, TaskState};

use crate::error::A2aError;
use crate::event_sink::EventSink;
use crate::executor::{AgentExecutor, ExecutionContext};
use crate::server::in_flight::{InFlightHandle, InFlightKey, InFlightRegistry, SupervisorSentinel};
use crate::storage::{A2aAtomicStore, A2aTaskStorage};
use crate::streaming::TaskEventBroker;

/// Shared spawn-path dependencies pulled out of `AppState` to keep the
/// call sites in `router.rs` / `jsonrpc.rs` ergonomic.
#[derive(Clone)]
pub(crate) struct SpawnDeps {
    pub executor: Arc<dyn AgentExecutor>,
    pub task_storage: Arc<dyn A2aTaskStorage>,
    pub atomic_store: Arc<dyn A2aAtomicStore>,
    pub event_broker: TaskEventBroker,
    pub in_flight: Arc<InFlightRegistry>,
    /// Push-delivery dispatcher (ADR-011 §2). Threaded into every
    /// sink constructed here so executor-emitted terminal status
    /// commits fan out to registered push configs.
    pub push_dispatcher: Option<Arc<crate::push::PushDispatcher>>,
}

/// Per-spawn scope: identifies the task whose executor we are spawning
/// and the message that triggered it.
pub(crate) struct SpawnScope {
    pub tenant: String,
    pub owner: String,
    pub task_id: String,
    pub context_id: String,
    pub message: Message,
}

/// Result of a successful spawn. The caller awaits `yielded_rx`
/// (blocking send) or drops it (non-blocking / streaming send).
pub(crate) struct SpawnResult {
    pub handle: Arc<InFlightHandle>,
    pub yielded_rx: oneshot::Receiver<Task>,
    pub cancellation: CancellationToken,
}

/// Spawn the executor on a tracked handle.
///
/// Fails with `A2aError::Internal` if an in-flight entry already exists
/// for `(tenant, task_id)` — that would indicate a framework bug
/// (double-spawn for the same task) which must not silently overwrite
/// a live handle.
pub(crate) fn spawn_tracked_executor(
    deps: SpawnDeps,
    scope: SpawnScope,
) -> Result<SpawnResult, A2aError> {
    let key: InFlightKey = (scope.tenant.clone(), scope.task_id.clone());

    let cancellation = CancellationToken::new();
    let (yielded_tx, yielded_rx) = oneshot::channel::<Task>();

    // Placeholder JoinHandle — replaced below once the real executor
    // task is spawned. The noop finishes immediately; we don't await
    // it, we just use it as a valid JoinHandle to satisfy the current
    // `InFlightHandle::new` contract until `set_spawned` overwrites.
    let placeholder_jh = tokio::spawn(async {});

    let handle = Arc::new(InFlightHandle::new(
        cancellation.clone(),
        yielded_tx,
        placeholder_jh,
    ));

    // Register BEFORE spawn so an immediate `:cancel` on this instance
    // can trip the token even if the executor hasn't started running yet.
    deps.in_flight.try_insert(key.clone(), handle.clone())
        .map_err(|collision| {
            A2aError::Internal(format!(
                "double-spawn for in-flight task: {collision}"
            ))
        })?;

    // Build the live sink and capture the pieces the executor task
    // needs. The sink holds Arc<InFlightHandle> so `fire_yielded` on
    // terminal/interrupted commit goes through this handle's oneshot.
    let sink = EventSink::new(
        scope.tenant.clone(),
        scope.task_id.clone(),
        scope.context_id.clone(),
        scope.owner.clone(),
        deps.atomic_store.clone(),
        deps.task_storage.clone(),
        deps.event_broker.clone(),
        handle.clone(),
        deps.push_dispatcher.clone(),
    );

    let exec_deps = deps.clone();
    let exec_scope_tenant = scope.tenant.clone();
    let exec_scope_owner = scope.owner.clone();
    let exec_scope_task_id = scope.task_id.clone();
    let exec_scope_context_id = scope.context_id.clone();
    let exec_scope_message = scope.message.clone();
    let exec_cancellation = cancellation.clone();
    let exec_sink = sink.clone();

    let executor_jh = tokio::spawn(async move {
        run_executor_body(
            exec_deps,
            exec_sink,
            exec_cancellation,
            exec_scope_tenant,
            exec_scope_owner,
            exec_scope_task_id,
            exec_scope_context_id,
            exec_scope_message,
        )
        .await;
    });

    handle.set_spawned(executor_jh);

    // Supervisor: holds the sentinel that removes the registry entry
    // and aborts a still-live executor on every exit path.
    let sup_registry = deps.in_flight.clone();
    let sup_key = key;
    let sup_handle = handle.clone();
    tokio::spawn(async move {
        let _sentinel = SupervisorSentinel::new(sup_registry, sup_key, sup_handle.clone());
        if let Some(jh) = sup_handle.take_spawned() {
            let _ = jh.await;
        }
        // sentinel drops here, running cleanup.
    });

    Ok(SpawnResult {
        handle,
        yielded_rx,
        cancellation,
    })
}

/// Body of the spawned executor task. Loads the task, runs
/// `execute()`, applies the §7.2 detection rule.
#[allow(clippy::too_many_arguments)]
async fn run_executor_body(
    deps: SpawnDeps,
    sink: EventSink,
    cancellation: CancellationToken,
    tenant: String,
    owner: String,
    task_id: String,
    context_id: String,
    message: Message,
) {
    let mut task = match deps
        .task_storage
        .get_task(&tenant, &task_id, &owner, None)
        .await
    {
        Ok(Some(t)) => t,
        Ok(None) => {
            // Task vanished between the create and the spawn body —
            // commit FAILED to the sink so any awaiter gets a response.
            let _ = sink
                .commit_state_internal(
                    TaskState::Failed,
                    Some(agent_text(
                        "framework: task not found when executor body started",
                    )),
                )
                .await;
            return;
        }
        Err(e) => {
            let _ = sink
                .commit_state_internal(
                    TaskState::Failed,
                    Some(agent_text(&format!(
                        "framework: failed to load task in executor body: {e}"
                    ))),
                )
                .await;
            return;
        }
    };

    // Per-index (artifact_id, part_count) snapshot pre-execute. The
    // post-execute detection rule diffs this against the final task
    // state to emit `ArtifactUpdate` events for anything the executor
    // appended or extended via direct-task-mutation. Index-based (not
    // id-based) so executors that extend the parts of an existing
    // artifact — the streaming-chunk pattern under direct mutation —
    // are observed as append=true emits, not silently dropped.
    let start_artifact_sigs: Vec<(String, usize)> = task
        .artifacts()
        .iter()
        .map(|a| (a.artifact_id.clone(), a.parts.len()))
        .collect();

    let ctx = ExecutionContext {
        owner,
        tenant: if tenant.is_empty() {
            None
        } else {
            Some(tenant.clone())
        },
        task_id: task_id.clone(),
        context_id: Some(context_id),
        claims: None,
        cancellation,
        events: sink.clone(),
    };

    let result = deps.executor.execute(&mut task, &message, &ctx).await;

    commit_post_execute(&sink, &task, &start_artifact_sigs, result).await;
}

/// ADR-010 §7.2 detection rule.
///
/// Routes direct-task-mutation executors through the CAS-guarded
/// atomic-store paths so every terminal transition is observable
/// through the same contract that sink users already honour. Artifact
/// diffs found on `&mut Task` are published before the final status
/// commit so streaming subscribers see the artifacts before the
/// terminal event.
pub(crate) async fn commit_post_execute(
    sink: &EventSink,
    task: &Task,
    start_artifact_sigs: &[(String, usize)],
    execute_result: Result<(), A2aError>,
) {
    if sink.is_closed() {
        // Executor drove its own lifecycle via the sink. Nothing to do.
        return;
    }

    // Diff the per-index (id, part_count) signature to publish any
    // artifacts the executor added or extended via direct mutation of
    // `&mut Task`:
    //
    // - index beyond the pre-execute length → brand-new artifact,
    //   emit with `append=false`.
    // - same index, same id, more parts → parts appended in place,
    //   emit the tail with `append=true` so storage extends the
    //   existing artifact's parts (matching the streaming-chunk
    //   semantics of [`EventSink::emit_artifact`]).
    // - any other mutation pattern at an existing index (id replaced
    //   in place, parts truncated) is anti-pattern for direct-
    //   mutation executors; such executors should drive artifacts
    //   through `ctx.events.emit_artifact` instead. This diff does
    //   not try to reproduce those mutations.
    for (i, pb_artifact) in task.artifacts().iter().enumerate() {
        let current_id = &pb_artifact.artifact_id;
        let current_len = pb_artifact.parts.len();
        match start_artifact_sigs.get(i) {
            None => {
                let wrapper: Artifact = pb_artifact.clone().into();
                let _ = sink.emit_artifact(wrapper, false, true).await;
            }
            Some((prev_id, prev_len)) if prev_id == current_id && current_len > *prev_len => {
                // Parts appended to the same artifact — emit only
                // the tail as an `append=true` chunk so storage does
                // not duplicate the original parts.
                let mut tail = pb_artifact.clone();
                tail.parts = tail.parts.split_off(*prev_len);
                let wrapper: Artifact = tail.into();
                let _ = sink.emit_artifact(wrapper, true, true).await;
            }
            Some(_) => {
                // Unchanged, truncated, or identity-swapped at this
                // index — see the comment above; the detection rule
                // does not publish those cases.
            }
        }
        if sink.is_closed() {
            return;
        }
    }

    match execute_result {
        Err(e) => {
            // Executor returned Err — framework commits FAILED with the
            // error text so the client sees a real reason.
            let _ = sink
                .commit_state_internal(
                    TaskState::Failed,
                    Some(agent_text(&format!("executor error: {e}"))),
                )
                .await;
        }
        Ok(()) => {
            let state_opt = task.status().and_then(|s| s.state().ok());
            let message_opt = task
                .status()
                .and_then(|s| s.as_proto().message.clone())
                .and_then(|pb_msg| Message::try_from(pb_msg).ok());

            match state_opt {
                Some(s) if turul_a2a_types::state_machine::is_terminal(s) => {
                    // Terminal via direct-task-mutation: route through
                    // the CAS-guarded helper. The message (if the
                    // executor set one) is preserved verbatim.
                    let _ = sink.commit_state_internal(s, message_opt).await;
                }
                Some(TaskState::InputRequired) | Some(TaskState::AuthRequired) => {
                    // Interrupted via direct-task-mutation. Same path;
                    // the sink fires yielded so a blocking caller
                    // observes the interrupt.
                    let _ = sink
                        .commit_state_internal(state_opt.unwrap(), message_opt)
                        .await;
                }
                Some(TaskState::Submitted) | Some(TaskState::Working) | None => {
                    // Executor returned without reaching a terminal or
                    // interrupted state AND without using the sink.
                    // Force FAILED so the client doesn't hang.
                    let _ = sink
                        .commit_state_internal(
                            TaskState::Failed,
                            Some(agent_text(
                                "executor returned without reaching terminal \
                                 state and without emitting events",
                            )),
                        )
                        .await;
                }
                Some(_) => {
                    // Unknown future TaskState variant. Safe default:
                    // force FAILED with a descriptive reason.
                    let _ = sink
                        .commit_state_internal(
                            TaskState::Failed,
                            Some(agent_text(
                                "executor returned with an unrecognised task \
                                 state; framework forced FAILED",
                            )),
                        )
                        .await;
                }
            }
        }
    }
}

fn agent_text(text: &str) -> Message {
    Message::new(
        uuid::Uuid::now_v7().to_string(),
        turul_a2a_types::Role::Agent,
        vec![turul_a2a_types::Part::text(text.to_string())],
    )
}
