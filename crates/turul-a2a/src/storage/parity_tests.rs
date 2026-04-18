//! Shared parity test functions for A2A storage backends.
//!
//! Each test takes a `&dyn A2aTaskStorage` so the same assertions apply to InMemory,
//! SQLite, PostgreSQL, and DynamoDB backends. Backend-specific test modules call
//! these functions with their own storage instance.

use turul_a2a_types::{Artifact, Message, Part, Role, Task, TaskState, TaskStatus};

use crate::streaming::StreamEvent;
use super::atomic::A2aAtomicStore;
use super::event_store::A2aEventStore;
use super::filter::TaskFilter;
use super::traits::{A2aPushNotificationStorage, A2aTaskStorage};

fn make_task(task_id: &str, context_id: &str) -> Task {
    Task::new(task_id, TaskStatus::new(TaskState::Submitted)).with_context_id(context_id)
}

fn make_message(id: &str, text: &str) -> Message {
    Message::new(id, Role::User, vec![Part::text(text)])
}

fn make_artifact(id: &str, text: &str) -> Artifact {
    Artifact::new(id, vec![Part::text(text)])
}

// =========================================================
// P2-001: CRUD round-trip
// =========================================================
pub async fn test_create_and_retrieve(storage: &dyn A2aTaskStorage) {
    let task = make_task("parity-crud-1", "ctx-1");
    let created = storage.create_task("default", "owner-a", task).await.unwrap();
    assert_eq!(created.id(), "parity-crud-1");
    assert_eq!(created.context_id(), "ctx-1");

    // Get
    let fetched = storage
        .get_task("default", "parity-crud-1", "owner-a", None)
        .await
        .unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.id(), "parity-crud-1");

    // Get nonexistent
    let missing = storage
        .get_task("default", "nonexistent", "owner-a", None)
        .await
        .unwrap();
    assert!(missing.is_none());

    // Delete
    assert!(storage.delete_task("default", "parity-crud-1", "owner-a").await.unwrap());
    // Second delete returns false
    assert!(!storage.delete_task("default", "parity-crud-1", "owner-a").await.unwrap());
}

// =========================================================
// P2-004: State machine enforcement
// =========================================================
pub async fn test_state_machine_enforcement(storage: &dyn A2aTaskStorage) {
    let task = make_task("sm-1", "ctx-sm");
    storage.create_task("default", "owner-a", task).await.unwrap();

    // Valid: Submitted -> Working
    let updated = storage
        .update_task_status("default", "sm-1", "owner-a", TaskStatus::new(TaskState::Working))
        .await
        .unwrap();
    assert_eq!(updated.status().unwrap().state().unwrap(), TaskState::Working);

    // Valid: Working -> InputRequired
    storage
        .update_task_status("default", "sm-1", "owner-a", TaskStatus::new(TaskState::InputRequired))
        .await
        .unwrap();

    // Valid: InputRequired -> Working
    storage
        .update_task_status("default", "sm-1", "owner-a", TaskStatus::new(TaskState::Working))
        .await
        .unwrap();

    // Valid: Working -> Completed
    storage
        .update_task_status("default", "sm-1", "owner-a", TaskStatus::new(TaskState::Completed))
        .await
        .unwrap();
}

// =========================================================
// P2-004: Terminal state rejection
// =========================================================
pub async fn test_terminal_state_rejection(storage: &dyn A2aTaskStorage) {
    for (i, terminal) in [
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Canceled,
        TaskState::Rejected,
    ]
    .iter()
    .enumerate()
    {
        let id = format!("term-{i}");
        let task = make_task(&id, "ctx-term");
        storage.create_task("default", "owner-a", task).await.unwrap();

        // Move to Working first (Submitted -> Working is valid)
        storage
            .update_task_status("default", &id, "owner-a", TaskStatus::new(TaskState::Working))
            .await
            .unwrap();

        // Move to terminal (Working -> terminal is valid for all except Rejected)
        if *terminal == TaskState::Rejected {
            // Rejected is only valid from Submitted, so create a new task
            let id2 = format!("term-rej-{i}");
            storage.create_task("default", "owner-a", make_task(&id2, "ctx-term")).await.unwrap();
            storage
                .update_task_status("default", &id2, "owner-a", TaskStatus::new(TaskState::Rejected))
                .await
                .unwrap();
            // Now try to transition away from Rejected
            let result = storage
                .update_task_status("default", &id2, "owner-a", TaskStatus::new(TaskState::Working))
                .await;
            assert!(result.is_err(), "Terminal {terminal:?} should reject transitions");
        } else {
            storage
                .update_task_status("default", &id, "owner-a", TaskStatus::new(*terminal))
                .await
                .unwrap();
            // Try to transition away
            let result = storage
                .update_task_status("default", &id, "owner-a", TaskStatus::new(TaskState::Working))
                .await;
            assert!(result.is_err(), "Terminal {terminal:?} should reject transitions");
        }
    }
}

