use turul_a2a_types::{A2aTypeError, TaskState};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum A2aStorageError {
    #[error("Task not found: {0}")]
    TaskNotFound(String),

    #[error("Invalid state transition: {current:?} -> {requested:?}")]
    InvalidTransition {
        current: TaskState,
        requested: TaskState,
    },

    #[error("Task is in terminal state: {0:?}")]
    TerminalState(TaskState),

    /// The atomic store's single-terminal-writer invariant rejected a
    /// terminal write because the task was already in a terminal state
    /// at the time of the CAS. Distinct from [`Self::TerminalState`]
    /// which is the raw state-machine signal: `TerminalStateAlreadySet`
    /// is specifically the "you lost the race" signal emitted by
    /// [`crate::storage::A2aAtomicStore::update_task_status_with_events`].
    /// Callers typically translate this to HTTP 409 `TaskNotCancelable` /
    /// JSON-RPC `-32002` on the wire, and to `EventSink::closed` semantics
    /// when the caller is an executor sink.
    ///
    /// The `current_state` field carries the wire enum name (e.g.
    /// `"TASK_STATE_COMPLETED"`) so log/telemetry consumers can see
    /// exactly which terminal won the race.
    #[error("task {task_id} already in terminal state {current_state} (CAS loser)")]
    TerminalStateAlreadySet {
        task_id: String,
        current_state: String,
    },

    #[error("Owner mismatch for task: {task_id}")]
    OwnerMismatch { task_id: String },

    #[error("Tenant mismatch for task: {task_id}")]
    TenantMismatch { task_id: String },

    #[error("Concurrent modification: {0}")]
    ConcurrentModification(String),

    #[error("Push notification config not found: {0}")]
    PushConfigNotFound(String),

    /// The push delivery claim for a `(tenant, task_id, event_sequence,
    /// config_id)` tuple is already held by another instance whose claim
    /// has not yet expired, OR the tuple has already reached a terminal
    /// outcome (`Succeeded`, `GaveUp`, `Abandoned`) and cannot be
    /// re-claimed regardless of expiry.
    ///
    /// Returned only by
    /// [`crate::push::A2aPushDeliveryStore::claim_delivery`]. Callers
    /// treat this as "skip delivery on this instance" — the event is
    /// already being (or has already been) handled.
    #[error(
        "push delivery claim already held: tenant={tenant} task_id={task_id} \
         event_sequence={event_sequence} config_id={config_id}"
    )]
    ClaimAlreadyHeld {
        tenant: String,
        task_id: String,
        event_sequence: u64,
        config_id: String,
    },

    /// The claim identity passed to
    /// [`crate::push::A2aPushDeliveryStore::record_delivery_outcome`]
    /// does not match the currently-stored claim for this tuple.
    /// Two causes: the claim expired and another instance re-claimed
    /// (generation advanced), or the same instance's prior process
    /// died and the restarted process holds a different `claimant`
    /// identifier. Either way, the stale caller's outcome is
    /// dropped so it cannot overwrite a terminal state committed by
    /// the current claimant.
    ///
    /// Workers that receive this error MUST abort their retry loop
    /// for the affected tuple — the current claimant (or whoever
    /// re-claims next) owns the remaining lifecycle.
    #[error(
        "stale push delivery claim: tenant={tenant} task_id={task_id} \
         event_sequence={event_sequence} config_id={config_id} — recorded \
         outcome dropped because the claim was re-acquired by another \
         claimant or generation"
    )]
    StaleDeliveryClaim {
        tenant: String,
        task_id: String,
        event_sequence: u64,
        config_id: String,
    },

    /// / §6.4: `create_config` exhausted its bounded
    /// retry budget (default 5 attempts with 10/50/250/1000 ms
    /// backoff) while its CAS against `a2a_tasks.latest_event_sequence`
    /// kept losing to concurrent event commits. The operator should
    /// retry the create from the handler; in practice this surfaces
    /// only under pathological event-burst workloads against a single
    /// task.
    #[error(
        "create_config CAS exhausted retries for tenant={tenant} task_id={task_id}: \
         concurrent event commits kept advancing latest_event_sequence"
    )]
    CreateConfigCasTimeout { tenant: String, task_id: String },

    #[error("Database error: {0}")]
    DatabaseError(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),

    #[error("Type error: {0}")]
    TypeError(#[from] A2aTypeError),

    #[error("Generic storage error: {0}")]
    Generic(String),
}
