//! Shared parity test functions for A2A storage backends.
//!
//! Each test takes a `&dyn A2aTaskStorage` so the same assertions apply to InMemory,
//! SQLite, PostgreSQL, and DynamoDB backends. Backend-specific test modules call
//! these functions with their own storage instance.

use turul_a2a_types::{Artifact, Message, Part, Role, Task, TaskState, TaskStatus};

use crate::streaming::StreamEvent;
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