// =========================================================
// P2-014: Tenant isolation
// =========================================================
pub async fn test_tenant_isolation(storage: &dyn A2aTaskStorage) {
    storage.create_task("tenant-a", "owner", make_task("ti-1", "ctx")).await.unwrap();
    storage.create_task("tenant-b", "owner", make_task("ti-2", "ctx")).await.unwrap();

    // Tenant A can't see Tenant B's task
    let result = storage.get_task("tenant-a", "ti-2", "owner", None).await.unwrap();
    assert!(result.is_none(), "Tenant A should not see Tenant B's task");

    // Tenant B can't see Tenant A's task
    let result = storage.get_task("tenant-b", "ti-1", "owner", None).await.unwrap();
    assert!(result.is_none(), "Tenant B should not see Tenant A's task");

    // List scoped by tenant
    let page_a = storage
        .list_tasks(TaskFilter {
            tenant: Some("tenant-a".to_string()),
            owner: Some("owner".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page_a.total_size, 1);
    assert_eq!(page_a.tasks[0].id(), "ti-1");

    // Tenant A can't delete Tenant B's task
    assert!(!storage.delete_task("tenant-a", "ti-2", "owner").await.unwrap());
}

// =========================================================
// P2-005: Owner isolation
// =========================================================
pub async fn test_owner_isolation(storage: &dyn A2aTaskStorage) {
    storage.create_task("default", "alice", make_task("oi-1", "ctx")).await.unwrap();
    storage.create_task("default", "bob", make_task("oi-2", "ctx")).await.unwrap();

    // Alice can't see Bob's task
    let result = storage.get_task("default", "oi-2", "alice", None).await.unwrap();
    assert!(result.is_none());

    // Alice can't delete Bob's task
    assert!(!storage.delete_task("default", "oi-2", "alice").await.unwrap());

    // Alice can't update status on Bob's task
    let result = storage
        .update_task_status("default", "oi-2", "alice", TaskStatus::new(TaskState::Working))
        .await;
    assert!(result.is_err());
}

// =========================================================
// P2-005: History length semantics
// =========================================================
pub async fn test_history_length(storage: &dyn A2aTaskStorage) {
    storage.create_task("default", "owner", make_task("hl-1", "ctx")).await.unwrap();

    // Append 5 messages
    for i in 0..5 {
        storage
            .append_message("default", "hl-1", "owner", make_message(&format!("m-{i}"), &format!("msg {i}")))
            .await
            .unwrap();
    }

    // history_length=0 -> empty history
    let task = storage.get_task("default", "hl-1", "owner", Some(0)).await.unwrap().unwrap();
    assert!(task.history().is_empty(), "history_length=0 should return empty history");

    // history_length=None -> all messages
    let task = storage.get_task("default", "hl-1", "owner", None).await.unwrap().unwrap();
    assert_eq!(task.history().len(), 5, "history_length=None should return all");

    // history_length=2 -> last 2
    let task = storage.get_task("default", "hl-1", "owner", Some(2)).await.unwrap().unwrap();
    assert_eq!(task.history().len(), 2, "history_length=2 should return 2");
    // Should be the LAST 2 messages
    assert_eq!(task.history()[0].message_id, "m-3");
    assert_eq!(task.history()[1].message_id, "m-4");
}

// =========================================================
// P2-009: List pagination
// =========================================================
pub async fn test_list_pagination(storage: &dyn A2aTaskStorage) {
    // Create 7 tasks
    for i in 0..7 {
        storage
            .create_task("default", "owner", make_task(&format!("pg-{i}"), "ctx-pg"))
            .await
            .unwrap();
    }

    let mut all_ids = Vec::new();
    let mut page_token = None;

    loop {
        let page = storage
            .list_tasks(TaskFilter {
                tenant: Some("default".to_string()),
                owner: Some("owner".to_string()),
                context_id: Some("ctx-pg".to_string()),
                page_size: Some(3),
                page_token: page_token.clone(),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(page.total_size, 7, "total_size should be 7 on every page");
        assert!(page.tasks.len() <= 3, "page should have at most 3 tasks");
        all_ids.extend(page.tasks.iter().map(|t| t.id().to_string()));

        if page.next_page_token.is_empty() {
            break;
        }
        page_token = Some(page.next_page_token);
    }

    assert_eq!(all_ids.len(), 7, "should collect all 7 tasks across pages");
    // No duplicates
    let unique: std::collections::HashSet<_> = all_ids.iter().collect();
    assert_eq!(unique.len(), 7);
}

// =========================================================
// P2-011: List filter by status
// =========================================================
pub async fn test_list_filter_by_status(storage: &dyn A2aTaskStorage) {
    for i in 0..3 {
        storage
            .create_task("default", "owner", make_task(&format!("fs-{i}"), "ctx-fs"))
            .await
            .unwrap();
    }
    // Move task 0 and 2 to Working
    storage.update_task_status("default", "fs-0", "owner", TaskStatus::new(TaskState::Working)).await.unwrap();
    storage.update_task_status("default", "fs-2", "owner", TaskStatus::new(TaskState::Working)).await.unwrap();

    let page = storage
        .list_tasks(TaskFilter {
            tenant: Some("default".to_string()),
            owner: Some("owner".to_string()),
            status: Some(TaskState::Working),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total_size, 2);
}

// =========================================================
// P2-008: List filter by context_id
// =========================================================
pub async fn test_list_filter_by_context_id(storage: &dyn A2aTaskStorage) {
    storage.create_task("default", "owner", make_task("fc-1", "ctx-alpha")).await.unwrap();
    storage.create_task("default", "owner", make_task("fc-2", "ctx-beta")).await.unwrap();
    storage.create_task("default", "owner", make_task("fc-3", "ctx-alpha")).await.unwrap();

    let page = storage
        .list_tasks(TaskFilter {
            tenant: Some("default".to_string()),
            owner: Some("owner".to_string()),
            context_id: Some("ctx-alpha".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total_size, 2);
}

// =========================================================
// P2-006: Append message
// =========================================================
pub async fn test_append_message(storage: &dyn A2aTaskStorage) {
    storage.create_task("default", "owner", make_task("am-1", "ctx")).await.unwrap();

    storage.append_message("default", "am-1", "owner", make_message("m-1", "first")).await.unwrap();
    storage.append_message("default", "am-1", "owner", make_message("m-2", "second")).await.unwrap();

    let task = storage.get_task("default", "am-1", "owner", None).await.unwrap().unwrap();
    assert_eq!(task.history().len(), 2);
    assert_eq!(task.history()[0].message_id, "m-1");
    assert_eq!(task.history()[1].message_id, "m-2");
}

// =========================================================
// P2-007: Append artifact
// =========================================================
pub async fn test_append_artifact(storage: &dyn A2aTaskStorage) {
    storage.create_task("default", "owner", make_task("aa-1", "ctx")).await.unwrap();

    storage
        .append_artifact("default", "aa-1", "owner", make_artifact("art-1", "chunk1"), false, false)
        .await
        .unwrap();

    let task = storage.get_task("default", "aa-1", "owner", None).await.unwrap().unwrap();
    assert_eq!(task.artifacts().len(), 1);
    assert_eq!(task.artifacts()[0].artifact_id, "art-1");
}

// =========================================================
// Owner isolation for mutation APIs
// =========================================================
pub async fn test_owner_isolation_mutations(storage: &dyn A2aTaskStorage) {
    storage.create_task("default", "alice", make_task("oim-1", "ctx")).await.unwrap();

    // Bob can't append message to Alice's task
    let result = storage
        .append_message("default", "oim-1", "bob", make_message("m-bad", "nope"))
        .await;
    assert!(result.is_err(), "Bob should not append message to Alice's task");

    // Bob can't append artifact to Alice's task
    let result = storage
        .append_artifact("default", "oim-1", "bob", make_artifact("a-bad", "nope"), false, false)
        .await;
    assert!(result.is_err(), "Bob should not append artifact to Alice's task");

    // Alice CAN append
    storage
        .append_message("default", "oim-1", "alice", make_message("m-ok", "yes"))
        .await
        .unwrap();
    storage
        .append_artifact("default", "oim-1", "alice", make_artifact("a-ok", "yes"), false, false)
        .await
        .unwrap();
}

// =========================================================
// P2-008: Artifact chunk semantics (append + last_chunk)
// =========================================================
pub async fn test_artifact_chunk_semantics(storage: &dyn A2aTaskStorage) {
    storage.create_task("default", "owner", make_task("acs-1", "ctx")).await.unwrap();

    // First chunk: append=false (new artifact)
    storage
        .append_artifact("default", "acs-1", "owner", make_artifact("art-1", "chunk-1"), false, false)
        .await
        .unwrap();

    let task = storage.get_task("default", "acs-1", "owner", None).await.unwrap().unwrap();
    assert_eq!(task.artifacts().len(), 1);
    assert_eq!(task.artifacts()[0].parts.len(), 1);

    // Second chunk: append=true, same artifact_id -> should append parts
    storage
        .append_artifact("default", "acs-1", "owner", make_artifact("art-1", "chunk-2"), true, false)
        .await
        .unwrap();

    let task = storage.get_task("default", "acs-1", "owner", None).await.unwrap().unwrap();
    assert_eq!(task.artifacts().len(), 1, "should still be 1 artifact");
    assert_eq!(task.artifacts()[0].parts.len(), 2, "should have 2 parts after append");

    // Third chunk: append=true, last_chunk=true -> append parts, mark complete
    storage
        .append_artifact("default", "acs-1", "owner", make_artifact("art-1", "chunk-3"), true, true)
        .await
        .unwrap();

    let task = storage.get_task("default", "acs-1", "owner", None).await.unwrap().unwrap();
    assert_eq!(task.artifacts()[0].parts.len(), 3, "should have 3 parts total");

    // New artifact with different ID: append=false
    storage
        .append_artifact("default", "acs-1", "owner", make_artifact("art-2", "separate"), false, true)
        .await
        .unwrap();

    let task = storage.get_task("default", "acs-1", "owner", None).await.unwrap().unwrap();
    assert_eq!(task.artifacts().len(), 2, "should have 2 distinct artifacts");
}

// =========================================================
// P2-014: Task count
// =========================================================
pub async fn test_task_count(storage: &dyn A2aTaskStorage) {
    let initial = storage.task_count().await.unwrap();
    storage.create_task("default", "owner", make_task("tc-1", "ctx")).await.unwrap();
    storage.create_task("default", "owner", make_task("tc-2", "ctx")).await.unwrap();
    assert_eq!(storage.task_count().await.unwrap(), initial + 2);

    storage.delete_task("default", "tc-1", "owner").await.unwrap();
    assert_eq!(storage.task_count().await.unwrap(), initial + 1);
}

// =========================================================
// P2-016: Push notification config CRUD
// =========================================================
pub async fn test_push_config_crud(storage: &dyn A2aPushNotificationStorage) {
    let config = turul_a2a_proto::TaskPushNotificationConfig {
        tenant: String::new(),
        id: String::new(), // server generates
        task_id: "task-1".to_string(),
        url: "https://example.com/webhook".to_string(),
        token: "tok-123".to_string(),
        authentication: None,
    };

    let created = storage.create_config("default", config).await.unwrap();
    assert!(!created.id.is_empty(), "server should generate config id");
    assert_eq!(created.task_id, "task-1");

    // Get
    let fetched = storage
        .get_config("default", "task-1", &created.id)
        .await
        .unwrap();
    assert!(fetched.is_some());

    // Get nonexistent
    let missing = storage.get_config("default", "task-1", "nope").await.unwrap();
    assert!(missing.is_none());
}

// =========================================================
// P2-017: Push config idempotent delete
// =========================================================
pub async fn test_push_config_idempotent_delete(storage: &dyn A2aPushNotificationStorage) {
    let config = turul_a2a_proto::TaskPushNotificationConfig {
        tenant: String::new(),
        id: String::new(),
        task_id: "task-del".to_string(),
        url: "https://example.com/hook".to_string(),
        token: String::new(),
        authentication: None,
    };

    let created = storage.create_config("default", config).await.unwrap();

    // Delete succeeds
    storage.delete_config("default", "task-del", &created.id).await.unwrap();
    // Second delete also succeeds (idempotent)
    storage.delete_config("default", "task-del", &created.id).await.unwrap();
    // Get returns None
    assert!(storage.get_config("default", "task-del", &created.id).await.unwrap().is_none());
}

// =========================================================
// P2-017: Push config list pagination
// =========================================================
pub async fn test_push_config_list_pagination(storage: &dyn A2aPushNotificationStorage) {
    // Create 5 configs for one task
    for i in 0..5 {
        let config = turul_a2a_proto::TaskPushNotificationConfig {
            tenant: String::new(),
            id: String::new(),
            task_id: "task-pg".to_string(),
            url: format!("https://example.com/hook-{i}"),
            token: String::new(),
            authentication: None,
        };
        storage.create_config("default", config).await.unwrap();
    }

    // Page through with page_size=2
    let mut all_ids = Vec::new();
    let mut page_token = None;

    loop {
        let page = storage
            .list_configs("default", "task-pg", page_token.as_deref(), Some(2))
            .await
            .unwrap();
        assert!(page.configs.len() <= 2);
        all_ids.extend(page.configs.iter().map(|c| c.id.clone()));

        if page.next_page_token.is_empty() {
            break;
        }
        page_token = Some(page.next_page_token);
    }

    assert_eq!(all_ids.len(), 5, "should collect all 5 configs across pages");
    let unique: std::collections::HashSet<_> = all_ids.iter().collect();
    assert_eq!(unique.len(), 5, "no duplicate configs");
}

// =========================================================
// P2-018: Push config tenant isolation
// =========================================================
pub async fn test_push_config_tenant_isolation(storage: &dyn A2aPushNotificationStorage) {
    let config = turul_a2a_proto::TaskPushNotificationConfig {
        tenant: String::new(),
        id: String::new(),
        task_id: "task-iso".to_string(),
        url: "https://example.com/hook".to_string(),
        token: String::new(),
        authentication: None,
    };

    let created = storage.create_config("tenant-a", config).await.unwrap();

    // Tenant B can't see Tenant A's config
    let result = storage
        .get_config("tenant-b", "task-iso", &created.id)
        .await
        .unwrap();
    assert!(result.is_none());
}

// =========================================================
// Event store parity tests
// =========================================================

fn make_status_event(state: &str) -> StreamEvent {
    StreamEvent::StatusUpdate {
        status_update: crate::streaming::StatusUpdatePayload {
            task_id: String::new(),
            context_id: String::new(),
            status: serde_json::json!({"state": state}),
        },
    }
}

pub async fn test_event_append_and_retrieve(storage: &dyn A2aEventStore) {
    let seq1 = storage
        .append_event("default", "evt-1", make_status_event("WORKING"))
        .await
        .unwrap();
    assert_eq!(seq1, 1);

    let seq2 = storage
        .append_event("default", "evt-1", make_status_event("COMPLETED"))
        .await
        .unwrap();
    assert_eq!(seq2, 2);

    // Get all events
    let events = storage
        .get_events_after("default", "evt-1", 0)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, 1);
    assert_eq!(events[1].0, 2);

    // Get events after seq 1
    let events = storage
        .get_events_after("default", "evt-1", 1)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, 2);

    // Get events after seq 2 (none left)
    let events = storage
        .get_events_after("default", "evt-1", 2)
        .await
        .unwrap();
    assert!(events.is_empty());
}

pub async fn test_event_monotonic_ordering(storage: &dyn A2aEventStore) {
    for i in 1..=5 {
        let seq = storage
            .append_event("default", "ord-1", make_status_event(&format!("state-{i}")))
            .await
            .unwrap();
        assert_eq!(seq, i as u64);
    }

    let events = storage
        .get_events_after("default", "ord-1", 0)
        .await
        .unwrap();
    // Must be in order
    for (i, (seq, _)) in events.iter().enumerate() {
        assert_eq!(*seq, (i + 1) as u64);
    }
}

pub async fn test_event_per_task_isolation(storage: &dyn A2aEventStore) {
    // Events for task A
    storage.append_event("default", "iso-a", make_status_event("A1")).await.unwrap();
    storage.append_event("default", "iso-a", make_status_event("A2")).await.unwrap();

    // Events for task B
    storage.append_event("default", "iso-b", make_status_event("B1")).await.unwrap();

    // Task A has 2 events
    let a_events = storage.get_events_after("default", "iso-a", 0).await.unwrap();
    assert_eq!(a_events.len(), 2);

    // Task B has 1 event
    let b_events = storage.get_events_after("default", "iso-b", 0).await.unwrap();
    assert_eq!(b_events.len(), 1);

    // Sequences are per-task
    assert_eq!(a_events[0].0, 1);
    assert_eq!(b_events[0].0, 1);
}

pub async fn test_event_tenant_isolation(storage: &dyn A2aEventStore) {
    storage.append_event("tenant-x", "iso-t", make_status_event("X")).await.unwrap();
    storage.append_event("tenant-y", "iso-t", make_status_event("Y")).await.unwrap();

    // Same task_id, different tenants — isolated
    let x_events = storage.get_events_after("tenant-x", "iso-t", 0).await.unwrap();
    assert_eq!(x_events.len(), 1);

    let y_events = storage.get_events_after("tenant-y", "iso-t", 0).await.unwrap();
    assert_eq!(y_events.len(), 1);
}

pub async fn test_event_latest_sequence(storage: &dyn A2aEventStore) {
    assert_eq!(storage.latest_sequence("default", "seq-t").await.unwrap(), 0);

    storage.append_event("default", "seq-t", make_status_event("S1")).await.unwrap();
    assert_eq!(storage.latest_sequence("default", "seq-t").await.unwrap(), 1);

    storage.append_event("default", "seq-t", make_status_event("S2")).await.unwrap();
    storage.append_event("default", "seq-t", make_status_event("S3")).await.unwrap();
    assert_eq!(storage.latest_sequence("default", "seq-t").await.unwrap(), 3);
}

pub async fn test_event_empty_task(storage: &dyn A2aEventStore) {
    let events = storage.get_events_after("default", "nonexistent", 0).await.unwrap();
    assert!(events.is_empty());
    assert_eq!(storage.latest_sequence("default", "nonexistent").await.unwrap(), 0);
}

// =========================================================
// Atomic store parity tests (ADR-009 §10)
// =========================================================

/// Helper to create a status event for atomic tests.
fn make_status_event_for(task_id: &str, context_id: &str, state: &str) -> StreamEvent {
    StreamEvent::StatusUpdate {
        status_update: crate::streaming::StatusUpdatePayload {
            task_id: task_id.to_string(),
            context_id: context_id.to_string(),
            status: serde_json::json!({"state": state}),
        },
    }
}

/// AT-001: create_task_with_events writes task and events atomically.
/// Both are readable after the call.
pub async fn test_atomic_create_task_with_events(
    atomic: &dyn A2aAtomicStore,
    tasks: &dyn A2aTaskStorage,
    events: &dyn A2aEventStore,
) {
    let task = make_task("at-create-1", "ctx-1");
    let evts = vec![
        make_status_event_for("at-create-1", "ctx-1", "TASK_STATE_SUBMITTED"),
    ];

    let (created, seqs) = atomic
        .create_task_with_events("default", "owner-1", task, evts)
        .await
        .unwrap();

    assert_eq!(created.id(), "at-create-1");
    assert_eq!(seqs.len(), 1);
    assert_eq!(seqs[0], 1);

    // Task is readable via task storage
    let fetched = tasks
        .get_task("default", "at-create-1", "owner-1", None)
        .await
        .unwrap()
        .expect("Task should exist after atomic create");
    assert_eq!(fetched.id(), "at-create-1");

    // Events are readable via event store
    let stored_events = events
        .get_events_after("default", "at-create-1", 0)
        .await
        .unwrap();
    assert_eq!(stored_events.len(), 1);
    assert_eq!(stored_events[0].0, 1);
}

/// AT-002: update_task_status_with_events updates status and appends events atomically.
pub async fn test_atomic_update_status_with_events(
    atomic: &dyn A2aAtomicStore,
    tasks: &dyn A2aTaskStorage,
    events: &dyn A2aEventStore,
) {
    // Setup: create task first
    let task = make_task("at-status-1", "ctx-1");
    tasks.create_task("default", "owner-1", task).await.unwrap();

    // Atomic status update with event
    let evts = vec![
        make_status_event_for("at-status-1", "ctx-1", "TASK_STATE_WORKING"),
    ];
    let (updated, seqs) = atomic
        .update_task_status_with_events(
            "default", "at-status-1", "owner-1",
            TaskStatus::new(TaskState::Working), evts,
        )
        .await
        .unwrap();

    assert_eq!(seqs.len(), 1);
    assert_eq!(updated.status().unwrap().state().unwrap(), TaskState::Working);

    // Verify via reads
    let fetched = tasks
        .get_task("default", "at-status-1", "owner-1", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.status().unwrap().state().unwrap(), TaskState::Working);

    let stored_events = events
        .get_events_after("default", "at-status-1", 0)
        .await
        .unwrap();
    assert_eq!(stored_events.len(), 1);
}

/// AT-003: update_task_status_with_events rejects invalid transitions
/// and neither task nor events are modified.
pub async fn test_atomic_status_rejects_invalid_transition(
    atomic: &dyn A2aAtomicStore,
    tasks: &dyn A2aTaskStorage,
    events: &dyn A2aEventStore,
) {
    // Setup: create a completed task
    let task = make_task("at-invalid-1", "ctx-1");
    tasks.create_task("default", "owner-1", task).await.unwrap();
    tasks
        .update_task_status("default", "at-invalid-1", "owner-1", TaskStatus::new(TaskState::Working))
        .await
        .unwrap();
    tasks
        .update_task_status("default", "at-invalid-1", "owner-1", TaskStatus::new(TaskState::Completed))
        .await
        .unwrap();

    // Attempt invalid transition (Completed → Working) with event
    let evts = vec![
        make_status_event_for("at-invalid-1", "ctx-1", "TASK_STATE_WORKING"),
    ];
    let result = atomic
        .update_task_status_with_events(
            "default", "at-invalid-1", "owner-1",
            TaskStatus::new(TaskState::Working), evts,
        )
        .await;

    assert!(result.is_err(), "Invalid transition should fail");

    // Verify task is still Completed (not modified)
    let fetched = tasks
        .get_task("default", "at-invalid-1", "owner-1", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.status().unwrap().state().unwrap(), TaskState::Completed);

    // Verify no events were written
    let stored_events = events
        .get_events_after("default", "at-invalid-1", 0)
        .await
        .unwrap();
    assert!(stored_events.is_empty(), "No events should be written on failed atomic op");
}

/// AT-004: update_task_with_events replaces task and appends events atomically.
pub async fn test_atomic_update_task_with_events(
    atomic: &dyn A2aAtomicStore,
    tasks: &dyn A2aTaskStorage,
    events: &dyn A2aEventStore,
) {
    // Setup: create task
    let task = make_task("at-update-1", "ctx-1");
    tasks.create_task("default", "owner-1", task).await.unwrap();

    // Mutate task (add artifact, change status) and append events
    let mut updated_task = tasks
        .get_task("default", "at-update-1", "owner-1", None)
        .await
        .unwrap()
        .unwrap();
    updated_task.set_status(TaskStatus::new(TaskState::Working));
    updated_task.push_text_artifact("art-1", "Result", "some output");
    updated_task.complete();

    let evts = vec![
        make_status_event_for("at-update-1", "ctx-1", "TASK_STATE_WORKING"),
        make_status_event_for("at-update-1", "ctx-1", "TASK_STATE_COMPLETED"),
    ];
    let seqs = atomic
        .update_task_with_events("default", "owner-1", updated_task, evts)
        .await
        .unwrap();

    assert_eq!(seqs.len(), 2);
    assert_eq!(seqs[0], 1);
    assert_eq!(seqs[1], 2);

    // Verify task has updated state
    let fetched = tasks
        .get_task("default", "at-update-1", "owner-1", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.status().unwrap().state().unwrap(), TaskState::Completed);
    assert!(!fetched.artifacts().is_empty());

    // Verify events
    let stored_events = events
        .get_events_after("default", "at-update-1", 0)
        .await
        .unwrap();
    assert_eq!(stored_events.len(), 2);
}

/// AT-005: Atomic operations enforce owner isolation.
pub async fn test_atomic_owner_isolation(
    atomic: &dyn A2aAtomicStore,
    tasks: &dyn A2aTaskStorage,
) {
    // Create task owned by "alice"
    let task = make_task("at-owner-1", "ctx-1");
    tasks.create_task("default", "alice", task).await.unwrap();

    // "bob" cannot update status with events
    let evts = vec![
        make_status_event_for("at-owner-1", "ctx-1", "TASK_STATE_WORKING"),
    ];
    let result = atomic
        .update_task_status_with_events(
            "default", "at-owner-1", "bob",
            TaskStatus::new(TaskState::Working), evts,
        )
        .await;
    assert!(result.is_err(), "Wrong owner should fail");

    // "bob" cannot full-update with events
    let mut fake_task = make_task("at-owner-1", "ctx-1");
    fake_task.set_status(TaskStatus::new(TaskState::Working));
    let evts = vec![
        make_status_event_for("at-owner-1", "ctx-1", "TASK_STATE_WORKING"),
    ];
    let result = atomic
        .update_task_with_events("default", "bob", fake_task, evts)
        .await;
    assert!(result.is_err(), "Wrong owner should fail for update_task_with_events");
}

/// AT-006: Tenant isolation for atomic operations.
pub async fn test_atomic_tenant_isolation(
    atomic: &dyn A2aAtomicStore,
    tasks: &dyn A2aTaskStorage,
    events: &dyn A2aEventStore,
) {
    // Create tasks under different tenants
    let task_a = make_task("at-tenant-1", "ctx-1");
    let task_b = make_task("at-tenant-1", "ctx-1"); // Same task ID, different tenant

    atomic
        .create_task_with_events(
            "tenant-a", "owner-1", task_a,
            vec![make_status_event_for("at-tenant-1", "ctx-1", "SUBMITTED_A")],
        )
        .await
        .unwrap();

    atomic
        .create_task_with_events(
            "tenant-b", "owner-1", task_b,
            vec![make_status_event_for("at-tenant-1", "ctx-1", "SUBMITTED_B")],
        )
        .await
        .unwrap();

    // Events are isolated by tenant
    let a_events = events.get_events_after("tenant-a", "at-tenant-1", 0).await.unwrap();
    let b_events = events.get_events_after("tenant-b", "at-tenant-1", 0).await.unwrap();
    assert_eq!(a_events.len(), 1);
    assert_eq!(b_events.len(), 1);

    // Tasks are isolated by tenant
    let a_task = tasks.get_task("tenant-a", "at-tenant-1", "owner-1", None).await.unwrap();
    let b_task = tasks.get_task("tenant-b", "at-tenant-1", "owner-1", None).await.unwrap();
    assert!(a_task.is_some());
    assert!(b_task.is_some());

    // Cross-tenant invisible
    let cross = tasks.get_task("tenant-a", "at-tenant-1", "owner-1", None).await.unwrap();
    assert!(cross.is_some()); // Own tenant visible
    let wrong_tenant_events = events.get_events_after("tenant-c", "at-tenant-1", 0).await.unwrap();
    assert!(wrong_tenant_events.is_empty()); // Non-existent tenant has no events
}

/// AT-007: create_task_with_events with empty events vec creates task but no events.
pub async fn test_atomic_create_with_empty_events(
    atomic: &dyn A2aAtomicStore,
    tasks: &dyn A2aTaskStorage,
    events: &dyn A2aEventStore,
) {
    let task = make_task("at-empty-1", "ctx-1");
    let (created, seqs) = atomic
        .create_task_with_events("default", "owner-1", task, vec![])
        .await
        .unwrap();

    assert_eq!(created.id(), "at-empty-1");
    assert!(seqs.is_empty());

    // Task exists
    let fetched = tasks.get_task("default", "at-empty-1", "owner-1", None).await.unwrap();
    assert!(fetched.is_some());

    // No events
    let stored = events.get_events_after("default", "at-empty-1", 0).await.unwrap();
    assert!(stored.is_empty());
}

/// AT-008: Event sequences continue correctly across atomic and non-atomic operations
/// under serial access.
pub async fn test_atomic_sequence_continuity(
    atomic: &dyn A2aAtomicStore,
    events: &dyn A2aEventStore,
) {
    // Create task with 1 event via atomic
    let task = make_task("at-seq-1", "ctx-1");
    let (_, seqs) = atomic
        .create_task_with_events(
            "default", "owner-1", task,
            vec![make_status_event_for("at-seq-1", "ctx-1", "SUBMITTED")],
        )
        .await
        .unwrap();
    assert_eq!(seqs, vec![1]);

    // Append event directly via event store
    let seq = events
        .append_event("default", "at-seq-1", make_status_event("WORKING"))
        .await
        .unwrap();
    assert_eq!(seq, 2);

    // Atomic update with event — should continue from 3
    let mut task2 = make_task("at-seq-1", "ctx-1");
    task2.set_status(TaskStatus::new(TaskState::Working));
    task2.complete();
    let seqs2 = atomic
        .update_task_with_events(
            "default", "owner-1", task2,
            vec![make_status_event_for("at-seq-1", "ctx-1", "COMPLETED")],
        )
        .await
        .unwrap();
    assert_eq!(seqs2, vec![3]);

    // Verify all 3 events in order
    let all = events.get_events_after("default", "at-seq-1", 0).await.unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].0, 1);
    assert_eq!(all[1].0, 2);
    assert_eq!(all[2].0, 3);
}

/// AT-009: Concurrent atomic and non-atomic event appends produce unique, monotonic sequences.
///
/// This test requires `Arc`-wrapped storage, so it takes concrete types
/// rather than trait objects. Backend-specific test modules can call it
/// with their own storage type.
pub async fn test_atomic_concurrent_sequence_integrity<S>(storage: std::sync::Arc<S>)
where
    S: A2aAtomicStore + A2aEventStore + A2aTaskStorage + Send + Sync + 'static,
{
    // Create task first
    let task = make_task("at-conc-1", "ctx-1");
    storage
        .create_task("default", "owner-1", task)
        .await
        .unwrap();

    let total_atomic = 10usize;
    let total_non_atomic = 10usize;
    let mut handles = Vec::new();

    // Spawn concurrent atomic event appends (via update_task_with_events)
    for _ in 0..total_atomic {
        let s = storage.clone();
        handles.push(tokio::spawn(async move {
            // Read current task, append events atomically
            let task = s
                .get_task("default", "at-conc-1", "owner-1", None)
                .await
                .unwrap()
                .unwrap();
            let evts = vec![make_status_event("atomic")];
            s.update_task_with_events("default", "owner-1", task, evts)
                .await
                .unwrap()
        }));
    }

    // Spawn concurrent non-atomic event appends
    for _ in 0..total_non_atomic {
        let s = storage.clone();
        handles.push(tokio::spawn(async move {
            let seq = s
                .append_event("default", "at-conc-1", make_status_event("non-atomic"))
                .await
                .unwrap();
            vec![seq]
        }));
    }

    // Collect all assigned sequences
    let mut all_seqs = Vec::new();
    for handle in handles {
        let seqs = handle.await.unwrap();
        all_seqs.extend(seqs);
    }

    // All sequences must be unique
    all_seqs.sort();
    let before_dedup = all_seqs.len();
    all_seqs.dedup();
    assert_eq!(
        all_seqs.len(),
        before_dedup,
        "All sequences must be unique — found duplicates"
    );

    // Total must match
    assert_eq!(
        all_seqs.len(),
        total_atomic + total_non_atomic,
        "Expected {} events, got {}",
        total_atomic + total_non_atomic,
        all_seqs.len()
    );

    // Events in store must match
    let stored = storage
        .get_events_after("default", "at-conc-1", 0)
        .await
        .unwrap();
    assert_eq!(stored.len(), total_atomic + total_non_atomic);

    // Sequences in store must be monotonically ordered
    for window in stored.windows(2) {
        assert!(
            window[0].0 < window[1].0,
            "Events must be monotonically ordered in store: {} >= {}",
            window[0].0,
            window[1].0,
        );
    }
}

// =========================================================
// Terminal-write CAS (ADR-010 §7.1): single-terminal-writer parity
// =========================================================
//
// For each backend, `A2aAtomicStore::update_task_status_with_events` MUST:
// - accept exactly one terminal write per task,
// - return `A2aStorageError::TerminalStateAlreadySet` to all other
//   concurrent terminal-write attempts,
// - NOT append events from losing calls to the event store,
// - keep the task's persisted state equal to the winner's write.
//
// `InvalidTransition` MUST remain a distinct variant (non-terminal
// illegal transitions), not be conflated with `TerminalStateAlreadySet`.

use std::sync::Arc;

/// Need an Arc-friendly view of the backend so spawned tasks can share
/// it without cloning the underlying struct. Each backend's test entry
/// point wraps its storage in `Arc` before calling these helpers.
pub async fn test_terminal_cas_single_winner_on_concurrent_terminals(
    atomic: Arc<dyn A2aAtomicStore>,
    tasks: Arc<dyn A2aTaskStorage>,
    events: Arc<dyn A2aEventStore>,
) {
    // Setup: create task in Submitted state, advance to Working.
    let task = make_task("cas-1", "ctx-cas");
    tasks
        .create_task("default", "owner-cas", task)
        .await
        .unwrap();
    let prep_events = vec![make_status_event_for(
        "cas-1",
        "ctx-cas",
        "TASK_STATE_WORKING",
    )];
    atomic
        .update_task_status_with_events(
            "default",
            "cas-1",
            "owner-cas",
            TaskStatus::new(TaskState::Working),
            prep_events,
        )
        .await
        .unwrap();

    // Event sequence at race start
    let pre_race_events = events
        .get_events_after("default", "cas-1", 0)
        .await
        .unwrap();
    let pre_race_count = pre_race_events.len();

    // Race three concurrent terminal writers: Completed, Failed, Canceled.
    // Per the A2A v1.0 state machine, Working → { Completed, Failed,
    // Canceled, InputRequired, AuthRequired } are the permitted exits;
    // REJECTED is only reachable from Submitted (refuse-at-intake). With
    // all three candidates being spec-valid Working-exits, the winner is
    // decided solely by the atomic store's CAS.
    let terminals = [
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Canceled,
    ];
    let mut handles = Vec::with_capacity(terminals.len());
    let barrier = Arc::new(tokio::sync::Barrier::new(terminals.len() + 1));
    for (i, terminal) in terminals.into_iter().enumerate() {
        let atomic = Arc::clone(&atomic);
        let barrier = Arc::clone(&barrier);
        let evt = make_status_event_for(
            "cas-1",
            "ctx-cas",
            crate::storage::terminal_cas::task_state_wire_name(terminal),
        );
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let result = atomic
                .update_task_status_with_events(
                    "default",
                    "cas-1",
                    "owner-cas",
                    TaskStatus::new(terminal),
                    vec![evt],
                )
                .await;
            (i, terminal, result)
        }));
    }
    // Release all racers simultaneously.
    barrier.wait().await;

    let mut winners = Vec::new();
    let mut losers = Vec::new();
    for handle in handles {
        let (_i, terminal, result) = handle.await.unwrap();
        match result {
            Ok((task, seqs)) => winners.push((terminal, task, seqs)),
            Err(crate::storage::A2aStorageError::TerminalStateAlreadySet {
                task_id,
                current_state,
            }) => {
                losers.push((terminal, task_id, current_state));
            }
            Err(other) => panic!(
                "unexpected error from terminal-CAS attempt ({terminal:?}): {other:?}"
            ),
        }
    }

    // Exactly one winner, rest lose with TerminalStateAlreadySet.
    assert_eq!(winners.len(), 1, "exactly one terminal write must win");
    assert_eq!(
        losers.len(),
        terminals.len() - 1,
        "all non-winners must surface TerminalStateAlreadySet"
    );

    let (winning_terminal, _winning_task, winning_seqs) = &winners[0];
    assert_eq!(winning_seqs.len(), 1, "winner appended exactly one event");

    // Losers' `current_state` points at the winning terminal's wire name.
    // Under READ-COMMITTED-ish timing, the loser may have read a stale
    // state before the winner committed; but the CAS detection path always
    // re-reads after the failed UPDATE to classify the failure. Either way,
    // losers must report A TERMINAL state.
    let terminal_wire_names = [
        "TASK_STATE_COMPLETED",
        "TASK_STATE_FAILED",
        "TASK_STATE_CANCELED",
    ];
    for (_t, task_id, current_state) in &losers {
        assert_eq!(task_id, "cas-1", "loser should carry the task_id");
        assert!(
            terminal_wire_names.contains(&current_state.as_str()),
            "loser's current_state must be a terminal wire name: got {current_state}"
        );
    }

    // Persisted state matches the winner.
    let fetched = tasks
        .get_task("default", "cas-1", "owner-cas", None)
        .await
        .unwrap()
        .expect("task still exists");
    let persisted_state = fetched.status().unwrap().state().unwrap();
    assert_eq!(
        persisted_state, *winning_terminal,
        "persisted state must equal winner's write"
    );

    // Event store holds exactly one additional event (the winner's).
    let post_race_events = events
        .get_events_after("default", "cas-1", 0)
        .await
        .unwrap();
    assert_eq!(
        post_race_events.len(),
        pre_race_count + 1,
        "exactly one terminal event appended; losers must NOT persist events"
    );
}

