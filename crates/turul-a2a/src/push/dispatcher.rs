//! Push-notification delivery dispatcher (ADR-011 §2, §13.1, §13.13).
//!
//! When a durable status event is committed to storage, this
//! dispatcher fans out per-config deliveries:
//!
//! - Terminal `StatusUpdate` events trigger delivery (one POST per
//!   registered push config on the task).
//! - `ArtifactUpdate` events do not trigger delivery (ADR-011
//!   §13.11).
//! - Non-terminal status transitions do not trigger delivery
//!   (ADR-011 §2).
//!
//! The dispatcher takes ownership of the work of translating a
//! [`turul_a2a_proto::TaskPushNotificationConfig`] into a
//! [`PushTarget`] — wrapping credentials + token in [`Secret`] and
//! preserving the authentication scheme verbatim. It then spawns one
//! tokio task per (event, config) pair, each invoking the
//! [`PushDeliveryWorker`] to claim and POST.
//!
//! The framework-committed terminal paths (CancelTask force-commit,
//! blocking-send hard-timeout FAILED) both route through this
//! dispatcher so ADR-011 §13.13 — "framework-committed CANCELED
//! triggers delivery" — is satisfied by construction.
//!
//! # Transient-store-error handling
//!
//! If the worker reports [`DeliveryReport::TransientStoreError`]
//! after its own bounded retries, the POST succeeded but the
//! terminal row is still non-terminal. The dispatcher logs a warning
//! and returns; the row stays expired and non-terminal on storage.
//! The server's reclaim-and-redispatch sweep loop (see
//! [`crate::server::A2aServer::run`]) enumerates such rows via
//! [`crate::push::A2aPushDeliveryStore::list_reclaimable_claims`]
//! and drives each through [`PushDispatcher::redispatch_one`]. Each
//! redispatch re-claims atomically (the stored row is expired and
//! non-terminal, so `claim_delivery` increments the generation) and
//! invokes the worker, which at-least-once semantics allow to
//! re-POST. Rows whose task or push config has since been deleted
//! are moved to `Abandoned` by the sweeper so they stop being
//! reclaimable.

use std::sync::Arc;

use turul_a2a_types::Task;
use url::Url;

use crate::push::claim::ReclaimableClaim;
use crate::push::delivery::{DeliveryReport, PushDeliveryWorker, PushTarget};
use crate::push::secret::Secret;
use crate::storage::{A2aPushNotificationStorage, A2aTaskStorage};
use crate::streaming::StreamEvent;

/// Dispatcher of push notifications from durable commit events.
///
/// Constructed once per server instance (see
/// [`crate::server::A2aServerBuilder::build`]) and held as
/// `Option<Arc<PushDispatcher>>` on [`crate::router::AppState`]. A
/// `None` value means "this deployment doesn't run push delivery";
/// the push-config CRUD endpoints still function (configs are
/// stored), deliveries just never fire.
#[derive(Clone)]
pub struct PushDispatcher {
    worker: Arc<PushDeliveryWorker>,
    push_storage: Arc<dyn A2aPushNotificationStorage>,
    task_storage: Arc<dyn A2aTaskStorage>,
}

impl PushDispatcher {
    pub fn new(
        worker: Arc<PushDeliveryWorker>,
        push_storage: Arc<dyn A2aPushNotificationStorage>,
        task_storage: Arc<dyn A2aTaskStorage>,
    ) -> Self {
        Self {
            worker,
            push_storage,
            task_storage,
        }
    }

    /// Dispatch deliveries for a batch of events just committed for a task.
    ///
    /// The caller provides the `(sequence, event)` pairs returned by the
    /// atomic store, plus the final task snapshot that will serve as the
    /// POST payload. The dispatcher filters for dispatch-eligible events
    /// (terminal `StatusUpdate`) and spawns one delivery tokio task per
    /// (event, config) pair so a slow receiver for one config cannot
    /// hold up deliveries to other configs on the same task.
    ///
    /// `owner` is recorded on the claim row so the reclaim sweeper can
    /// re-load the task via the owner-scoped `A2aTaskStorage::get_task`
    /// without opening a new unscoped read path.
    ///
    /// This call never awaits; it returns after spawning. Failures
    /// inside deliveries land in the push-delivery store's
    /// failed-delivery ledger; callers do not see per-delivery errors.
    pub fn dispatch(
        &self,
        tenant: String,
        owner: String,
        task: Task,
        events: Vec<(u64, StreamEvent)>,
    ) {
        // Early-filter terminal events so we skip the list_configs
        // round-trip on tasks that aren't actually terminating.
        let terminal_seqs: Vec<u64> = events
            .into_iter()
            .filter(|(_, e)| dispatch_eligible(e))
            .map(|(seq, _)| seq)
            .collect();
        if terminal_seqs.is_empty() {
            return;
        }

        let task_id = match task.as_proto().id.as_str() {
            s if !s.is_empty() => s.to_string(),
            _ => return,
        };

        let self_clone = self.clone();
        tokio::spawn(async move {
            self_clone
                .run_fanout(tenant, owner, task_id, task, terminal_seqs)
                .await;
        });
    }

