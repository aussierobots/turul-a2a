//! A Lambda execution environment can freeze as soon as an invocation
//! returns. A storage request signed before the freeze and sent after the
//! thaw is rejected as expired, so a Lambda invocation leaves no work
//! running when it returns:
//!
//! - it starts no push fan-out (HTTP requests, SQS jobs and the idle-task
//!   sweep alike); the pending-dispatch marker committed with the terminal
//!   is delivered by the stream or scheduled recovery worker;
//! - an executor still running after it reported its result is aborted
//!   when the response is ready, without waiting.
//!
//! Each test holds a probe open inside the work in question and asserts
//! that no probe is open, and that none ran, once the invocation returns.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use serde_json::{Value, json};

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::push::A2aPushDeliveryStore;
use turul_a2a::server::RuntimeConfig;
use turul_a2a::storage::{
    A2aAtomicStore, A2aPushNotificationStorage, A2aStorageError, A2aTaskStorage,
    InMemoryA2aStorage, PushConfigListPage,
};
use turul_a2a_aws_lambda::LambdaA2aHandler;
use turul_a2a_proto::TaskPushNotificationConfig;
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};

/// Counts the probes currently open and the probes that ran to the end.
#[derive(Clone, Default)]
struct Probe {
    open: Arc<AtomicUsize>,
    started: Arc<AtomicUsize>,
    finished: Arc<AtomicUsize>,
}

struct ProbeGuard(Arc<AtomicUsize>);

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Probe {
    /// Stays open for `hold`, as a slow storage call would.
    async fn hold(&self, hold: Duration) {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.open.fetch_add(1, Ordering::SeqCst);
        let _guard = ProbeGuard(self.open.clone());
        tokio::time::sleep(hold).await;
        self.finished.fetch_add(1, Ordering::SeqCst);
    }

    #[track_caller]
    fn assert_none_ran(&self, what: &str) {
        assert_eq!(
            self.open.load(Ordering::SeqCst),
            0,
            "{what} pending at return"
        );
        assert_eq!(self.started.load(Ordering::SeqCst), 0, "{what} was started");
    }
}

/// Push-config storage that records every fan-out read.
struct ProbedPushConfigs {
    inner: InMemoryA2aStorage,
    probe: Probe,
}

#[async_trait]
impl A2aPushNotificationStorage for ProbedPushConfigs {
    fn backend_name(&self) -> &'static str {
        A2aPushNotificationStorage::backend_name(&self.inner)
    }
    async fn create_config(
        &self,
        tenant: &str,
        config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2aStorageError> {
        self.inner.create_config(tenant, config).await
    }
    async fn get_config(
        &self,
        tenant: &str,
        task_id: &str,
        config_id: &str,
    ) -> Result<Option<TaskPushNotificationConfig>, A2aStorageError> {
        self.inner.get_config(tenant, task_id, config_id).await
    }
    async fn list_configs(
        &self,
        tenant: &str,
        task_id: &str,
        page_token: Option<&str>,
        page_size: Option<i32>,
    ) -> Result<PushConfigListPage, A2aStorageError> {
        self.inner
            .list_configs(tenant, task_id, page_token, page_size)
            .await
    }
    async fn delete_config(
        &self,
        tenant: &str,
        task_id: &str,
        config_id: &str,
    ) -> Result<(), A2aStorageError> {
        self.inner.delete_config(tenant, task_id, config_id).await
    }
    async fn list_configs_eligible_at_event(
        &self,
        tenant: &str,
        task_id: &str,
        event_sequence: u64,
        page_token: Option<&str>,
        page_size: Option<i32>,
    ) -> Result<PushConfigListPage, A2aStorageError> {
        self.probe.hold(Duration::from_millis(100)).await;
        self.inner
            .list_configs_eligible_at_event(tenant, task_id, event_sequence, page_token, page_size)
            .await
    }
}

/// Completes the task, then optionally keeps working for `tail`, as an
/// executor finishing post-terminal storage work would.
struct CompletesThenWorks {
    probe: Probe,
    tail: Option<Duration>,
}

#[async_trait]
impl AgentExecutor for CompletesThenWorks {
    async fn execute(
        &self,
        _task: &mut Task,
        msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        if msg.joined_text() == "ask" {
            ctx.events.require_input(None).await?;
            return Ok(());
        }
        ctx.events.complete(None).await?;
        if let Some(tail) = self.tail {
            self.probe.hold(tail).await;
        }
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            name: "invocation-return".into(),
            capabilities: Some(turul_a2a_proto::AgentCapabilities::default()),
            ..Default::default()
        }
    }
}