/// AT-CAS-001b: same race from SUBMITTED, proving REJECTED participates
/// in the single-terminal-writer CAS.
///
/// The first race (`test_terminal_cas_single_winner_on_concurrent_terminals`)
/// starts from WORKING, where the valid terminal exits are COMPLETED /
/// FAILED / CANCELED — REJECTED is not a legal exit from WORKING per the
/// A2A v1.0 state machine (only from SUBMITTED). This test covers the gap:
/// race FAILED / CANCELED / REJECTED from SUBMITTED, proving that all four
/// terminal variants (COMPLETED covered in the other test, FAILED /
/// CANCELED / REJECTED here) participate in the CAS invariant.
pub async fn test_terminal_cas_single_winner_from_submitted_includes_rejected(
    atomic: Arc<dyn A2aAtomicStore>,
    tasks: Arc<dyn A2aTaskStorage>,
    events: Arc<dyn A2aEventStore>,
) {
    // Create task in SUBMITTED (no state transition needed; `create_task`
    // defaults to SUBMITTED).
    let task = make_task("cas-sub-1", "ctx-cas-sub");
    tasks
        .create_task("default", "owner-cas-sub", task)
        .await
        .unwrap();

    let pre_race_events = events
        .get_events_after("default", "cas-sub-1", 0)
        .await
        .unwrap();
    let pre_race_count = pre_race_events.len();

    // All three are valid terminal exits from SUBMITTED per the state
    // machine (SUBMITTED → { Working, Rejected, Failed, Canceled }).
    let terminals = [
        TaskState::Rejected,
        TaskState::Failed,
        TaskState::Canceled,
    ];
    let mut handles = Vec::with_capacity(terminals.len());
    let barrier = Arc::new(tokio::sync::Barrier::new(terminals.len() + 1));
    for (i, terminal) in terminals.into_iter().enumerate() {
        let atomic = Arc::clone(&atomic);
        let barrier = Arc::clone(&barrier);
        let evt = make_status_event_for(
            "cas-sub-1",
            "ctx-cas-sub",
            crate::storage::terminal_cas::task_state_wire_name(terminal),
        );
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let result = atomic
                .update_task_status_with_events(
                    "default",
                    "cas-sub-1",
                    "owner-cas-sub",
                    TaskStatus::new(terminal),
                    vec![evt],
                )
                .await;
            (i, terminal, result)
        }));
    }
    barrier.wait().await;

    let mut winners = Vec::new();
    let mut losers = Vec::new();
    for handle in handles {
        let (_i, terminal, result) = handle.await.unwrap();
        match result {
            Ok((task, seqs)) => winners.push((terminal, task, seqs)),
            Err(crate::storage::A2aStorageError::TerminalStateAlreadySet {
                task_id,
                current_state,
            }) => losers.push((terminal, task_id, current_state)),
            Err(other) => panic!(
                "unexpected error from terminal-CAS attempt ({terminal:?}) from SUBMITTED: {other:?}"
            ),
        }
    }

    assert_eq!(winners.len(), 1, "exactly one terminal write must win from SUBMITTED");
    assert_eq!(
        losers.len(),
        terminals.len() - 1,
        "all non-winners must surface TerminalStateAlreadySet"
    );

    let (winning_terminal, _winning_task, winning_seqs) = &winners[0];
    assert_eq!(winning_seqs.len(), 1, "winner appended exactly one event");

    // Explicit invariant: REJECTED either wins or is one of the
    // CAS losers — but never a non-CAS error class. This is the
    // property that was previously unproven.
    let rejected_outcome_ok = winners.iter().any(|(t, _, _)| *t == TaskState::Rejected)
        || losers.iter().any(|(t, _, _)| *t == TaskState::Rejected);
    assert!(
        rejected_outcome_ok,
        "REJECTED must participate in the CAS race — either as the winner or a TerminalStateAlreadySet loser"
    );

    let terminal_wire_names = [
        "TASK_STATE_FAILED",
        "TASK_STATE_CANCELED",
        "TASK_STATE_REJECTED",
    ];
    for (_t, task_id, current_state) in &losers {
        assert_eq!(task_id, "cas-sub-1", "loser carries the task_id");
        assert!(
            terminal_wire_names.contains(&current_state.as_str()),
            "loser current_state must be a terminal wire name from the racing set: got {current_state}"
        );
    }

    let fetched = tasks
        .get_task("default", "cas-sub-1", "owner-cas-sub", None)
        .await
        .unwrap()
        .expect("task still exists");
    assert_eq!(
        fetched.status().unwrap().state().unwrap(),
        *winning_terminal,
        "persisted state matches winner"
    );

    let post_race_events = events
        .get_events_after("default", "cas-sub-1", 0)
        .await
        .unwrap();
    assert_eq!(
        post_race_events.len(),
        pre_race_count + 1,
        "exactly one terminal event appended; losers must NOT persist events"
    );
}

