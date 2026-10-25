//! Background loops that keep task state honest about its executor.
//!
//! - **Lease renewal** stamps the expiry of every task in this process's
//!   in-flight registry, so a peer can tell a live executor from a dead
//!   instance.
//! - **Lease reaper** fails tasks whose expiry has passed: the instance
//!   that ran their executor stopped renewing it.
//! - **Stall watchdog** fails and aborts executors in this process that
//!   stopped making progress.
//!
//! Each loop holds the in-flight registry weakly and exits when the
//! server state that owns it is dropped. The loops commit framework
//! terminals through a copy of the server state whose registry is a
//! private empty one: `commit_framework_terminal` reads only the stores,
//! the broker and the push dispatcher, and keeping the real registry out
//! of the copy is what lets the loops observe the owner's drop.

use std::sync::{Arc, Weak};
use std::time::Duration;

use turul_a2a_types::{Message, Part, Role, TaskState, TaskStatus};

use crate::router::{AppState, commit_framework_terminal};
use crate::server::in_flight::InFlightRegistry;
use crate::storage::{A2aCancellationSupervisor, A2aStorageError};

const LEASE_LOST_REASON: &str =
    "task failed: the server running its executor stopped renewing the executor's lease";

/// Start the loops the runtime configuration enables. Must be called
/// from within a tokio runtime.
pub(crate) fn spawn_loops(state: &AppState) {
    let config = &state.runtime_config;
    if config.executor_lease_ttl.is_none() && config.executor_stall_timeout.is_none() {
        return;
    }
    let registry = Arc::downgrade(&state.in_flight);
    let supervisor = Arc::clone(&state.cancellation_supervisor);
    let committer = AppState {
        in_flight: Arc::new(InFlightRegistry::new()),
        ..state.clone()
    };

    if let Some(ttl) = config.executor_lease_ttl {
        tokio::spawn(renew_loop(registry.clone(), supervisor.clone(), ttl));
        tokio::spawn(reap_loop(
            registry.clone(),
            supervisor,
            committer.clone(),
            ttl,
        ));
    }
    if let Some(timeout) = config.executor_stall_timeout {
        tokio::spawn(stall_loop(
            registry,
            committer,
            timeout,
            config.timeout_abort_grace,
        ));
    }
}

fn failed_status(reason: &str) -> TaskStatus {
    let message = Message::new(
        uuid::Uuid::now_v7().to_string(),
        Role::Agent,
        vec![Part::text(reason)],
    );
    TaskStatus::new(TaskState::Failed).with_message(message)
}

fn now_micros() -> i64 {
    chrono::Utc::now().timestamp_micros()
}

async fn renew_loop(
    registry: Weak<InFlightRegistry>,
    supervisor: Arc<dyn A2aCancellationSupervisor>,
    ttl: Duration,
) {
    // Fixed-rate ticks: the period must not stretch by the duration of a
    // renewal pass, or a slow store lets live leases lapse.
    let period = ttl / 3;
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let Some(groups) = registry.upgrade().map(|r| r.snapshot_by_tenant()) else {
            return;
        };
        let expires_at =
            now_micros().saturating_add(i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX));
        let supervisor = supervisor.as_ref();
        futures::future::join_all(groups.iter().map(|(tenant, task_ids)| async move {
            if let Err(e) = supervisor
                .supervisor_renew_executor_leases(tenant, task_ids, Some(expires_at))
                .await
            {
                tracing::warn!(
                    target: "turul_a2a::executor_lease_renew_error",
                    %tenant,
                    error = %e,
                    "executor lease renewal failed; will retry next tick"
                );
            }
        }))
        .await;
    }
}

async fn reap_loop(
    registry: Weak<InFlightRegistry>,
    supervisor: Arc<dyn A2aCancellationSupervisor>,
    committer: AppState,
    ttl: Duration,
) {
    loop {
        if registry.strong_count() == 0 {
            return;
        }
        reap_once(supervisor.as_ref(), &committer).await;
        tokio::time::sleep(ttl).await;
    }
}

async fn reap_once(supervisor: &dyn A2aCancellationSupervisor, committer: &AppState) {
    let expired = match supervisor
        .supervisor_list_expired_executor_leases(now_micros())
        .await
    {
        Ok(expired) => expired,
        Err(e) => {
            tracing::warn!(
                target: "turul_a2a::executor_lease_reap_error",
                error = %e,
                "listing expired executor leases failed; will retry next tick"
            );
            return;
        }
    };
    for task in expired {
        match commit_framework_terminal(
            committer,
            &task.tenant,
            &task.owner,
            &task.task_id,
            &task.context_id,
            failed_status(LEASE_LOST_REASON),
        )
        .await
        {
            Ok(_) => {
                tracing::warn!(
                    target: "turul_a2a::executor_lease_expired",
                    tenant = %task.tenant,
                    task_id = %task.task_id,
                    "failed task whose executor lease expired"
                );
                clear_lease(supervisor, &task.tenant, &task.task_id).await;
            }
            Err(A2aStorageError::TerminalStateAlreadySet { .. })
            | Err(A2aStorageError::TaskNotFound(_)) => {
                clear_lease(supervisor, &task.tenant, &task.task_id).await;
            }
            Err(e) => tracing::warn!(
                target: "turul_a2a::executor_lease_reap_error",
                tenant = %task.tenant,
                task_id = %task.task_id,
                error = %e,
                "failing task with expired executor lease failed; will retry next tick"
            ),
        }
    }
}

async fn clear_lease(supervisor: &dyn A2aCancellationSupervisor, tenant: &str, task_id: &str) {
    if let Err(e) = supervisor
        .supervisor_renew_executor_leases(tenant, &[task_id.to_string()], None)
        .await
    {
        tracing::warn!(
            target: "turul_a2a::executor_lease_renew_error",
            %tenant,
            %task_id,
            error = %e,
            "clearing executor lease failed"
        );
    }
}

async fn stall_loop(
    registry: Weak<InFlightRegistry>,
    committer: AppState,
    timeout: Duration,
    abort_grace: Duration,
) {
    loop {
        tokio::time::sleep(timeout / 4).await;
        let Some(handles) = registry.upgrade().map(|r| r.snapshot_handles()) else {
            return;
        };
        for ((tenant, task_id), handle) in handles {
            if handle.terminal_committed()
                || handle.client_held()
                || handle.cancellation.is_cancelled()
                || handle.since_progress() <= timeout
            {
                continue;
            }
            let Some((owner, context_id)) = handle.identity() else {
                continue;
            };
            let reason = format!("task failed: its executor made no progress for {timeout:?}");
            match commit_framework_terminal(
                &committer,
                &tenant,
                owner,
                &task_id,
                context_id,
                failed_status(&reason),
            )
            .await
            {
                Ok(_) => tracing::warn!(
                    target: "turul_a2a::executor_stalled",
                    %tenant,
                    %task_id,
                    "failed task whose executor made no progress"
                ),
                Err(
                    A2aStorageError::TerminalStateAlreadySet { .. }
                    | A2aStorageError::TaskNotFound(_),
                ) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "turul_a2a::executor_stalled",
                        %tenant,
                        %task_id,
                        error = %e,
                        "failing stalled task failed; will retry next tick"
                    );
                    continue;
                }
            }
            handle.cancel_then_abort_after(abort_grace);
        }
    }
}