fn push_handler(
    storage: &InMemoryA2aStorage,
    probe: &Probe,
    cfg: RuntimeConfig,
) -> LambdaA2aHandler {
    LambdaA2aHandler::builder()
        .executor(CompletesThenWorks {
            probe: Probe::default(),
            tail: None,
        })
        .storage(storage.clone())
        .push_storage(ProbedPushConfigs {
            inner: storage.clone(),
            probe: probe.clone(),
        })
        .push_delivery_store(storage.clone())
        .runtime_config(cfg)
        .build()
        .expect("lambda handler builds")
}

async fn send(handler: &LambdaA2aHandler, text: &str, task: Option<(&str, &str)>) -> Value {
    let mut message = json!({
        "messageId": uuid::Uuid::now_v7().to_string(),
        "role": "ROLE_USER",
        "parts": [{"text": text}],
    });
    if let Some((task_id, context_id)) = task {
        message["taskId"] = json!(task_id);
        message["contextId"] = json!(context_id);
    }
    let req = http::Request::builder()
        .method("POST")
        .uri("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(lambda_http::Body::Text(
            json!({ "message": message }).to_string(),
        ))
        .unwrap();
    let resp = handler.handle(req).await.expect("handle");
    let body: Value = serde_json::from_slice(resp.body().as_ref()).unwrap_or(Value::Null);
    assert_eq!(resp.status().as_u16(), 200, "{body}");
    body
}

fn assert_state(body: &Value, state: &str) {
    assert_eq!(body["task"]["status"]["state"], state, "{body}");
}

/// Register a webhook for `task_id` directly in storage.
async fn register_webhook(storage: &InMemoryA2aStorage, task_id: &str, url: &str) {
    storage
        .create_config(
            "",
            TaskPushNotificationConfig {
                tenant: String::new(),
                id: format!("{task_id}-cfg"),
                task_id: task_id.into(),
                url: url.into(),
                token: String::new(),
                authentication: None,
            },
        )
        .await
        .expect("register webhook");
}

/// The pending-dispatch marker sequences stored for `task_id`.
async fn markers(storage: &InMemoryA2aStorage, task_id: &str) -> Vec<u64> {
    storage
        .list_stale_pending_dispatches(SystemTime::now() + Duration::from_secs(60), 64)
        .await
        .expect("list markers")
        .iter()
        .filter(|p| p.task_id == task_id)
        .map(|p| p.event_sequence)
        .collect()
}

/// Give a fan-out the request might have spawned time to start.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn request_starts_no_push_fan_out_and_the_stream_worker_delivers_its_marker() {
    use turul_a2a::push::PushDispatcher;
    use turul_a2a::push::delivery::{PushDeliveryConfig, PushDeliveryWorker};
    use turul_a2a_aws_lambda::LambdaStreamRecoveryHandler;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let webhook = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&webhook)
        .await;

    let storage = InMemoryA2aStorage::new().with_push_dispatch_enabled(true);
    let probe = Probe::default();
    let handler = push_handler(&storage, &probe, RuntimeConfig::default());

    let asked = send(&handler, "ask", None).await;
    assert_state(&asked, "TASK_STATE_INPUT_REQUIRED");
    let task_id = asked["task"]["id"].as_str().unwrap().to_string();
    let context_id = asked["task"]["contextId"].as_str().unwrap().to_string();
    register_webhook(&storage, &task_id, &format!("{}/hook", webhook.uri())).await;

    let done = send(&handler, "finish", Some((&task_id, &context_id))).await;
    assert_state(&done, "TASK_STATE_COMPLETED");
    probe.assert_none_ran("push fan-out");
    settle().await;
    probe.assert_none_ran("push fan-out");
    assert!(
        webhook.received_requests().await.unwrap().is_empty(),
        "request Lambda POSTed"
    );

    // The terminal commit left its marker; the stream worker delivers it.
    let seqs = markers(&storage, &task_id).await;
    assert_eq!(seqs.len(), 1, "one marker for the terminal event: {seqs:?}");
    let mut cfg = PushDeliveryConfig::default();
    cfg.max_attempts = 1;
    cfg.allow_insecure_urls = true;
    let delivery: Arc<dyn A2aPushDeliveryStore> = Arc::new(storage.clone());
    let worker = PushDeliveryWorker::new(delivery, cfg, None, "stream-worker".into());
    let dispatcher = Arc::new(PushDispatcher::new(
        Arc::new(worker),
        Arc::new(storage.clone()),
        Arc::new(storage.clone()),
    ));
    let image: serde_dynamo::Item = serde_dynamo::to_item(json!({
        "tenant": "",
        "taskId": task_id,
        "owner": "anonymous",
        "eventSequence": seqs[0],
        "recordedAtMicros": 1,
    }))
    .unwrap();
    let mut change = aws_lambda_events::event::dynamodb::StreamRecord::default();
    change.new_image = image;
    change.sequence_number = Some("1".into());
    let mut record = aws_lambda_events::event::dynamodb::EventRecord::default();
    record.change = change;
    record.event_name = "INSERT".into();
    let mut event = aws_lambda_events::event::dynamodb::Event::default();
    event.records = vec![record];

    let resp = LambdaStreamRecoveryHandler::new(dispatcher)
        .handle_stream_event(event)
        .await;
    assert!(
        resp.batch_item_failures.is_empty(),
        "{:?}",
        resp.batch_item_failures
    );
    assert_eq!(webhook.received_requests().await.unwrap().len(), 1);
    assert!(
        markers(&storage, &task_id).await.is_empty(),
        "marker deleted after delivery"
    );
}