/// AT-CAS-002: terminal-already-set rejects a follow-up terminal write
/// sequentially, not just under race. Sanity check that the CAS contract
/// is not only about concurrency — a later write to a terminal row also
/// fails with TerminalStateAlreadySet.
pub async fn test_terminal_cas_rejects_sequential_second_terminal(
    atomic: &dyn A2aAtomicStore,
    tasks: &dyn A2aTaskStorage,
    events: &dyn A2aEventStore,
) {
    let task = make_task("cas-seq-1", "ctx-cas-seq");
    tasks
        .create_task("default", "owner-seq", task)
        .await
        .unwrap();
    atomic
        .update_task_status_with_events(
            "default",
            "cas-seq-1",
            "owner-seq",
            TaskStatus::new(TaskState::Working),
            vec![make_status_event_for(
                "cas-seq-1",
                "ctx-cas-seq",
                "TASK_STATE_WORKING",
            )],
        )
        .await
        .unwrap();
    atomic
        .update_task_status_with_events(
            "default",
            "cas-seq-1",
            "owner-seq",
            TaskStatus::new(TaskState::Completed),
            vec![make_status_event_for(
                "cas-seq-1",
                "ctx-cas-seq",
                "TASK_STATE_COMPLETED",
            )],
        )
        .await
        .unwrap();

    let event_count_after_first_terminal =
        events.get_events_after("default", "cas-seq-1", 0).await.unwrap().len();

    // Second terminal — must fail.
    let second = atomic
        .update_task_status_with_events(
            "default",
            "cas-seq-1",
            "owner-seq",
            TaskStatus::new(TaskState::Canceled),
            vec![make_status_event_for(
                "cas-seq-1",
                "ctx-cas-seq",
                "TASK_STATE_CANCELED",
            )],
        )
        .await;
    match second {
        Err(crate::storage::A2aStorageError::TerminalStateAlreadySet {
            task_id,
            current_state,
        }) => {
            assert_eq!(task_id, "cas-seq-1");
            assert_eq!(current_state, "TASK_STATE_COMPLETED");
        }
        other => panic!(
            "expected TerminalStateAlreadySet on second terminal write, got {other:?}"
        ),
    }

    // No new event appended.
    let final_events = events
        .get_events_after("default", "cas-seq-1", 0)
        .await
        .unwrap();
    assert_eq!(
        final_events.len(),
        event_count_after_first_terminal,
        "second-terminal loser must not persist events"
    );
}

