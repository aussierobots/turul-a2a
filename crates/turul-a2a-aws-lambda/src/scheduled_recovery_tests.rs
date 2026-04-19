//! ADR-013 §5.3 / §10.9 / §10.10 tests for [`LambdaScheduledRecoveryHandler`].
//!
//! The scheduled handler is the mandatory backstop: when DynamoDB
//! Streams are unavailable (non-DDB backends, stream stalled, poison
//! record) it is the ONLY path from a durable marker to a webhook
//! POST. These tests pin that invariant — a single `run_sweep()`
//! call must recover stale markers and redispatch stuck claims
//! without relying on any post-return background task.

use std::sync::Arc;

use async_trait::async_trait;
use turul_a2a::push::delivery::{PushDeliveryConfig, PushDeliveryWorker};
use turul_a2a::push::{
    A2aPushDeliveryStore, PendingDispatch, PushDispatcher, ReclaimableClaim,
};
use turul_a2a::storage::{
    A2aAtomicStore, A2aPushNotificationStorage, A2aStorageError, A2aTaskStorage,
    InMemoryA2aStorage,
};
use turul_a2a::streaming::{StatusUpdatePayload, StreamEvent};
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::scheduled_recovery::{
    LambdaScheduledRecoveryConfig, LambdaScheduledRecoveryHandler,
};

async fn build_stack() -> (
    Arc<InMemoryA2aStorage>,
    Arc<PushDispatcher>,
    Arc<dyn A2aPushDeliveryStore>,
) {
    let storage = Arc::new(InMemoryA2aStorage::new().with_push_dispatch_enabled(true));
    let delivery: Arc<dyn A2aPushDeliveryStore> = storage.clone();
    let push_storage: Arc<dyn A2aPushNotificationStorage> = storage.clone();
    let task_storage: Arc<dyn A2aTaskStorage> = storage.clone();
    let mut cfg = PushDeliveryConfig::default();
    cfg.max_attempts = 1;
    cfg.backoff_base = std::time::Duration::from_millis(1);
    cfg.backoff_cap = std::time::Duration::from_millis(1);
    cfg.backoff_jitter = 0.0;
    cfg.request_timeout = std::time::Duration::from_secs(1);
    cfg.connect_timeout = std::time::Duration::from_millis(500);
    cfg.read_timeout = std::time::Duration::from_secs(1);
    cfg.claim_expiry = std::time::Duration::from_millis(50);
    cfg.max_payload_bytes = 64 * 1024;
    cfg.allow_insecure_urls = true;
    let worker = Arc::new(
        PushDeliveryWorker::new(
            delivery.clone(),
            cfg,
            None,
            "scheduled-recovery-test".into(),
        )
        .expect("worker build"),
    );
    let dispatcher = Arc::new(PushDispatcher::new(worker, push_storage, task_storage));
    (storage, dispatcher, delivery)
}

async fn seed_task_with_marker(
    storage: &Arc<InMemoryA2aStorage>,
    webhook_url: &str,
    task_id: &str,
) -> (String, String, String, u64) {
    let tenant = "t-sched";
    let owner = "owner-1";
    let ctx = "ctx-sched";
    let working =
        Task::new(task_id, TaskStatus::new(TaskState::Working)).with_context_id(ctx);
    A2aTaskStorage::create_task(storage.as_ref(), tenant, owner, working)
        .await
        .expect("seed task");
    A2aPushNotificationStorage::create_config(
        storage.as_ref(),
        tenant,
        turul_a2a_proto::TaskPushNotificationConfig {
            tenant: tenant.into(),
            id: format!("{task_id}-cfg"),
            task_id: task_id.into(),
            url: webhook_url.to_string(),
            token: String::new(),
            authentication: None,
        },
    )
    .await
    .expect("seed config");
    let completed = TaskStatus::new(TaskState::Completed);
    let evt = StreamEvent::StatusUpdate {
        status_update: StatusUpdatePayload {
            task_id: task_id.into(),
            context_id: ctx.into(),
            status: serde_json::to_value(&completed).unwrap(),
        },
    };
    let (_t, seqs) = storage
        .update_task_status_with_events(tenant, task_id, owner, completed, vec![evt])
        .await
        .expect("terminal commit");
    (tenant.into(), task_id.into(), owner.into(), seqs[0])
}