    /// Run the fan-out logic for a given task + terminal sequences.
    /// Shared between the fresh-dispatch path and the reclaim
    /// redispatch path. Awaits every per-config delivery so the
    /// pending-dispatch marker is only deleted once every delivery
    /// task has had a chance to create its claim row — after that
    /// point, recovery is the claim-row reclaim path's
    /// responsibility.
    async fn run_fanout(
        &self,
        tenant: String,
        owner: String,
        task_id: String,
        task: Task,
        terminal_seqs: Vec<u64>,
    ) {
        // Record a pending-dispatch marker for each terminal
        // sequence BEFORE calling list_configs. This is the durable
        // evidence the event needs fan-out — if list_configs fails
        // persistently, the reclaim sweep enumerates these markers
        // via `list_stale_pending_dispatches` and re-invokes this
        // fan-out. Failure to record the marker is non-fatal (it
        // reduces recovery to the existing bounded-retry behaviour)
        // but emits a warning so operators can alert on a claim
        // store outage that's coincident with a dispatch wave.
        for seq in &terminal_seqs {
            if let Err(e) = self
                .push_delivery_store_handle()
                .record_pending_dispatch(&tenant, &owner, &task_id, *seq)
                .await
            {
                tracing::warn!(
                    target: "turul_a2a::push_pending_dispatch_record_failed",
                    tenant = %tenant,
                    task_id = %task_id,
                    event_sequence = seq,
                    error = %e,
                    "pending-dispatch marker write failed; dispatch continues \
                     but reclaim cannot recover if list_configs also fails"
                );
            }
        }

        // Load every config for this task by walking the full
        // pagination chain. Each page read gets a bounded retry
        // with exponential backoff: a transient config-store blip
        // would otherwise drop the notification even though the
        // pending-dispatch marker is now in place. `next_page_token
        // == ""` marks the end of iteration per the storage trait
        // contract.
        const LIST_MAX_ATTEMPTS: u32 = 3;
        let backoffs = [
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(150),
            std::time::Duration::from_millis(500),
        ];

        let mut configs: Vec<turul_a2a_proto::TaskPushNotificationConfig> = Vec::new();
        let mut page_token: Option<String> = None;
        let page_fetch_outcome: Result<(), crate::storage::A2aStorageError> = loop {
            let mut last_err: Option<crate::storage::A2aStorageError> = None;
            let page = 'retry: loop {
                for attempt in 0..LIST_MAX_ATTEMPTS {
                    if attempt > 0 {
                        tokio::time::sleep(
                            backoffs[(attempt as usize).min(backoffs.len() - 1)],
                        )
                        .await;
                    }
                    match self
                        .push_storage
                        .list_configs(&tenant, &task_id, page_token.as_deref(), None)
                        .await
                    {
                        Ok(p) => break 'retry Some(p),
                        Err(e) => last_err = Some(e),
                    }
                }
                break 'retry None;
            };
            match page {
                Some(p) => {
                    configs.extend(p.configs);
                    if p.next_page_token.is_empty() {
                        break Ok(());
                    }
                    page_token = Some(p.next_page_token);
                }
                None => {
                    break Err(last_err.unwrap_or_else(|| {
                        crate::storage::A2aStorageError::DatabaseError(
                            "list_configs exhausted retry budget without recording an error"
                                .into(),
                        )
                    }));
                }
            }
        };

        if let Err(e) = page_fetch_outcome {
            // Markers stay in place so the reclaim sweep will retry
            // this dispatch on its next tick once the config store
            // recovers.
            tracing::error!(
                target: "turul_a2a::push_dispatch_config_list_failed",
                tenant = %tenant,
                task_id = %task_id,
                error = %e,
                "push dispatch aborted: list_configs failed after bounded retry; \
                 pending-dispatch markers retained for reclaim sweep"
            );
            return;
        }