/// Terminal-preservation CAS on `update_task_with_events` (ADR-010 §7.1
/// extension): once the persisted task is terminal, any subsequent
/// full-task replacement MUST be rejected with
/// `TerminalStateAlreadySet` and MUST NOT append any events.
///
/// This protects [`crate::event_sink::EventSink::emit_artifact`]'s
/// read-mutate-write path from silently overwriting a concurrently
/// committed terminal. Without this CAS, an artifact emit that read the
/// task while it was WORKING and wrote back after a terminal commit
/// would roll back the terminal.
pub async fn test_update_task_with_events_rejects_terminal_already_set(
    atomic: &dyn A2aAtomicStore,
    tasks: &dyn A2aTaskStorage,
    events: &dyn A2aEventStore,
) {
    // Arrange: create task, move to WORKING, then commit a terminal.
    let task = make_task("upd-tpc-1", "ctx-upd-tpc");
    tasks
        .create_task("default", "owner-upd", task)
        .await
        .unwrap();
    atomic
        .update_task_status_with_events(
            "default",
            "upd-tpc-1",
            "owner-upd",
            TaskStatus::new(TaskState::Working),
            vec![make_status_event_for(
                "upd-tpc-1",
                "ctx-upd-tpc",
                "TASK_STATE_WORKING",
            )],
        )
        .await
        .unwrap();
    atomic
        .update_task_status_with_events(
            "default",
            "upd-tpc-1",
            "owner-upd",
            TaskStatus::new(TaskState::Completed),
            vec![make_status_event_for(
                "upd-tpc-1",
                "ctx-upd-tpc",
                "TASK_STATE_COMPLETED",
            )],
        )
        .await
        .unwrap();

    let event_count_before = events
        .get_events_after("default", "upd-tpc-1", 0)
        .await
        .unwrap()
        .len();

    // Act: attempt a full-task replacement that pretends the task is still
    // WORKING — the exact shape of the EventSink::emit_artifact
    // read-mutate-write race. The argument task carries WORKING;
    // the persisted task is COMPLETED.
    let stale = Task::new("upd-tpc-1", TaskStatus::new(TaskState::Working))
        .with_context_id("ctx-upd-tpc");
    let result = atomic
        .update_task_with_events(
            "default",
            "owner-upd",
            stale,
            vec![make_status_event_for(
                "upd-tpc-1",
                "ctx-upd-tpc",
                "TASK_STATE_WORKING",
            )],
        )
        .await;

    // Assert: rejected with TerminalStateAlreadySet and the persisted
    // state reported in proto wire form.
    match result {
        Err(crate::storage::A2aStorageError::TerminalStateAlreadySet {
            task_id,
            current_state,
        }) => {
            assert_eq!(task_id, "upd-tpc-1");
            assert_eq!(current_state, "TASK_STATE_COMPLETED");
        }
        Ok(_) => panic!(
            "update_task_with_events must reject writes to a task whose \
             persisted state is terminal"
        ),
        Err(other) => panic!("expected TerminalStateAlreadySet, got {other:?}"),
    }

    // Assert: no events committed by the rejected write.
    let event_count_after = events
        .get_events_after("default", "upd-tpc-1", 0)
        .await
        .unwrap()
        .len();
    assert_eq!(
        event_count_after, event_count_before,
        "rejected update_task_with_events must not append events"
    );

    // Assert: persisted task state is unchanged.
    let persisted = tasks
        .get_task("default", "upd-tpc-1", "owner-upd", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        persisted
            .status()
            .unwrap()
            .state()
            .unwrap(),
        TaskState::Completed,
        "rejected update_task_with_events must not mutate task state"
    );
}

