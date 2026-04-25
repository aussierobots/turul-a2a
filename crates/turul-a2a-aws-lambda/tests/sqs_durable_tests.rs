//! Lambda-crate tests (feature-gated on `sqs`).
//!
//! Covers:
//! - `classify_event` routes HTTP / SQS / unknown shapes correctly.
//! - `handle_sqs` / `drive_sqs_batch` contract:
//!     - terminal task → idempotent no-op success.
//!     - cancel marker set → CANCELED committed directly, executor
//!       never invoked, `TerminalStateAlreadySet` also returns success.
//!     - non-terminal + no cancel → executor runs to Completed.
//!     - poison record (malformed body / unknown version) → reported
//!       as a batch-item failure.
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

fn sqs_event_with_body(body: String) -> SqsEvent {
    let mut msg = SqsMessage::default();
    msg.message_id = Some(uuid::Uuid::now_v7().to_string());
    msg.body = Some(body);
    msg.event_source = Some("aws:sqs".into());
    let mut event = SqsEvent::default();
    event.records = vec![msg];
    event
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
    let (state, _storage) = state_with_executor(executor);

    // Envelope with version=999 (future / unknown).
    let body = serde_json::json!({
        "version": 999,
        "tenant": "",
        "owner": "alice",
        "taskId": "t",
        "contextId": "c",
        "message": {},
        "claims": null,
        "enqueuedAtMicros": 0,
    });
    // Note: the envelope uses serde default field names — need camelCase
    // keys matching QueuedExecutorJob's default Serialize layout.
    let body_str = serde_json::to_string(&body).unwrap();

    let resp = drive_sqs_batch(&state, sqs_event_with_body(body_str)).await;
    // Either: deserialise fails (wrong shape) OR deserialise succeeds
    // and version check fails. Both produce a batch-item failure.
    assert_eq!(resp.batch_item_failures.len(), 1);
    assert_eq!(invocations.load(Ordering::SeqCst), 0);
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