        // No configs registered for this task — delete markers
        // (there's nothing to deliver) and return.
        if configs.is_empty() {
            for seq in &terminal_seqs {
                let _ = self
                    .push_delivery_store_handle()
                    .delete_pending_dispatch(&tenant, &task_id, *seq)
                    .await;
            }
            return;
        }

        // Serialise the task payload once — every config receives
        // the same JSON body (ADR-011 §3).
        let payload = match serde_json::to_vec(&task) {
            Ok(p) => p,
            Err(_) => return,
        };

        // Fan out per (event, config), awaiting each task so the
        // pending-dispatch marker isn't deleted until every
        // per-config deliver has returned — at which point each
        // claim row exists (or was durably attempted and the
        // reclaim-claims path will recover it).
        let mut join_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        for seq in &terminal_seqs {
            for cfg in &configs {
                let Some(target) =
                    PushTarget::from_config(&tenant, &owner, &task_id, *seq, cfg)
                else {
                    continue;
                };
                let worker = self.worker.clone();
                let payload = payload.clone();
                let log_tenant = target.tenant.clone();
                let log_task = target.task_id.clone();
                let log_cfg = target.config_id.clone();
                let log_seq = target.event_sequence;
                join_handles.push(tokio::spawn(async move {
                    let report = worker.deliver(&target, &payload).await;
                    if matches!(report, DeliveryReport::TransientStoreError) {
                        tracing::warn!(
                            target: "turul_a2a::push_delivery_stuck",
                            tenant = %log_tenant,
                            task_id = %log_task,
                            config_id = %log_cfg,
                            event_sequence = log_seq,
                            "push delivery POST succeeded but terminal claim write \
                             did not persist; row remains non-terminal"
                        );
                    }
                }));
            }
        }
        for h in join_handles {
            let _ = h.await;
        }