// =========================================================
// ADR-012 cancel-marker parity
// =========================================================

/// CS-001: `set_cancel_requested` + `supervisor_get_cancel_requested`
/// round-trip. After set, the marker is observable via the supervisor
/// read path.
pub async fn test_cancel_marker_roundtrip(
    tasks: &dyn A2aTaskStorage,
    supervisor: &dyn crate::storage::A2aCancellationSupervisor,
) {
    let task = make_task("cm-rt-1", "ctx");
    tasks.create_task("default", "owner-cm", task).await.unwrap();
    // Advance to Working so the marker write is eligible.
    tasks
        .update_task_status("default", "cm-rt-1", "owner-cm", TaskStatus::new(TaskState::Working))
        .await
        .unwrap();

    // Before: marker is false.
    assert!(!supervisor
        .supervisor_get_cancel_requested("default", "cm-rt-1")
        .await
        .unwrap());

    // Set the marker (owner-scoped).
    tasks
        .set_cancel_requested("default", "cm-rt-1", "owner-cm")
        .await
        .unwrap();

    // After: marker is true.
    assert!(supervisor
        .supervisor_get_cancel_requested("default", "cm-rt-1")
        .await
        .unwrap());

    // Idempotent: setting again is a successful no-op.
    tasks
        .set_cancel_requested("default", "cm-rt-1", "owner-cm")
        .await
        .unwrap();
    assert!(supervisor
        .supervisor_get_cancel_requested("default", "cm-rt-1")
        .await
        .unwrap());
}

