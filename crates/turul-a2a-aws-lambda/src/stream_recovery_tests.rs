//! ADR-013 §5.2 / §10 tests for [`LambdaStreamRecoveryHandler`].
//!
//! The release-gate contract (ADR §10.5, §10.6, §10.7):
//!
//! - Valid marker + successful redispatch → no BatchItemFailure; POST fires.
//! - `get_task → Ok(None)` (task deleted) → no BatchItemFailure, marker deleted.
//! - Transient storage error on `get_task` → BatchItemFailure with the record's SequenceNumber.
//! - Unparseable NEW_IMAGE → BatchItemFailure.
//! - Duplicate records (same `(tenant, task_id, event_sequence)`) → exactly one POST.
//! - Non-INSERT records (MODIFY / REMOVE) → skipped silently.
//!
//! The tests run the full request-Lambda storage stack (in-memory,
//! opt-in enabled) plus a wiremock for push URLs, so the assertion
//! surface is the same as production behaviour — only the input
//! shape differs (synthetic `DynamoDbEvent` vs a real DDB stream).

use std::sync::Arc;

use async_trait::async_trait;
use aws_lambda_events::event::dynamodb::{Event as DynamoDbEvent, EventRecord, StreamRecord};
use chrono::{TimeZone, Utc};
use serde::Serialize;
use turul_a2a::error::A2aError;
use turul_a2a::executor::AgentExecutor;
use turul_a2a::push::delivery::{PushDeliveryConfig, PushDeliveryWorker};
use turul_a2a::push::{A2aPushDeliveryStore, PushDispatcher};
use turul_a2a::storage::{
    A2aAtomicStore, A2aPushNotificationStorage, A2aStorageError, A2aTaskStorage,
    InMemoryA2aStorage,
};
use turul_a2a::streaming::{StatusUpdatePayload, StreamEvent};
use turul_a2a_types::{Message, Task, TaskState, TaskStatus};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::stream_recovery::LambdaStreamRecoveryHandler;

struct NoopExecutor;
#[async_trait]
impl AgentExecutor for NoopExecutor {
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

/// Helper: build an in-memory storage with push dispatch enabled,
/// plus a dispatcher wired against it.
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
    cfg.claim_expiry = std::time::Duration::from_secs(30);
    cfg.max_payload_bytes = 64 * 1024;
    cfg.allow_insecure_urls = true;

    let worker = Arc::new(
        PushDeliveryWorker::new(delivery.clone(), cfg, None, "stream-recovery-test".into())
            .expect("worker build"),
    );
    let dispatcher = Arc::new(PushDispatcher::new(worker, push_storage, task_storage));
    (storage, dispatcher, delivery)
}

/// Helper: seed a task + config and run an atomic terminal commit so a
/// pending-dispatch marker exists. Returns `(tenant, task_id, owner,
/// terminal_seq)`.
async fn seed_task_with_marker(
    storage: &Arc<InMemoryA2aStorage>,
    webhook_url: &str,
    task_id: &str,
) -> (String, String, String, u64) {
    let tenant = "t-stream";
    let owner = "owner-1";
    let ctx = "ctx-stream";

    let working =
        Task::new(task_id, TaskStatus::new(TaskState::Working)).with_context_id(ctx);
    A2aTaskStorage::create_task(storage.as_ref(), tenant, owner, working)
        .await
        .expect("seed");

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
    let terminal_evt = StreamEvent::StatusUpdate {
        status_update: StatusUpdatePayload {
            task_id: task_id.into(),
            context_id: ctx.into(),
            status: serde_json::to_value(&completed).unwrap(),
        },
    };
    let (_t, seqs) = storage
        .update_task_status_with_events(
            tenant,
            task_id,
            owner,
            completed,
            vec![terminal_evt],
        )
        .await
        .expect("terminal commit");

    (tenant.into(), task_id.into(), owner.into(), seqs[0])
}

/// Helper: build a DDB stream INSERT record for a pending-dispatch
/// marker with the attribute shape that
/// `DynamoDbA2aStorage::update_task_status_with_events` writes.
/// Mirror of the DynamoDB backend's pending-dispatch row shape.
/// Serialising this via `serde_dynamo::to_item` produces the same
/// `serde_dynamo::Item` the AWS DDB Stream would deliver as NEW_IMAGE.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PendingRowImage<'a> {
    tenant: &'a str,
    task_id: &'a str,
    owner: &'a str,
    event_sequence: u64,
    recorded_at_micros: i64,
}