        // All per-config deliveries have returned. Each either has
        // a terminal claim row (nothing more to do) or a
        // non-terminal claim row that `list_reclaimable_claims`
        // will surface — both cases are beyond what the
        // pending-dispatch marker protects. Delete the markers.
        for seq in &terminal_seqs {
            let _ = self
                .push_delivery_store_handle()
                .delete_pending_dispatch(&tenant, &task_id, *seq)
                .await;
        }
    }

    /// Handle back to the push-delivery store that the worker wraps.
    /// Exposed here so the fan-out path can write/delete the
    /// pending-dispatch marker without re-plumbing the trait object
    /// through every call site.
    fn push_delivery_store_handle(&self) -> &Arc<dyn crate::push::A2aPushDeliveryStore> {
        &self.worker.push_delivery_store
    }

    /// Redispatch a stale pending-dispatch marker.
    ///
    /// Called by the server's reclaim loop for every
    /// [`crate::push::PendingDispatch`] returned by
    /// [`crate::push::A2aPushDeliveryStore::list_stale_pending_dispatches`].
    /// Loads the task (owner-scoped) and re-runs the fan-out. Missing
    /// task → delete the marker (task was deleted before dispatch
    /// could complete). Storage error on the load → leave the marker
    /// for the next sweep tick.
    pub async fn redispatch_pending(&self, pending: crate::push::claim::PendingDispatch) {
        let crate::push::claim::PendingDispatch {
            tenant,
            owner,
            task_id,
            event_sequence,
            ..
        } = pending;

        let task_result = self
            .task_storage
            .get_task(&tenant, &task_id, &owner, None)
            .await;

        let task = match task_result {
            Ok(Some(task)) => task,
            Ok(None) => {
                // Task deleted between original dispatch and the
                // reclaim tick. Nothing to deliver; drop the marker
                // so it stops being reclaimable.
                let _ = self
                    .push_delivery_store_handle()
                    .delete_pending_dispatch(&tenant, &task_id, event_sequence)
                    .await;
                return;
            }
            Err(e) => {
                tracing::warn!(
                    target: "turul_a2a::push_redispatch_pending_read_error",
                    tenant = %tenant,
                    task_id = %task_id,
                    event_sequence = event_sequence,
                    error = %e,
                    "pending redispatch skipped: task store read failed; \
                     marker retained for next sweep tick"
                );
                return;
            }
        };

        // Re-run the fan-out for this event. The inner logic
        // refreshes the marker's recorded_at before list_configs
        // and deletes it after completion.
        self.run_fanout(tenant, owner, task_id, task, vec![event_sequence])
            .await;
    }

    /// Redispatch one previously-stuck claim row.
    ///
    /// Called by the server's reclaim-and-redispatch loop on rows
    /// returned by [`crate::push::A2aPushDeliveryStore::list_reclaimable_claims`].
    /// The sweeper does not hold any claim itself — it only enumerates
    /// rows. This method assembles the target the same way the
    /// dispatch-on-event path does, then invokes
    /// [`PushDeliveryWorker::deliver`], which re-claims the row
    /// atomically (the stored row is expired and non-terminal, so
    /// `claim_delivery` increments the generation and resets status
    /// to `Pending` before proceeding).
    ///
    /// Missing task or missing config causes the redispatch to still
    /// claim the row and record a terminal `Abandoned` outcome so
    /// the row stops being reclaimable. The worker's existing
    /// `persist_terminal` path performs that commit atomically; if
    /// the write fails with `TransientStoreError` we log and leave
    /// the row for the next sweep tick.
    pub async fn redispatch_one(&self, claim: ReclaimableClaim) {
        use crate::push::claim::AbandonedReason;

        let ReclaimableClaim {
            tenant,
            owner,
            task_id,
            event_sequence,
            config_id,
        } = claim;

        // Three-way branching on each storage read: only `Ok(None)`
        // is "deleted". `Err(_)` is a transient outage and MUST
        // leave the row non-terminal so the next sweep tick picks
        // it up — otherwise a short blip on the push-config or task
        // store would terminalise perfectly-valid reclaimable rows.
        let config_result = self
            .push_storage
            .get_config(&tenant, &task_id, &config_id)
            .await;
        let task_result = self
            .task_storage
            .get_task(&tenant, &task_id, &owner, None)
            .await;

        if let Err(e) = &config_result {
            tracing::warn!(
                target: "turul_a2a::push_redispatch_read_error",
                tenant = %tenant,
                task_id = %task_id,
                config_id = %config_id,
                event_sequence,
                error = %e,
                "reclaim redispatch skipped: push-config store read failed; will retry next sweep"
            );
            return;
        }
        if let Err(e) = &task_result {
            tracing::warn!(
                target: "turul_a2a::push_redispatch_read_error",
                tenant = %tenant,
                task_id = %task_id,
                config_id = %config_id,
                event_sequence,
                error = %e,
                "reclaim redispatch skipped: task store read failed; will retry next sweep"
            );
            return;
        }

        let config = config_result.expect("Err handled above");
        let task = task_result.expect("Err handled above");

        match (task, config) {
            (Some(task), Some(cfg)) => {
                let Some(target) =
                    PushTarget::from_config(&tenant, &owner, &task_id, event_sequence, &cfg)
                else {
                    // Malformed URL; create-time validation rejects
                    // these so this is effectively unreachable. We
                    // can't POST and can't classify under any
                    // existing AbandonedReason, so we leave the row
                    // for the next sweep — a future URL-validation
                    // follow-up can promote this to a terminal
                    // outcome.
                    return;
                };
                let payload = match serde_json::to_vec(&task) {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let log_tenant = target.tenant.clone();
                let log_task = target.task_id.clone();
                let log_cfg = target.config_id.clone();
                let log_seq = target.event_sequence;
                let report = self.worker.deliver(&target, &payload).await;
                if matches!(report, DeliveryReport::TransientStoreError) {
                    tracing::warn!(
                        target: "turul_a2a::push_redispatch_stuck",
                        tenant = %log_tenant,
                        task_id = %log_task,
                        config_id = %log_cfg,
                        event_sequence = log_seq,
                        "reclaim redispatch POST succeeded but terminal \
                         claim write still did not persist"
                    );
                }
            }
            (task, cfg) => {
                // `Ok(None)` on either side = genuine deletion. Mark
                // the row Abandoned so it stops being reclaimable;
                // the sweep loop would otherwise walk it forever.
                let abandon_reason = if task.is_none() {
                    AbandonedReason::TaskDeleted
                } else {
                    AbandonedReason::ConfigDeleted
                };
                let url = cfg
                    .as_ref()
                    .and_then(|c| Url::parse(&c.url).ok())
                    // Placeholder URL when the config is gone — the
                    // worker's abandon path never POSTs, so the URL
                    // is never dialled.
                    .unwrap_or_else(|| {
                        Url::parse("https://invalid.abandoned.push/").expect("static")
                    });
                let target = PushTarget {
                    tenant: tenant.clone(),
                    owner: owner.clone(),
                    task_id: task_id.clone(),
                    event_sequence,
                    config_id: config_id.clone(),
                    url,
                    auth_scheme: String::new(),
                    auth_credentials: Secret::new(String::new()),
                    token: None,
                };
                let _ = self
                    .worker
                    .abandon_reclaimed(&target, abandon_reason)
                    .await;
            }
        }
    }
}

