//! A Lambda execution environment can freeze as soon as an invocation
//! returns, so no storage or push-dispatch request this invocation started
//! may still be pending when `LambdaA2aHandler::handle` returns. A request
//! signed before a freeze and sent after the thaw fails with an expired
//! signature.
//!
//! Each test holds a probe open inside work the request leaves running
//! after its response (the push fan-out, or an executor's tail after its
//! terminal), and asserts that no probe is open once `handle` returns.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
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
    finished: Arc<AtomicUsize>,
}

struct ProbeGuard(Arc<AtomicUsize>);

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Probe {
    /// Stays open for `hold` (forever when `None`), as a slow storage
    /// call would.
    async fn hold(&self, hold: Option<Duration>) {
        self.open.fetch_add(1, Ordering::SeqCst);
        let _guard = ProbeGuard(self.open.clone());
        match hold {
            Some(d) => tokio::time::sleep(d).await,
            None => std::future::pending::<()>().await,
        }
        self.finished.fetch_add(1, Ordering::SeqCst);
    }

    fn open(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    fn finished(&self) -> usize {
        self.finished.load(Ordering::SeqCst)
    }
}

/// Push-config storage whose fan-out read is slow.
struct SlowPushConfigs {
    inner: InMemoryA2aStorage,
    probe: Probe,
    hold: Option<Duration>,
}

#[async_trait]
impl A2aPushNotificationStorage for SlowPushConfigs {
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
        self.probe.hold(self.hold).await;
        self.inner
            .list_configs_eligible_at_event(tenant, task_id, event_sequence, page_token, page_size)
            .await
    }
}

/// Completes the task, then keeps working for `tail` (forever when
/// `None`), as an executor finishing post-terminal storage work would.
struct CompletesThenWorks {
    probe: Probe,
    tail: Option<Option<Duration>>,
}

#[async_trait]
impl AgentExecutor for CompletesThenWorks {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        ctx.events.complete(None).await?;
        if let Some(tail) = self.tail {
            self.probe.hold(tail).await;
        }
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            name: "settle".into(),
            capabilities: Some(turul_a2a_proto::AgentCapabilities::default()),
            ..Default::default()
        }
    }
}

const GRACE: Duration = Duration::from_millis(300);

fn runtime_config() -> RuntimeConfig {
    let mut cfg = RuntimeConfig::default();
    cfg.timeout_abort_grace = GRACE;
    cfg
}

fn push_handler(probe: &Probe, hold: Option<Duration>) -> LambdaA2aHandler {
    push_handler_on(
        InMemoryA2aStorage::new().with_push_dispatch_enabled(true),
        probe,
        hold,
        runtime_config(),
    )
}

fn push_handler_on(
    storage: InMemoryA2aStorage,
    probe: &Probe,
    hold: Option<Duration>,
    cfg: RuntimeConfig,
) -> LambdaA2aHandler {
    LambdaA2aHandler::builder()
        .executor(CompletesThenWorks {
            probe: Probe::default(),
            tail: None,
        })
        .storage(storage.clone())
        .push_storage(SlowPushConfigs {
            inner: storage.clone(),
            probe: probe.clone(),
            hold,
        })
        .push_delivery_store(storage)
        .runtime_config(cfg)
        .build()
        .expect("lambda handler builds")
}

fn tail_handler(probe: &Probe, tail: Option<Duration>) -> LambdaA2aHandler {
    LambdaA2aHandler::builder()
        .executor(CompletesThenWorks {
            probe: probe.clone(),
            tail: Some(tail),
        })
        .storage(InMemoryA2aStorage::new())
        .runtime_config(runtime_config())
        .build()
        .expect("lambda handler builds")
}

async fn send(handler: &LambdaA2aHandler) -> (u16, Value) {
    let body = json!({
        "message": {
            "messageId": uuid::Uuid::now_v7().to_string(),
            "role": "ROLE_USER",
            "parts": [{"text": "hi"}],
        },
    });
    let req = http::Request::builder()
        .method("POST")
        .uri("/message:send")
        .header("content-type", "application/json")
        .header("a2a-version", "1.0")
        .body(lambda_http::Body::Text(body.to_string()))
        .unwrap();
    let resp = handler.handle(req).await.expect("handle");
    let status = resp.status().as_u16();
    let body = serde_json::from_slice(resp.body().as_ref()).unwrap_or(Value::Null);
    (status, body)
}

fn assert_completed(status: u16, body: &Value) {
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{body}"
    );
}

#[tokio::test]
async fn push_fan_out_finishes_before_the_response_returns() {
    let probe = Probe::default();
    let handler = push_handler(&probe, Some(Duration::from_millis(100)));
    let (status, body) = send(&handler).await;
    assert_completed(status, &body);
    assert_eq!(probe.open(), 0, "push fan-out still pending after return");
    assert_eq!(probe.finished(), 1, "push fan-out did not run to the end");
}

#[tokio::test]
async fn stuck_push_fan_out_is_cancelled_before_the_response_returns() {
    let probe = Probe::default();
    let handler = push_handler(&probe, None);
    let started = Instant::now();
    let (status, body) = send(&handler).await;
    assert_completed(status, &body);
    assert_eq!(
        probe.open(),
        0,
        "stuck push fan-out still pending after return"
    );
    assert!(
        started.elapsed() < GRACE * 3,
        "settling took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn executor_tail_finishes_before_the_response_returns() {
    let probe = Probe::default();
    let handler = tail_handler(&probe, Some(Duration::from_millis(100)));
    let (status, body) = send(&handler).await;
    assert_completed(status, &body);
    assert_eq!(probe.open(), 0, "executor tail still pending after return");
    assert_eq!(probe.finished(), 1, "executor tail did not run to the end");
}

#[tokio::test]
async fn stuck_executor_tail_is_aborted_before_the_response_returns() {
    let probe = Probe::default();
    let handler = tail_handler(&probe, None);
    let started = Instant::now();
    let (status, body) = send(&handler).await;
    assert_completed(status, &body);
    assert_eq!(
        probe.open(),
        0,
        "stuck executor tail still pending after return"
    );
    assert!(
        started.elapsed() < GRACE * 3,
        "settling took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn idle_sweep_push_fan_out_finishes_before_the_sweep_returns() {
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
    tokio::time::sleep(Duration::from_millis(100)).await;

    let probe = Probe::default();
    let mut cfg = runtime_config();
    cfg.idle_task_timeout = Some(Duration::from_millis(50));
    let handler = push_handler_on(storage, &probe, Some(Duration::from_millis(100)), cfg);
    let sweep = handler.cancel_idle_tasks().await.expect("sweep");
    assert_eq!(sweep.canceled, 1);
    assert_eq!(
        probe.open(),
        0,
        "idle-sweep push fan-out still pending after return"
    );
    assert_eq!(
        probe.finished(),
        1,
        "idle-sweep push fan-out did not run to the end"
    );
}