// ---------- Release-gate tests ----------

#[tokio::test]
async fn scheduled_sweep_recovers_stale_marker_and_fires_post() {
    // ADR-013 §5.3 / §10.9: a marker committed before this tick's
    // cutoff is picked up by list_stale_pending_dispatches and
    // redispatched → exactly one POST lands; the marker is gone.
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock)
        .await;

    let (storage, dispatcher, delivery) = build_stack().await;
    let (tenant, task_id, _owner, seq) =
        seed_task_with_marker(&storage, &format!("{}/webhook", mock.uri()), "t-stale").await;

    // Configure a zero-length stale cutoff so the just-written marker
    // qualifies immediately. This matches the ADR's contract — the
    // cutoff is operator-tuned, not hard-coded; tests set it to the
    // shortest value that admits the seeded marker.
    let handler = LambdaScheduledRecoveryHandler::new(dispatcher, delivery.clone())
        .with_config(LambdaScheduledRecoveryConfig {
            stale_cutoff: std::time::Duration::ZERO,
            stale_markers_limit: 16,
            reclaimable_claims_limit: 16,
        });
    let response = handler.run_sweep().await;

    assert_eq!(response.stale_markers_found, 1);
    assert_eq!(response.stale_markers_recovered, 1);
    assert_eq!(response.stale_markers_transient_errors, 0);
    assert!(response.errors.is_empty(), "no errors expected: {:?}", response.errors);

    // Marker must have been deleted by the redispatch path.
    let remaining = delivery
        .list_stale_pending_dispatches(
            std::time::SystemTime::now() + std::time::Duration::from_secs(60),
            16,
        )
        .await
        .expect("list");
    assert!(
        remaining.iter().all(|p| !(p.tenant == tenant && p.task_id == task_id && p.event_sequence == seq)),
        "marker must be deleted after successful sweep; remaining: {remaining:?}"
    );
}