fn make_insert_record(
    tenant: &str,
    task_id: &str,
    owner: &str,
    event_sequence: u64,
    sequence_number: &str,
) -> EventRecord {
    let new_image: serde_dynamo::Item = serde_dynamo::to_item(PendingRowImage {
        tenant,
        task_id,
        owner,
        event_sequence,
        recorded_at_micros: 1,
    })
    .expect("serialize NEW_IMAGE");

    EventRecord {
        aws_region: "us-east-1".into(),
        change: StreamRecord {
            approximate_creation_date_time: Utc.timestamp_opt(0, 0).unwrap(),
            keys: serde_dynamo::Item::default(),
            new_image,
            old_image: serde_dynamo::Item::default(),
            sequence_number: Some(sequence_number.into()),
            size_bytes: 0,
            stream_view_type: Some(aws_lambda_events::event::dynamodb::StreamViewType::NewImage),
        },
        event_id: format!("evt-{sequence_number}"),
        event_name: "INSERT".into(),
        event_source: None,
        event_version: None,
        event_source_arn: None,
        user_identity: None,
        record_format: None,
        table_name: None,
    }
}

/// Variant that omits a field or uses wrong types so NEW_IMAGE parses fail.
fn make_malformed_record(sequence_number: &str) -> EventRecord {
    // Only the `tenant` attribute — parser should fail on the first
    // missing required attribute.
    #[derive(Serialize)]
    struct OnlyTenant<'a> {
        tenant: &'a str,
    }
    let new_image: serde_dynamo::Item =
        serde_dynamo::to_item(OnlyTenant { tenant: "t" }).expect("serialize");
    EventRecord {
        aws_region: "us-east-1".into(),
        change: StreamRecord {
            approximate_creation_date_time: Utc.timestamp_opt(0, 0).unwrap(),
            keys: serde_dynamo::Item::default(),
            new_image,
            old_image: serde_dynamo::Item::default(),
            sequence_number: Some(sequence_number.into()),
            size_bytes: 0,
            stream_view_type: Some(aws_lambda_events::event::dynamodb::StreamViewType::NewImage),
        },
        event_id: format!("evt-{sequence_number}"),
        event_name: "INSERT".into(),
        event_source: None,
        event_version: None,
        event_source_arn: None,
        user_identity: None,
        record_format: None,
        table_name: None,
    }
}

// ---------- Release-gate tests ----------

#[tokio::test]
async fn stream_success_path_fires_one_post_and_no_batch_failures() {
    // ADR-013 §5.2: a valid INSERT triggers redispatch; dispatcher
    // fans out to the registered config; exactly one POST fires; no
    // BatchItemFailure surfaces.
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock)
        .await;

    let (storage, dispatcher, _delivery) = build_stack().await;
    let (tenant, task_id, owner, seq) =
        seed_task_with_marker(&storage, &format!("{}/webhook", mock.uri()), "t-success").await;

    let handler = LambdaStreamRecoveryHandler::new(dispatcher);
    let event = DynamoDbEvent {
        records: vec![make_insert_record(&tenant, &task_id, &owner, seq, "seq-1")],
    };
    let response = handler.handle_stream_event(event).await;

    assert!(
        response.batch_item_failures.is_empty(),
        "success path must not surface BatchItemFailure: {:?}",
        response.batch_item_failures
    );
}

#[tokio::test]
async fn stream_deleted_task_returns_success_and_deletes_marker() {
    // ADR-013 §4.6 / §10.5: task deleted between marker write and
    // stream delivery → delete marker, return success (NO
    // BatchItemFailure).
    let (storage, dispatcher, delivery) = build_stack().await;
    let (tenant, task_id, owner, seq) =
        seed_task_with_marker(&storage, "http://unused.invalid/webhook", "t-deleted").await;

    // Delete the task so get_task returns Ok(None).
    A2aTaskStorage::delete_task(storage.as_ref(), &tenant, &task_id, &owner)
        .await
        .expect("delete task");

    let handler = LambdaStreamRecoveryHandler::new(dispatcher);
    let event = DynamoDbEvent {
        records: vec![make_insert_record(&tenant, &task_id, &owner, seq, "seq-1")],
    };
    let response = handler.handle_stream_event(event).await;

    assert!(
        response.batch_item_failures.is_empty(),
        "deleted task is a permanent signal — NO BatchItemFailure; got {:?}",
        response.batch_item_failures
    );

    // Marker must have been deleted.
    let pending = delivery
        .list_stale_pending_dispatches(
            std::time::SystemTime::now() + std::time::Duration::from_secs(60),
            16,
        )
        .await
        .expect("list");
    assert!(
        pending.iter().all(|p| p.task_id != task_id),
        "marker for deleted task must be removed; got {pending:?}"
    );
}