/// CS-002: `supervisor_list_cancel_requested` returns only the marked
/// subset and excludes terminal tasks. Tests the batch API parity.
pub async fn test_supervisor_list_cancel_requested_parity(
    tasks: &dyn A2aTaskStorage,
    atomic: &dyn A2aAtomicStore,
    supervisor: &dyn crate::storage::A2aCancellationSupervisor,
) {
    // Create 4 tasks: t1 marked+Working, t2 unmarked+Working,
    // t3 marked+Completed (terminal → excluded), t4 nonexistent.
    for id in ["cm-list-1", "cm-list-2", "cm-list-3"] {
        let task = make_task(id, "ctx-list");
        tasks.create_task("default", "owner-list", task).await.unwrap();
        // Advance each into Working so we can mark and set terminal later.
        atomic
            .update_task_status_with_events(
                "default",
                id,
                "owner-list",
                TaskStatus::new(TaskState::Working),
                vec![],
            )
            .await
            .unwrap();
    }

    // Mark t1 and t3.
    tasks
        .set_cancel_requested("default", "cm-list-1", "owner-list")
        .await
        .unwrap();
    tasks
        .set_cancel_requested("default", "cm-list-3", "owner-list")
        .await
        .unwrap();
    // Transition t3 to COMPLETED.
    atomic
        .update_task_status_with_events(
            "default",
            "cm-list-3",
            "owner-list",
            TaskStatus::new(TaskState::Completed),
            vec![],
        )
        .await
        .unwrap();

    let result = supervisor
        .supervisor_list_cancel_requested(
            "default",
            &[
                "cm-list-1".to_string(),
                "cm-list-2".to_string(),
                "cm-list-3".to_string(),
                "cm-list-4".to_string(),
            ],
        )
        .await
        .unwrap();

    // Only t1 is marked AND non-terminal. t2 unmarked, t3 terminal, t4 absent.
    assert_eq!(result, vec!["cm-list-1".to_string()]);
}