#[tokio::test]
async fn scheduled_sweep_counts_transient_error_and_retains_marker() {
    // ADR-013 §5.3: a marker that hits a transient error on
    // try_redispatch_pending is counted in
    // stale_markers_transient_errors and the marker is retained for
    // the next tick.
    //
    // We inject the transient failure by wrapping task_storage in a
    // one-shot-fail adapter so get_task returns Err exactly once.
    struct FlakyTaskStorage {
        inner: Arc<InMemoryA2aStorage>,
        fail_once: std::sync::atomic::AtomicBool,
    }
    #[async_trait]
    impl A2aTaskStorage for FlakyTaskStorage {
        fn backend_name(&self) -> &'static str {
            "in-memory"
        }
        async fn get_task(
            &self,
            t: &str,
            id: &str,
            o: &str,
            h: Option<i32>,
        ) -> Result<Option<Task>, A2aStorageError> {
            use std::sync::atomic::Ordering;
            if self.fail_once.swap(false, Ordering::SeqCst) {
                return Err(A2aStorageError::DatabaseError("simulated transient".into()));
            }
            self.inner.get_task(t, id, o, h).await
        }
        async fn create_task(
            &self,
            t: &str,
            o: &str,
            task: Task,
        ) -> Result<Task, A2aStorageError> {
            self.inner.create_task(t, o, task).await
        }
        async fn update_task(
            &self,
            t: &str,
            o: &str,
            task: Task,
        ) -> Result<(), A2aStorageError> {
            self.inner.update_task(t, o, task).await
        }
        async fn delete_task(
            &self,
            t: &str,
            id: &str,
            o: &str,
        ) -> Result<bool, A2aStorageError> {
            self.inner.delete_task(t, id, o).await
        }
        async fn list_tasks(
            &self,
            f: turul_a2a::storage::TaskFilter,
        ) -> Result<turul_a2a::storage::TaskListPage, A2aStorageError> {
            self.inner.list_tasks(f).await
        }
        async fn update_task_status(
            &self,
            t: &str,
            id: &str,
            o: &str,
            s: TaskStatus,
        ) -> Result<Task, A2aStorageError> {
            self.inner.update_task_status(t, id, o, s).await
        }
        async fn append_message(
            &self,
            t: &str,
            id: &str,
            o: &str,
            m: Message,
        ) -> Result<(), A2aStorageError> {
            self.inner.append_message(t, id, o, m).await
        }
        async fn append_artifact(
            &self,
            t: &str,
            id: &str,
            o: &str,
            a: turul_a2a_types::Artifact,
            append: bool,
            last: bool,
        ) -> Result<(), A2aStorageError> {
            self.inner
                .append_artifact(t, id, o, a, append, last)
                .await
        }
        async fn task_count(&self) -> Result<usize, A2aStorageError> {
            self.inner.task_count().await
        }
        async fn maintenance(&self) -> Result<(), A2aStorageError> {
            self.inner.maintenance().await
        }
        async fn set_cancel_requested(
            &self,
            t: &str,
            id: &str,
            o: &str,
        ) -> Result<(), A2aStorageError> {
            self.inner.set_cancel_requested(t, id, o).await
        }
    }

    let (storage, _dispatcher, delivery) = build_stack().await;
    let (tenant, task_id, _owner, seq) =
        seed_task_with_marker(&storage, "http://unused.invalid/webhook", "t-flaky").await;

    let flaky = Arc::new(FlakyTaskStorage {
        inner: storage.clone(),
        fail_once: std::sync::atomic::AtomicBool::new(true),
    });
    let mut cfg = PushDeliveryConfig::default();
    cfg.max_attempts = 1;
    cfg.backoff_base = std::time::Duration::from_millis(1);
    cfg.backoff_cap = std::time::Duration::from_millis(1);
    cfg.backoff_jitter = 0.0;
    cfg.request_timeout = std::time::Duration::from_secs(1);
    cfg.connect_timeout = std::time::Duration::from_millis(500);
    cfg.read_timeout = std::time::Duration::from_secs(1);
    cfg.claim_expiry = std::time::Duration::from_secs(30);
    cfg.max_payload_bytes = 64 * 1024;
    cfg.allow_insecure_urls = true;
    let worker = Arc::new(
        PushDeliveryWorker::new(delivery.clone(), cfg, None, "flaky-sched-test".into())
            .expect("worker"),
    );
    let push_storage: Arc<dyn A2aPushNotificationStorage> = storage.clone();
    let dispatcher = Arc::new(PushDispatcher::new(worker, push_storage, flaky));

    let handler = LambdaScheduledRecoveryHandler::new(dispatcher, delivery.clone())
        .with_config(LambdaScheduledRecoveryConfig {
            stale_cutoff: std::time::Duration::ZERO,
            stale_markers_limit: 16,
            reclaimable_claims_limit: 16,
        });
    let response = handler.run_sweep().await;

    assert_eq!(response.stale_markers_found, 1);
    assert_eq!(response.stale_markers_recovered, 0);
    assert_eq!(response.stale_markers_transient_errors, 1);
    assert!(
        !response.errors.is_empty(),
        "transient error must be sampled into response.errors"
    );
    assert!(
        response.errors[0].contains("simulated transient"),
        "sampled error must include the transient reason: {:?}",
        response.errors
    );

    // Marker must still be present for the next tick.
    let remaining = delivery
        .list_stale_pending_dispatches(
            std::time::SystemTime::now() + std::time::Duration::from_secs(60),
            16,
        )
        .await
        .expect("list");
    assert!(
        remaining
            .iter()
            .any(|p| p.tenant == tenant && p.task_id == task_id && p.event_sequence == seq),
        "transient-error marker must be retained; remaining: {remaining:?}"
    );
}