#[tokio::test]
async fn stream_transient_storage_error_surfaces_batch_item_failure() {
    // ADR-013 §10.6: transient storage error → BatchItemFailure
    // identified by the record's SequenceNumber.
    //
    // We wrap the in-memory task storage in a failing-once variant.
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
        async fn create_task(&self, t: &str, o: &str, task: Task) -> Result<Task, A2aStorageError> {
            self.inner.create_task(t, o, task).await
        }
        async fn update_task(&self, t: &str, o: &str, task: Task) -> Result<(), A2aStorageError> {
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

    let (storage, _dispatcher, _delivery) = build_stack().await;
    let (tenant, task_id, owner, seq) =
        seed_task_with_marker(&storage, "http://unused.invalid/webhook", "t-flaky").await;

    // Replace the task_storage inside a fresh dispatcher with a flaky wrapper.
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
    let delivery2: Arc<dyn A2aPushDeliveryStore> = storage.clone();
    let worker = Arc::new(
        PushDeliveryWorker::new(delivery2, cfg, None, "flaky-test".into())
            .expect("worker"),
    );
    let push_storage: Arc<dyn A2aPushNotificationStorage> = storage.clone();
    let dispatcher = Arc::new(PushDispatcher::new(worker, push_storage, flaky));

    let handler = LambdaStreamRecoveryHandler::new(dispatcher);
    let event = DynamoDbEvent {
        records: vec![make_insert_record(
            &tenant,
            &task_id,
            &owner,
            seq,
            "seq-flaky",
        )],
    };
    let response = handler.handle_stream_event(event).await;

    assert_eq!(
        response.batch_item_failures.len(),
        1,
        "transient get_task error must surface exactly one BatchItemFailure"
    );
    assert_eq!(
        response.batch_item_failures[0].item_identifier.as_deref(),
        Some("seq-flaky"),
        "BatchItemFailure must carry the record's SequenceNumber"
    );
}

#[tokio::test]
async fn stream_unparseable_new_image_surfaces_batch_item_failure() {
    // ADR-013 §5.2: malformed NEW_IMAGE → BatchItemFailure.
    let (_storage, dispatcher, _delivery) = build_stack().await;
    let handler = LambdaStreamRecoveryHandler::new(dispatcher);
    let event = DynamoDbEvent {
        records: vec![make_malformed_record("seq-malformed")],
    };
    let response = handler.handle_stream_event(event).await;

    assert_eq!(response.batch_item_failures.len(), 1);
    assert_eq!(
        response.batch_item_failures[0].item_identifier.as_deref(),
        Some("seq-malformed"),
    );
}

#[tokio::test]
async fn stream_non_insert_records_are_skipped() {
    // ADR-013 §5.2: MODIFY / REMOVE records are skipped silently.
    let (storage, dispatcher, _delivery) = build_stack().await;
    let (tenant, task_id, owner, seq) =
        seed_task_with_marker(&storage, "http://unused.invalid/webhook", "t-mod").await;

    let handler = LambdaStreamRecoveryHandler::new(dispatcher);

    let mut record = make_insert_record(&tenant, &task_id, &owner, seq, "seq-mod");
    record.event_name = "MODIFY".into();
    let mut remove = make_insert_record(&tenant, &task_id, &owner, seq, "seq-rem");
    remove.event_name = "REMOVE".into();

    let event = DynamoDbEvent {
        records: vec![record, remove],
    };
    let response = handler.handle_stream_event(event).await;

    assert!(
        response.batch_item_failures.is_empty(),
        "non-INSERT records must be skipped silently"
    );
}

#[tokio::test]
async fn stream_duplicate_inserts_produce_exactly_one_post() {
    // ADR-013 §5.4 / §10.7: duplicate stream records for the same
    // (tenant, task_id, event_sequence) — claim fencing yields
    // exactly one POST.
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock)
        .await;

    let (storage, dispatcher, _delivery) = build_stack().await;
    let (tenant, task_id, owner, seq) =
        seed_task_with_marker(&storage, &format!("{}/webhook", mock.uri()), "t-dupe").await;

    let handler = LambdaStreamRecoveryHandler::new(dispatcher);
    let event = DynamoDbEvent {
        records: vec![
            make_insert_record(&tenant, &task_id, &owner, seq, "seq-a"),
            make_insert_record(&tenant, &task_id, &owner, seq, "seq-b"),
        ],
    };
    let response = handler.handle_stream_event(event).await;

    assert!(
        response.batch_item_failures.is_empty(),
        "duplicate records must not surface BatchItemFailure: {:?}",
        response.batch_item_failures
    );
    // wiremock's `expect(1)` assertion fires on Drop.
    drop(mock);
}