/// AT-CAS-003: distinct error variants — `InvalidTransition` is NOT
/// conflated with `TerminalStateAlreadySet`. Callers that translate
/// atomic-store errors into executor-facing outcomes (for example,
/// `EventSink`'s sink-closed translation) rely on this distinction.
pub async fn test_invalid_transition_distinct_from_terminal_already_set(
    atomic: &dyn A2aAtomicStore,
    tasks: &dyn A2aTaskStorage,
) {
    // Create task in Submitted. Then try to jump SUBMITTED → INPUT_REQUIRED,
    // which is NOT a valid state-machine transition (Submitted → Working is
    // the only non-terminal forward path). Expected: InvalidTransition,
    // NOT TerminalStateAlreadySet.
    let task = make_task("cas-dist-1", "ctx-dist");
    tasks
        .create_task("default", "owner-dist", task)
        .await
        .unwrap();

    let result = atomic
        .update_task_status_with_events(
            "default",
            "cas-dist-1",
            "owner-dist",
            TaskStatus::new(TaskState::InputRequired),
            vec![make_status_event_for(
                "cas-dist-1",
                "ctx-dist",
                "TASK_STATE_INPUT_REQUIRED",
            )],
        )
        .await;

    match result {
        Err(crate::storage::A2aStorageError::InvalidTransition { current, requested }) => {
            assert_eq!(current, TaskState::Submitted);
            assert_eq!(requested, TaskState::InputRequired);
        }
        Err(crate::storage::A2aStorageError::TerminalStateAlreadySet { .. }) => {
            panic!("illegal non-terminal transition must surface InvalidTransition, \
                    not TerminalStateAlreadySet");
        }
        Err(other) => panic!("unexpected error: {other:?}"),
        Ok(_) => panic!("illegal transition must fail"),
    }
}