/// Deliver only on terminal status events (ADR-011 §2 + §13.11).
/// Artifact events are out of scope for push delivery.
fn dispatch_eligible(ev: &StreamEvent) -> bool {
    matches!(ev, StreamEvent::StatusUpdate { .. }) && ev.is_terminal()
}

impl PushTarget {
    /// Build a [`PushTarget`] from a stored push config + the event
    /// sequence that triggered delivery. Returns `None` if the URL is
    /// malformed — a malformed URL is an operator-visible config error
    /// that the SSRF guard would reject anyway; we skip it silently
    /// here so the whole dispatch run isn't poisoned by one bad config.
    pub fn from_config(
        tenant: &str,
        owner: &str,
        task_id: &str,
        event_sequence: u64,
        cfg: &turul_a2a_proto::TaskPushNotificationConfig,
    ) -> Option<Self> {
        let url = Url::parse(&cfg.url).ok()?;
        let (auth_scheme, auth_credentials) = match cfg.authentication.as_ref() {
            Some(a) => (a.scheme.clone(), Secret::new(a.credentials.clone())),
            None => (String::new(), Secret::new(String::new())),
        };
        let token = if cfg.token.is_empty() {
            None
        } else {
            Some(Secret::new(cfg.token.clone()))
        };
        Some(PushTarget {
            tenant: tenant.to_string(),
            owner: owner.to_string(),
            task_id: task_id.to_string(),
            event_sequence,
            config_id: cfg.id.clone(),
            url,
            auth_scheme,
            auth_credentials,
            token,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_events_are_not_dispatched() {
        let ev = StreamEvent::ArtifactUpdate {
            artifact_update: crate::streaming::ArtifactUpdatePayload {
                task_id: "t".into(),
                context_id: "c".into(),
                artifact: serde_json::json!({}),
                append: false,
                last_chunk: true,
            },
        };
        assert!(!dispatch_eligible(&ev));
    }

    #[test]
    fn non_terminal_status_is_not_dispatched() {
        let status = turul_a2a_types::TaskStatus::new(turul_a2a_types::TaskState::Working);
        let ev = StreamEvent::StatusUpdate {
            status_update: crate::streaming::StatusUpdatePayload {
                task_id: "t".into(),
                context_id: "c".into(),
                status: serde_json::to_value(&status).unwrap(),
            },
        };
        assert!(!dispatch_eligible(&ev));
    }

    #[test]
    fn terminal_status_is_dispatched() {
        for state in [
            turul_a2a_types::TaskState::Completed,
            turul_a2a_types::TaskState::Failed,
            turul_a2a_types::TaskState::Canceled,
            turul_a2a_types::TaskState::Rejected,
        ] {
            let status = turul_a2a_types::TaskStatus::new(state);
            let ev = StreamEvent::StatusUpdate {
                status_update: crate::streaming::StatusUpdatePayload {
                    task_id: "t".into(),
                    context_id: "c".into(),
                    status: serde_json::to_value(&status).unwrap(),
                },
            };
            assert!(dispatch_eligible(&ev), "state {state:?} must dispatch");
        }
    }

    #[test]
    fn push_target_from_config_wraps_secrets() {
        let cfg = turul_a2a_proto::TaskPushNotificationConfig {
            tenant: "t".into(),
            id: "cfg-A".into(),
            task_id: "task-1".into(),
            url: "https://example.com/hook".into(),
            token: "TOKEN-X".into(),
            authentication: Some(turul_a2a_proto::AuthenticationInfo {
                scheme: "Bearer".into(),
                credentials: "CRED-Y".into(),
            }),
        };
        let t = PushTarget::from_config("t", "anonymous", "task-1", 42, &cfg).expect("valid target");
        assert_eq!(t.config_id, "cfg-A");
        assert_eq!(t.event_sequence, 42);
        assert_eq!(t.auth_scheme, "Bearer");
        // Debug of the Secret must not leak the raw bytes.
        assert!(!format!("{:?}", t.auth_credentials).contains("CRED-Y"));
        assert!(!format!("{:?}", t.token).contains("TOKEN-X"));
    }

    #[test]
    fn push_target_from_config_rejects_malformed_url() {
        let cfg = turul_a2a_proto::TaskPushNotificationConfig {
            tenant: "t".into(),
            id: "cfg-A".into(),
            task_id: "task-1".into(),
            url: "not a url".into(),
            token: String::new(),
            authentication: None,
        };
        assert!(PushTarget::from_config("t", "anonymous", "task-1", 1, &cfg).is_none());
    }
}
