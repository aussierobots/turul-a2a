//! Push-notification delivery dispatcher (ADR-011 §2, §13.1, §13.13).
//!
//! When a durable status event is committed to storage, this
//! dispatcher fans out per-config deliveries:
//!
//! - Terminal `StatusUpdate` events trigger delivery (one POST per
//!   registered push config on the task).
//! - `ArtifactUpdate` events do NOT trigger delivery in 0.1.x
//!   (ADR-011 §13.11 pins this scope).
//! - Non-terminal status transitions do not trigger delivery either
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

use std::sync::Arc;

use turul_a2a_types::Task;
use url::Url;

use crate::push::delivery::{PushDeliveryWorker, PushTarget};
use crate::push::secret::Secret;
use crate::storage::A2aPushNotificationStorage;
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
}

impl PushDispatcher {
    pub fn new(
        worker: Arc<PushDeliveryWorker>,
        push_storage: Arc<dyn A2aPushNotificationStorage>,
    ) -> Self {
        Self { worker, push_storage }
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
    /// This call never awaits; it returns after spawning. Failures
    /// inside deliveries land in the push-delivery store's
    /// failed-delivery ledger; callers do not see per-delivery errors.
    pub fn dispatch(&self, tenant: String, task: Task, events: Vec<(u64, StreamEvent)>) {
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

        let worker = self.worker.clone();
        let push_storage = self.push_storage.clone();
        let task_id = match task.as_proto().id.as_str() {
            s if !s.is_empty() => s.to_string(),
            _ => return,
        };

        tokio::spawn(async move {
            // Load every config for this task by walking the full
            // pagination chain. Hitting only the first page —
            // whatever backend-chosen default size that is — would
            // silently skip deliveries to configs on later pages.
            // The storage trait contract says `next_page_token ==
            // ""` marks the end of iteration.
            let mut configs: Vec<turul_a2a_proto::TaskPushNotificationConfig> = Vec::new();
            let mut page_token: Option<String> = None;
            loop {
                let page = match push_storage
                    .list_configs(&tenant, &task_id, page_token.as_deref(), None)
                    .await
                {
                    Ok(p) => p,
                    Err(_) => return,
                };
                configs.extend(page.configs);
                if page.next_page_token.is_empty() {
                    break;
                }
                page_token = Some(page.next_page_token);
            }
            if configs.is_empty() {
                return;
            }

            // Serialise the task payload once — every config receives
            // the same JSON body (ADR-011 §3).
            let payload = match serde_json::to_vec(&task) {
                Ok(p) => p,
                Err(_) => return,
            };

            for seq in &terminal_seqs {
                for cfg in &configs {
                    let Some(target) = PushTarget::from_config(&tenant, &task_id, *seq, cfg)
                    else {
                        continue;
                    };
                    let worker = worker.clone();
                    let payload = payload.clone();
                    tokio::spawn(async move {
                        let _ = worker.deliver(&target, &payload).await;
                    });
                }
            }
        });
    }
}

/// ADR-011 §2 + §13.11: deliver only on terminal status events.
/// Artifact events are out of scope for 0.1.x.
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
        let t = PushTarget::from_config("t", "task-1", 42, &cfg).expect("valid target");
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
        assert!(PushTarget::from_config("t", "task-1", 1, &cfg).is_none());
    }
}