#[tokio::test]
async fn scheduled_sweep_processes_reclaimable_claims() {
    // ADR-013 §5.3 / §10.10: reclaimable claim rows are enumerated and
    // redispatched. Here we seed an expired non-terminal claim row
    // directly, then run a sweep and verify
    // reclaimable_claims_processed reflects the row.
    let (storage, dispatcher, delivery) = build_stack().await;
    let (tenant, task_id, _owner, seq) =
        seed_task_with_marker(&storage, "http://unused.invalid/webhook", "t-reclaim").await;

    // Create a claim with a very short expiry so it is immediately
    // reclaimable. `delivery.claim_delivery` is the public path.
    let claim = delivery
        .claim_delivery(
            &tenant,
            &task_id,
            seq,
            &format!("{task_id}-cfg"),
            "instance-A",
            "owner-1",
            std::time::Duration::from_millis(1),
        )
        .await
        .expect("claim");
    let _ = claim; // we don't need the returned claim, only its row

    // Wait out the tiny expiry.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let handler = LambdaScheduledRecoveryHandler::new(dispatcher, delivery.clone())
        .with_config(LambdaScheduledRecoveryConfig {
            stale_cutoff: std::time::Duration::from_secs(60 * 60),
            stale_markers_limit: 16,
            reclaimable_claims_limit: 16,
        });
    let response = handler.run_sweep().await;

    assert_eq!(
        response.reclaimable_claims_found, 1,
        "expired non-terminal claim must be found"
    );
    assert_eq!(
        response.reclaimable_claims_processed, 1,
        "the row must be driven through redispatch_one"
    );
}

#[tokio::test]
async fn scheduled_sweep_no_work_returns_zero_counts() {
    // Fresh stack, nothing seeded. Sweep should be a clean no-op.
    let (_storage, dispatcher, delivery) = build_stack().await;
    let handler = LambdaScheduledRecoveryHandler::new(dispatcher, delivery);
    let response = handler.run_sweep().await;

    assert_eq!(response.stale_markers_found, 0);
    assert_eq!(response.stale_markers_recovered, 0);
    assert_eq!(response.stale_markers_transient_errors, 0);
    assert_eq!(response.reclaimable_claims_found, 0);
    assert_eq!(response.reclaimable_claims_processed, 0);
    assert!(response.errors.is_empty());
}

#[tokio::test]
async fn scheduled_sweep_respects_stale_markers_limit() {
    // ADR-013 §5.3: the handler's batch limit MUST cap the number of
    // markers processed per tick, so a scheduler outage doesn't
    // produce unbounded work when it recovers.
    let (storage, dispatcher, delivery) = build_stack().await;

    // Seed 5 independent tasks+markers.
    let mut webhook_urls = Vec::new();
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        // up to 2 POSTs allowed; if the limit is ignored we'd see 5.
        .expect(2)
        .mount(&mock)
        .await;
    for i in 0..5 {
        let url = format!("{}/webhook", mock.uri());
        let _ = seed_task_with_marker(&storage, &url, &format!("task-{i}")).await;
        webhook_urls.push(url);
    }

    let handler = LambdaScheduledRecoveryHandler::new(dispatcher, delivery)
        .with_config(LambdaScheduledRecoveryConfig {
            stale_cutoff: std::time::Duration::ZERO,
            stale_markers_limit: 2, // cap
            reclaimable_claims_limit: 16,
        });
    let response = handler.run_sweep().await;

    assert_eq!(
        response.stale_markers_found, 2,
        "stale_markers_limit=2 must cap the first-tick batch: {response:?}"
    );
    assert_eq!(response.stale_markers_recovered, 2);
    // wiremock's expect(2) assertion fires on Drop.
    drop(mock);
}

// Tiny lints referenced so the `ReclaimableClaim`/`PendingDispatch` imports
// don't get flagged as unused on a build where the transient test is
// excluded. Accessing types is enough.
fn _assert_reexports(_p: PendingDispatch, _c: ReclaimableClaim) {}
