//! Lambda-crate tests (feature-gated on `sqs`).
//!
//! Covers:
//! - `classify_event` routes HTTP / SQS / unknown shapes correctly.
//! - `handle_sqs` / `drive_sqs_batch` contract:
//!     - terminal task → idempotent no-op success; the same record
//!       delivered twice runs the executor once.
//!     - cancel marker set → CANCELED committed directly, executor
//!       never invoked; losing the CANCELED commit to a concurrent
//!       terminal (`TerminalStateAlreadySet`) is still success.
//!     - non-terminal + no cancel → executor runs to Completed, seeing
//!       the envelope's claims.
//!     - two concurrent deliveries of one record: both run, one
//!       terminal commit wins, both succeed.
//!     - executor error → task FAILED with the error text; the record
//!       is acknowledged (a retry would find the task terminal).
//!     - task not found, malformed body, unknown envelope version →
//!       batch-item failure naming only that record.
//! - `SqsDurableExecutorQueue::max_payload_bytes == 256 * 1024`.

#![cfg(feature = "sqs")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use aws_lambda_events::event::sqs::{SqsEvent, SqsMessage};
use turul_a2a::durable_executor::QueuedExecutorJob;
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::router::AppState;
use turul_a2a::server::RuntimeConfig;
use turul_a2a::server::in_flight::InFlightRegistry;
use turul_a2a::storage::{A2aTaskStorage, InMemoryA2aStorage};
use turul_a2a::streaming::TaskEventBroker;
use turul_a2a_aws_lambda::{LambdaEvent, classify_event, drive_sqs_batch};
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct SpyExecutor {
    invocations: Arc<AtomicUsize>,
}

impl SpyExecutor {
    fn new() -> (Arc<Self>, Arc<AtomicUsize>) {
        let invocations = Arc::new(AtomicUsize::new(0));
        let ex = Arc::new(Self {
            invocations: invocations.clone(),
        });
        (ex, invocations)
    }
}

#[async_trait]
impl AgentExecutor for SpyExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        let mut proto = task.as_proto().clone();
        proto.status = Some(turul_a2a_proto::TaskStatus {
            state: turul_a2a_proto::TaskState::Completed.into(),
            message: None,
            timestamp: None,
        });
        *task = Task::try_from(proto).unwrap();
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

fn state_with_executor(executor: Arc<dyn AgentExecutor>) -> (AppState, Arc<InMemoryA2aStorage>) {
    let s = Arc::new(InMemoryA2aStorage::new());
    let state = AppState {
        executor,
        task_storage: s.clone(),
        push_storage: s.clone(),
        event_store: s.clone(),
        atomic_store: s.clone(),
        event_broker: TaskEventBroker::new(),
        middleware_stack: Arc::new(turul_a2a::middleware::MiddlewareStack::new(vec![])),
        runtime_config: RuntimeConfig::default(),
        in_flight: Arc::new(InFlightRegistry::new()),
        cancellation_supervisor: s.clone(),
        push_delivery_store: None,
        push_dispatcher: None,
        durable_executor_queue: None,
    };
    (state, s)
}

/// Helper: pre-create a task in Submitted → Working so `drive_sqs_batch`
/// finds it on dequeue.
async fn seed_task(
    storage: &InMemoryA2aStorage,
    tenant: &str,
    owner: &str,
    task_id: &str,
) -> String {
    let context_id = uuid::Uuid::now_v7().to_string();
    let task =
        Task::new(task_id, TaskStatus::new(TaskState::Submitted)).with_context_id(&context_id);
    let submitted_event = turul_a2a::streaming::StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: task_id.to_string(),
            context_id: context_id.clone(),
            status: serde_json::to_value(TaskStatus::new(TaskState::Submitted)).unwrap(),
        },
    };
    use turul_a2a::storage::A2aAtomicStore;
    storage
        .create_task_with_events(tenant, owner, task, vec![submitted_event])
        .await
        .unwrap();
    let working_event = turul_a2a::streaming::StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: task_id.to_string(),
            context_id: context_id.clone(),
            status: serde_json::to_value(TaskStatus::new(TaskState::Working)).unwrap(),
        },
    };
    storage
        .update_task_status_with_events(
            tenant,
            task_id,
            owner,
            TaskStatus::new(TaskState::Working),
            vec![working_event],
        )
        .await
        .unwrap();
    context_id
}

