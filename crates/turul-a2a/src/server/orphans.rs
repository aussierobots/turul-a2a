//! Startup reconciliation of tasks whose executor did not survive a
//! process exit.

use crate::error::A2aError;
use crate::router::{AppState, commit_framework_terminal};
use crate::storage::A2aStorageError;
use turul_a2a_types::{Message, Part, Role, TaskState, TaskStatus};

const RESTART_REASON: &str = "task failed: the server restarted while the task was running; its executor did not survive the restart";

/// Commit FAILED for every executor-held (SUBMITTED or WORKING) task in
/// storage and return how many were failed. Tasks resolved concurrently
/// are skipped; any other storage error aborts the run, and a rerun is
/// idempotent because failed tasks are no longer candidates.
pub(crate) async fn fail_orphaned_tasks(state: &AppState) -> Result<usize, A2aError> {
    let held = state
        .cancellation_supervisor
        .supervisor_list_executor_held_tasks()
        .await
        .map_err(A2aError::from)?;

    let mut failed = 0usize;
    for task in held {
        let message = Message::new(
            uuid::Uuid::now_v7().to_string(),
            Role::Agent,
            vec![Part::text(RESTART_REASON)],
        );
        let status = TaskStatus::new(TaskState::Failed).with_message(message);
        match commit_framework_terminal(
            state,
            &task.tenant,
            &task.owner,
            &task.task_id,
            &task.context_id,
            status,
        )
        .await
        {
            Ok(_) => {
                failed += 1;
                tracing::warn!(
                    target: "turul_a2a::orphaned_task_failed",
                    tenant = %task.tenant,
                    task_id = %task.task_id,
                    "failed orphaned task"
                );
            }
            Err(A2aStorageError::TerminalStateAlreadySet { .. })
            | Err(A2aStorageError::TaskNotFound(_)) => {}
            Err(e) => return Err(A2aError::from(e)),
        }
    }
    tracing::info!(target: "turul_a2a::orphaned_tasks_reconciled", failed = failed);
    Ok(failed)
}