#[tokio::test]
async fn executor_tail_is_aborted_at_return_without_waiting() {
    let probe = Probe::default();
    let handler = LambdaA2aHandler::builder()
        .executor(CompletesThenWorks {
            probe: probe.clone(),
            tail: Some(Duration::from_secs(2)),
        })
        .storage(InMemoryA2aStorage::new())
        .build()
        .expect("lambda handler builds");
    let started = Instant::now();
    assert_state(&send(&handler, "hi", None).await, "TASK_STATE_COMPLETED");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the response waited {:?} for the executor tail",
        started.elapsed()
    );
    assert_eq!(
        probe.open.load(Ordering::SeqCst),
        0,
        "executor tail pending at return"
    );
    settle().await;
    assert_eq!(
        probe.finished.load(Ordering::SeqCst),
        0,
        "executor tail kept running after return"
    );
}

#[tokio::test]
async fn idle_sweep_starts_no_push_fan_out_and_leaves_the_marker() {
    let storage = InMemoryA2aStorage::new().with_push_dispatch_enabled(true);
    let task = Task::new("t-idle", TaskStatus::new(TaskState::Submitted)).with_context_id("ctx");
    storage.create_task("", "anonymous", task).await.unwrap();
    for state in [TaskState::Working, TaskState::InputRequired] {
        storage
            .update_task_status_with_events(
                "",
                "t-idle",
                "anonymous",
                TaskStatus::new(state),
                vec![],
            )
            .await
            .unwrap();
    }
    register_webhook(&storage, "t-idle", "http://127.0.0.1:9/hook").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let probe = Probe::default();
    let mut cfg = RuntimeConfig::default();
    cfg.idle_task_timeout = Some(Duration::from_millis(50));
    let handler = push_handler(&storage, &probe, cfg);
    let sweep = handler.cancel_idle_tasks().await.expect("sweep");
    assert_eq!(sweep.canceled, 1);
    probe.assert_none_ran("idle-sweep push fan-out");
    settle().await;
    probe.assert_none_ran("idle-sweep push fan-out");
    assert_eq!(markers(&storage, "t-idle").await.len(), 1);
}

#[cfg(feature = "sqs")]
#[tokio::test]
async fn sqs_job_starts_no_push_fan_out_and_leaves_the_marker() {
    use aws_lambda_events::event::sqs::{SqsEvent, SqsMessage};
    use turul_a2a::durable_executor::QueuedExecutorJob;

    let storage = InMemoryA2aStorage::new().with_push_dispatch_enabled(true);
    let task = Task::new("t-sqs", TaskStatus::new(TaskState::Submitted)).with_context_id("ctx");
    storage.create_task("", "anonymous", task).await.unwrap();
    storage
        .update_task_status_with_events(
            "",
            "t-sqs",
            "anonymous",
            TaskStatus::new(TaskState::Working),
            vec![],
        )
        .await
        .unwrap();
    register_webhook(&storage, "t-sqs", "http://127.0.0.1:9/hook").await;

    let probe = Probe::default();
    let handler = push_handler(&storage, &probe, RuntimeConfig::default());
    let job = QueuedExecutorJob::new(
        "",
        "anonymous",
        "t-sqs",
        "ctx",
        turul_a2a_proto::Message {
            message_id: uuid::Uuid::now_v7().to_string(),
            role: turul_a2a_proto::Role::User.into(),
            parts: vec![turul_a2a_proto::Part {
                content: Some(turul_a2a_proto::part::Content::Text("hi".into())),
                ..Default::default()
            }],
            ..Default::default()
        },
        0,
    );
    let mut record = SqsMessage::default();
    record.message_id = Some("m-1".into());
    record.body = Some(serde_json::to_string(&job).unwrap());
    record.event_source = Some("aws:sqs".into());
    let mut event = SqsEvent::default();
    event.records = vec![record];

    let resp = handler.handle_sqs(event).await;
    assert!(resp.batch_item_failures.is_empty());
    let task = storage
        .get_task("", "t-sqs", "anonymous", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        task.status().unwrap().state().unwrap(),
        TaskState::Completed
    );
    probe.assert_none_ran("SQS push fan-out");
    settle().await;
    probe.assert_none_ran("SQS push fan-out");
    assert_eq!(markers(&storage, "t-sqs").await.len(), 1);
}