fn sqs_record(message_id: &str, body: String) -> SqsMessage {
    let mut msg = SqsMessage::default();
    msg.message_id = Some(message_id.into());
    msg.body = Some(body);
    msg.event_source = Some("aws:sqs".into());
    msg
}

fn sqs_event(records: Vec<SqsMessage>) -> SqsEvent {
    let mut event = SqsEvent::default();
    event.records = records;
    event
}

fn sqs_event_with_body(body: String) -> SqsEvent {
    sqs_event(vec![sqs_record(&uuid::Uuid::now_v7().to_string(), body)])
}

fn failed_ids(resp: &aws_lambda_events::event::sqs::SqsBatchResponse) -> Vec<String> {
    resp.batch_item_failures
        .iter()
        .map(|f| f.item_identifier.clone())
        .collect()
}

async fn task_state(storage: &InMemoryA2aStorage, owner: &str, task_id: &str) -> TaskState {
    storage
        .get_task("", task_id, owner, None)
        .await
        .unwrap()
        .expect("task")
        .status()
        .unwrap()
        .state()
        .unwrap()
}

fn status_text(task: &Task) -> String {
    task.status()
        .unwrap()
        .as_proto()
        .message
        .as_ref()
        .map(|m| {
            m.parts
                .iter()
                .filter_map(|p| match &p.content {
                    Some(turul_a2a_proto::part::Content::Text(t)) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<String>()
        })
        .unwrap_or_default()
}

fn job_for(tenant: &str, owner: &str, task_id: &str, context_id: &str) -> QueuedExecutorJob {
    QueuedExecutorJob {
        version: QueuedExecutorJob::VERSION,
        tenant: tenant.into(),
        owner: owner.into(),
        task_id: task_id.into(),
        context_id: context_id.into(),
        message: turul_a2a_proto::Message {
            message_id: uuid::Uuid::now_v7().to_string(),
            role: turul_a2a_proto::Role::User.into(),
            parts: vec![turul_a2a_proto::Part {
                content: Some(turul_a2a_proto::part::Content::Text("hi".into())),
                ..Default::default()
            }],
            ..Default::default()
        },
        claims: None,
        accepted_output_modes: vec![],
        request_metadata: None,
        activated_extensions: vec![],
        enqueued_at_micros: 0,
    }
}

// ---------------------------------------------------------------------------
// classify_event
// ---------------------------------------------------------------------------

#[test]
fn classify_event_http_shape_returns_http() {
    let event = serde_json::json!({
        "httpMethod": "POST",
        "path": "/message:send",
        "body": "",
    });
    assert_eq!(classify_event(&event), LambdaEvent::Http);
}

#[test]
fn classify_event_sqs_shape_returns_sqs() {
    let event = serde_json::json!({
        "Records": [{"eventSource": "aws:sqs", "body": "{}"}]
    });
    assert_eq!(classify_event(&event), LambdaEvent::Sqs);
}

#[test]
fn classify_event_unknown_shape_returns_unknown() {
    let event = serde_json::json!({"foo": "bar"});
    assert_eq!(classify_event(&event), LambdaEvent::Unknown);
}

#[test]
fn classify_event_dynamodb_stream_shape_is_unknown_not_sqs() {
    let event = serde_json::json!({
        "Records": [{"eventSource": "aws:dynamodb", "eventName": "INSERT"}]
    });
    assert_eq!(classify_event(&event), LambdaEvent::Unknown);
}

// ---------------------------------------------------------------------------
// drive_sqs_batch — terminal task is no-op success
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sqs_terminal_task_is_idempotent_noop() {
    let (executor, invocations) = SpyExecutor::new();
    let (state, storage) = state_with_executor(executor);
    let tenant = "";
    let owner = "alice";
    let task_id = "task-terminal";
    let context_id = seed_task(&storage, tenant, owner, task_id).await;

    // Terminalise the task before SQS fires.
    let completed_event = turul_a2a::streaming::StreamEvent::StatusUpdate {
        status_update: turul_a2a::streaming::StatusUpdatePayload {
            task_id: task_id.into(),
            context_id: context_id.clone(),
            status: serde_json::to_value(TaskStatus::new(TaskState::Completed)).unwrap(),
        },
    };
    use turul_a2a::storage::A2aAtomicStore;
    storage
        .update_task_status_with_events(
            tenant,
            task_id,
            owner,
            TaskStatus::new(TaskState::Completed),
            vec![completed_event],
        )
        .await
        .unwrap();

    let job = job_for(tenant, owner, task_id, &context_id);
    let body = serde_json::to_string(&job).unwrap();
    let resp = drive_sqs_batch(&state, sqs_event_with_body(body)).await;
    assert!(resp.batch_item_failures.is_empty(), "no retries");
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "executor must not run"
    );
}

// ---------------------------------------------------------------------------
// drive_sqs_batch — cancel marker → CANCELED committed, executor never runs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sqs_cancel_marker_commits_canceled_without_executor() {
    let (executor, invocations) = SpyExecutor::new();
    let (state, storage) = state_with_executor(executor);
    let tenant = "";
    let owner = "alice";
    let task_id = "task-cancel";
    let context_id = seed_task(&storage, tenant, owner, task_id).await;

    // Set cancel marker BEFORE SQS dispatch.
    use turul_a2a::storage::A2aTaskStorage;
    storage
        .set_cancel_requested(tenant, task_id, owner)
        .await
        .unwrap();

    let job = job_for(tenant, owner, task_id, &context_id);
    let body = serde_json::to_string(&job).unwrap();
    let resp = drive_sqs_batch(&state, sqs_event_with_body(body)).await;
    assert!(
        resp.batch_item_failures.is_empty(),
        "cancel path returns success"
    );
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "executor must never run when cancel marker is set pre-dispatch"
    );

    // Task must be CANCELED now, with the reason message.
    let task = storage
        .get_task(tenant, task_id, owner, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status().unwrap().state().unwrap(), TaskState::Canceled);
    let msg = task
        .status()
        .unwrap()
        .as_proto()
        .message
        .clone()
        .expect("CANCELED reason");
    let text = msg
        .parts
        .iter()
        .filter_map(|p| match &p.content {
            Some(turul_a2a_proto::part::Content::Text(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        text.contains("canceled before durable executor dispatch"),
        "reason text: {text}"
    );
}

// ---------------------------------------------------------------------------
// drive_sqs_batch — non-terminal + no cancel → executor runs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sqs_non_terminal_no_cancel_runs_executor_to_completion() {
    let (executor, invocations) = SpyExecutor::new();
    let (state, storage) = state_with_executor(executor);
    let tenant = "";
    let owner = "bob";
    let task_id = "task-run";
    let context_id = seed_task(&storage, tenant, owner, task_id).await;

    let job = job_for(tenant, owner, task_id, &context_id);
    let body = serde_json::to_string(&job).unwrap();
    let resp = drive_sqs_batch(&state, sqs_event_with_body(body)).await;
    assert!(resp.batch_item_failures.is_empty());
    assert_eq!(invocations.load(Ordering::SeqCst), 1);

    let task = storage
        .get_task(tenant, task_id, owner, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        task.status().unwrap().state().unwrap(),
        TaskState::Completed
    );
}

// ---------------------------------------------------------------------------
// drive_sqs_batch — unknown envelope version is a batch-item failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sqs_unknown_envelope_version_reports_batch_item_failure() {
    let (executor, invocations) = SpyExecutor::new();
    let (state, storage) = state_with_executor(executor);
    let owner = "alice";
    let task_id = "task-version";
    let context_id = seed_task(&storage, "", owner, task_id).await;

    // A valid envelope for a runnable task, with only the version
    // changed.
    let job = job_for("", owner, task_id, &context_id);
    let mut tampered = serde_json::to_value(&job).unwrap();
    tampered["version"] = serde_json::Value::from(QueuedExecutorJob::VERSION + 1);

    let resp = drive_sqs_batch(
        &state,
        sqs_event(vec![sqs_record("rec-future", tampered.to_string())]),
    )
    .await;
    assert_eq!(failed_ids(&resp), vec!["rec-future".to_string()]);
    assert_eq!(invocations.load(Ordering::SeqCst), 0);
    assert_eq!(
        task_state(&storage, owner, task_id).await,
        TaskState::Working
    );

    // The untouched envelope is accepted — the version was the only
    // difference.
    let resp = drive_sqs_batch(
        &state,
        sqs_event(vec![sqs_record(
            "rec-current",
            serde_json::to_string(&job).unwrap(),
        )]),
    )
    .await;
    assert!(failed_ids(&resp).is_empty());
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn sqs_same_record_twice_runs_the_executor_once() {
    let (executor, invocations) = SpyExecutor::new();
    let (state, storage) = state_with_executor(executor);
    let owner = "alice";
    let task_id = "task-redelivered";
    let context_id = seed_task(&storage, "", owner, task_id).await;
    let body = serde_json::to_string(&job_for("", owner, task_id, &context_id)).unwrap();

    for _ in 0..2 {
        let resp =
            drive_sqs_batch(&state, sqs_event(vec![sqs_record("rec-1", body.clone())])).await;
        assert!(failed_ids(&resp).is_empty());
    }
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    assert_eq!(
        task_state(&storage, owner, task_id).await,
        TaskState::Completed
    );
}

/// Records the claims the executor observes, then completes.
struct ClaimsSpyExecutor {
    seen: std::sync::Mutex<Option<serde_json::Value>>,
}

#[async_trait]
impl AgentExecutor for ClaimsSpyExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        *self.seen.lock().unwrap() = ctx.claims.clone();
        let mut proto = task.as_proto().clone();
        proto.status = Some(turul_a2a_proto::TaskStatus {
            state: turul_a2a_proto::TaskState::Completed.into(),
            message: None,
            timestamp: None,
        });
        *task = Task::try_from(proto).unwrap();
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

#[tokio::test]
async fn sqs_path_delivers_envelope_claims_to_executor() {
    let executor = Arc::new(ClaimsSpyExecutor {
        seen: std::sync::Mutex::new(None),
    });
    let (state, storage) = state_with_executor(executor.clone());
    let owner = "alice";
    let task_id = "task-claims";
    let context_id = seed_task(&storage, "", owner, task_id).await;
    let claims = serde_json::json!({"sub": "alice", "scope": "a2a:write"});
    let mut job = job_for("", owner, task_id, &context_id);
    job.claims = Some(claims.clone());

    let resp = drive_sqs_batch(
        &state,
        sqs_event_with_body(serde_json::to_string(&job).unwrap()),
    )
    .await;
    assert!(failed_ids(&resp).is_empty());
    assert_eq!(*executor.seen.lock().unwrap(), Some(claims));
}

#[tokio::test]
async fn sqs_task_not_found_is_a_batch_item_failure() {
    let (executor, invocations) = SpyExecutor::new();
    let (state, _storage) = state_with_executor(executor);
    let body = serde_json::to_string(&job_for("", "alice", "task-missing", "ctx")).unwrap();
    let resp = drive_sqs_batch(&state, sqs_event(vec![sqs_record("rec-missing", body)])).await;
    assert_eq!(failed_ids(&resp), vec!["rec-missing".to_string()]);
    assert_eq!(invocations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn sqs_mixed_batch_reports_only_the_poison_record() {
    let (executor, invocations) = SpyExecutor::new();
    let (state, storage) = state_with_executor(executor);
    let owner = "alice";
    let task_id = "task-good";
    let context_id = seed_task(&storage, "", owner, task_id).await;
    let good = serde_json::to_string(&job_for("", owner, task_id, &context_id)).unwrap();

    let resp = drive_sqs_batch(
        &state,
        sqs_event(vec![
            sqs_record("rec-good", good),
            sqs_record("rec-poison", "{not json".into()),
        ]),
    )
    .await;
    assert_eq!(failed_ids(&resp), vec!["rec-poison".to_string()]);
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    assert_eq!(
        task_state(&storage, owner, task_id).await,
        TaskState::Completed
    );
}

struct FailingExecutor;

#[async_trait]
impl AgentExecutor for FailingExecutor {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        Err(A2aError::Internal("model backend unavailable".into()))
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

/// An executor error is committed as the task's FAILED terminal and
/// the record is acknowledged: a redelivery would only find the task
/// terminal and do nothing.
#[tokio::test]
async fn sqs_executor_error_fails_the_task_and_acknowledges_the_record() {
    let (state, storage) = state_with_executor(Arc::new(FailingExecutor));
    let owner = "alice";
    let task_id = "task-exec-error";
    let context_id = seed_task(&storage, "", owner, task_id).await;
    let body = serde_json::to_string(&job_for("", owner, task_id, &context_id)).unwrap();

    let resp = drive_sqs_batch(&state, sqs_event(vec![sqs_record("rec-err", body)])).await;
    assert!(failed_ids(&resp).is_empty());
    let task = storage
        .get_task("", task_id, owner, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status().unwrap().state().unwrap(), TaskState::Failed);
    assert_eq!(
        status_text(&task),
        "executor error: Internal error: model backend unavailable"
    );
}

/// Atomic store whose CANCELED commit loses a race: a competing writer
/// commits COMPLETED first, so the CANCELED write meets a terminal task.
struct CompletedWinsRaceStore {
    inner: Arc<InMemoryA2aStorage>,
}

#[async_trait]
impl turul_a2a::storage::A2aAtomicStore for CompletedWinsRaceStore {
    fn backend_name(&self) -> &'static str {
        turul_a2a::storage::A2aAtomicStore::backend_name(self.inner.as_ref())
    }
    async fn create_task_with_events(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
        events: Vec<turul_a2a::streaming::StreamEvent>,
    ) -> Result<(Task, Vec<u64>), turul_a2a::storage::A2aStorageError> {
        self.inner
            .create_task_with_events(tenant, owner, task, events)
            .await
    }
    async fn update_task_status_with_events(
        &self,
        tenant: &str,
        task_id: &str,
        owner: &str,
        new_status: TaskStatus,
        events: Vec<turul_a2a::streaming::StreamEvent>,
    ) -> Result<(Task, Vec<u64>), turul_a2a::storage::A2aStorageError> {
        if new_status.state().ok() == Some(TaskState::Canceled) {
            let completed = TaskStatus::new(TaskState::Completed);
            let event = turul_a2a::streaming::StreamEvent::StatusUpdate {
                status_update: turul_a2a::streaming::StatusUpdatePayload {
                    task_id: task_id.into(),
                    context_id: String::new(),
                    status: serde_json::to_value(&completed).unwrap(),
                },
            };
            self.inner
                .update_task_status_with_events(tenant, task_id, owner, completed, vec![event])
                .await?;
        }
        self.inner
            .update_task_status_with_events(tenant, task_id, owner, new_status, events)
            .await
    }
    async fn update_task_with_events(
        &self,
        tenant: &str,
        owner: &str,
        task: Task,
        events: Vec<turul_a2a::streaming::StreamEvent>,
    ) -> Result<Vec<u64>, turul_a2a::storage::A2aStorageError> {
        self.inner
            .update_task_with_events(tenant, owner, task, events)
            .await
    }
}

#[tokio::test]
async fn sqs_cancel_losing_to_a_concurrent_terminal_is_success() {
    let (executor, invocations) = SpyExecutor::new();
    let (mut state, storage) = state_with_executor(executor);
    state.atomic_store = Arc::new(CompletedWinsRaceStore {
        inner: storage.clone(),
    });
    let owner = "alice";
    let task_id = "task-cancel-race";
    let context_id = seed_task(&storage, "", owner, task_id).await;
    storage
        .set_cancel_requested("", task_id, owner)
        .await
        .unwrap();

    let body = serde_json::to_string(&job_for("", owner, task_id, &context_id)).unwrap();
    let resp = drive_sqs_batch(&state, sqs_event(vec![sqs_record("rec-race", body)])).await;
    assert!(failed_ids(&resp).is_empty(), "no retry, no DLQ");
    assert_eq!(invocations.load(Ordering::SeqCst), 0);
    assert_eq!(
        task_state(&storage, owner, task_id).await,
        TaskState::Completed,
        "the competing terminal stands"
    );
}

/// Waits until both concurrent deliveries are inside `execute`, then
/// completes; so both invocations load the task while it is WORKING.
struct RendezvousExecutor {
    barrier: tokio::sync::Barrier,
    invocations: AtomicUsize,
}

#[async_trait]
impl AgentExecutor for RendezvousExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        self.barrier.wait().await;
        let mut proto = task.as_proto().clone();
        proto.status = Some(turul_a2a_proto::TaskStatus {
            state: turul_a2a_proto::TaskState::Completed.into(),
            message: None,
            timestamp: None,
        });
        *task = Task::try_from(proto).unwrap();
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

#[tokio::test]
async fn sqs_concurrent_duplicate_commits_one_terminal_and_both_succeed() {
    let executor = Arc::new(RendezvousExecutor {
        barrier: tokio::sync::Barrier::new(2),
        invocations: AtomicUsize::new(0),
    });
    let (state, storage) = state_with_executor(executor.clone());
    let owner = "alice";
    let task_id = "task-dup";
    let context_id = seed_task(&storage, "", owner, task_id).await;
    let body = serde_json::to_string(&job_for("", owner, task_id, &context_id)).unwrap();

    let (a, b) = tokio::join!(
        drive_sqs_batch(&state, sqs_event(vec![sqs_record("rec-a", body.clone())])),
        drive_sqs_batch(&state, sqs_event(vec![sqs_record("rec-b", body.clone())])),
    );
    assert!(failed_ids(&a).is_empty() && failed_ids(&b).is_empty());
    assert_eq!(executor.invocations.load(Ordering::SeqCst), 2);
    assert_eq!(
        task_state(&storage, owner, task_id).await,
        TaskState::Completed
    );

    use turul_a2a::storage::A2aEventStore;
    let terminal_events = storage
        .get_events_after("", task_id, 0)
        .await
        .unwrap()
        .into_iter()
        .filter(|(_, e)| e.is_terminal())
        .count();
    assert_eq!(terminal_events, 1, "exactly one terminal commit wins");
}

// ---------------------------------------------------------------------------
// SqsDurableExecutorQueue — static contract
// ---------------------------------------------------------------------------

#[test]
fn sqs_durable_executor_queue_max_payload_bytes_is_256kib() {
    // Construct with a dummy URL; no network call for this static check.
    // We use a real aws_sdk_sqs::Client — it doesn't hit the network
    // at construction (only on send_message).
    let config = aws_sdk_sqs::Config::builder()
        .region(aws_sdk_sqs::config::Region::new("us-east-1"))
        .behavior_version(aws_sdk_sqs::config::BehaviorVersion::latest())
        .build();
    let client = Arc::new(aws_sdk_sqs::Client::from_conf(config));
    let queue = turul_a2a_aws_lambda::SqsDurableExecutorQueue::new(
        "https://sqs.us-east-1.amazonaws.com/000000000000/fake-queue",
        client,
    );
    use turul_a2a::durable_executor::DurableExecutorQueue;
    assert_eq!(queue.max_payload_bytes(), 256 * 1024);
    assert_eq!(queue.kind(), "sqs");
}

// ---------------------------------------------------------------------------
// drive_sqs_batch — request-level inputs reach the executor
// ---------------------------------------------------------------------------

/// `(accepted_output_modes, request_metadata, activated_extensions)` per
/// execution.
type SeenInputs = Arc<std::sync::Mutex<Vec<(Vec<String>, Option<serde_json::Value>, Vec<String>)>>>;

/// Records the request-level inputs the executor observed.
struct InputsSpy {
    seen: SeenInputs,
}

#[async_trait]
impl AgentExecutor for InputsSpy {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        self.seen.lock().unwrap().push((
            ctx.accepted_output_modes.clone(),
            ctx.request_metadata.clone(),
            ctx.activated_extensions.clone(),
        ));
        ctx.events.complete(None).await?;
        Ok(())
    }
    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard::default()
    }
}

#[tokio::test]
async fn sqs_job_delivers_request_inputs_to_executor() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (state, storage) = state_with_executor(Arc::new(InputsSpy { seen: seen.clone() }));
    let (tenant, owner, task_id) = ("", "carol", "task-inputs");
    let context_id = seed_task(&storage, tenant, owner, task_id).await;

    let mut job = job_for(tenant, owner, task_id, &context_id);
    job.accepted_output_modes = vec!["application/json".into()];
    job.request_metadata = Some(serde_json::json!({"traceId": "sqs-1"}));
    job.activated_extensions = vec!["https://example.test/ext/a".into()];
    let body = serde_json::to_string(&job).unwrap();
    let resp = drive_sqs_batch(&state, sqs_event_with_body(body)).await;
    assert!(resp.batch_item_failures.is_empty());

    assert_eq!(
        *seen.lock().unwrap(),
        vec![(
            vec!["application/json".to_string()],
            Some(serde_json::json!({"traceId": "sqs-1"})),
            vec!["https://example.test/ext/a".to_string()],
        )]
    );
}
