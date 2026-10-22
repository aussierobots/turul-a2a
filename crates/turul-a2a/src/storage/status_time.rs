//! Status timestamps and the ListTasks ordering key.
//!
//! A2A v1.0.1 §3.1.4: "Implementations MUST return tasks sorted by their
//! status timestamp time in descending order". Every backend records the
//! time of each status write with [`stamp_status`] and derives its sort
//! key, `statusTimestampAfter` bound and page cursor from the functions
//! here, so all backends order and paginate identically.
//!
//! Artifact and history writes do not change the status, so they do not
//! move a task in the listing.

use chrono::{DateTime, NaiveDateTime, SubsecRound, Utc};
use turul_a2a_proto::Timestamp;
use turul_a2a_types::Task;

use super::error::A2aStorageError;
use crate::streaming::StreamEvent;

/// Fixed-width UTC format: lexical order equals chronological order, so
/// SQL backends can compare and sort the stored text directly.
const SORT_KEY_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.6fZ";

/// Record the commit time on `task`'s status and on every status-update
/// event committed with it, unless they already carry a timestamp. Times
/// are truncated to microseconds, the precision of the sort key.
pub(crate) fn stamp_status(task: &mut Task, events: &mut [StreamEvent]) {
    let now = Utc::now().trunc_subsecs(6);
    let Some(status) = task.as_proto_mut().status.as_mut() else {
        return;
    };
    let ts = *status.timestamp.get_or_insert_with(|| Timestamp::from(now));
    let ts_json = serde_json::to_value(ts).unwrap_or_default();
    for event in events {
        if let StreamEvent::StatusUpdate { status_update } = event
            && let Some(obj) = status_update.status.as_object_mut()
        {
            obj.entry("timestamp").or_insert_with(|| ts_json.clone());
        }
    }
}

/// Format an instant as a sort key.
pub(crate) fn key_of(at: &DateTime<Utc>) -> String {
    at.format(SORT_KEY_FORMAT).to_string()
}

/// The ListTasks sort key of `task`: its status timestamp, or the empty
/// string (sorting last) when it has none.
pub(crate) fn sort_key(task: &Task) -> String {
    task.as_proto()
        .status
        .as_ref()
        .and_then(|s| s.timestamp.as_ref())
        .and_then(|ts| DateTime::<Utc>::try_from(*ts).ok())
        .map(|at| key_of(&at))
        .unwrap_or_default()
}

/// Page cursor pointing just past the task with this sort key and id.
pub(crate) fn encode_cursor(sort_key: &str, task_id: &str) -> String {
    format!("{sort_key}|{task_id}")
}

/// Split a cursor produced by [`encode_cursor`] into `(sort_key, task_id)`.
/// Any other token is an invalid request: the sort key must be empty or in
/// the fixed-width sort-key format, and the task id non-empty.
pub(crate) fn decode_cursor(token: &str) -> Result<(&str, &str), A2aStorageError> {
    token
        .split_once('|')
        .filter(|(key, task_id)| !task_id.is_empty() && (key.is_empty() || is_sort_key(key)))
        .ok_or_else(|| A2aStorageError::InvalidListRequest(format!("invalid pageToken: {token}")))
}

fn is_sort_key(key: &str) -> bool {
    NaiveDateTime::parse_from_str(key, SORT_KEY_FORMAT).is_ok_and(|at| key_of(&at.and_utc()) == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::StatusUpdatePayload;
    use turul_a2a_types::{TaskState, TaskStatus};

    #[test]
    fn stamp_sets_task_and_event_timestamps_once() {
        let mut task = Task::new("t", TaskStatus::new(TaskState::Working));
        let mut events = vec![StreamEvent::StatusUpdate {
            status_update: StatusUpdatePayload {
                task_id: "t".into(),
                context_id: "c".into(),
                status: serde_json::to_value(TaskStatus::new(TaskState::Working)).unwrap(),
            },
        }];
        stamp_status(&mut task, &mut events);

        let ts = task.as_proto().status.as_ref().unwrap().timestamp;
        assert!(ts.is_some(), "status write must record a timestamp");
        let StreamEvent::StatusUpdate { status_update } = &events[0] else {
            unreachable!()
        };
        assert_eq!(
            status_update.status["timestamp"],
            serde_json::to_value(ts.as_ref().unwrap()).unwrap(),
            "the event carries the same timestamp as the task"
        );

        // A status that already carries a time keeps it.
        let key = sort_key(&task);
        stamp_status(&mut task, &mut []);
        assert_eq!(sort_key(&task), key);
    }

    #[test]
    fn sort_key_is_fixed_width_and_chronological() {
        let a = DateTime::from_timestamp(1_700_000_000, 5_000).unwrap();
        let b = DateTime::from_timestamp(1_700_000_000, 120_000_000).unwrap();
        let c = DateTime::from_timestamp(1_700_000_001, 0).unwrap();
        let keys = [key_of(&a), key_of(&b), key_of(&c)];
        assert_eq!(keys[0], "2023-11-14T22:13:20.000005Z");
        assert!(keys.iter().all(|k| k.len() == keys[0].len()));
        assert!(keys[0] < keys[1] && keys[1] < keys[2]);
    }

    #[test]
    fn cursor_roundtrips_and_rejects_garbage() {
        let token = encode_cursor("2023-11-14T22:13:20.000005Z", "task|with|bars");
        assert_eq!(
            decode_cursor(&token).unwrap(),
            ("2023-11-14T22:13:20.000005Z", "task|with|bars")
        );
        // A task with no status timestamp has the empty sort key.
        assert_eq!(decode_cursor("|t").unwrap(), ("", "t"));
        for garbage in [
            "no-separator",
            "x|y",
            "2023-11-14T22:13:20Z|t",
            "2023-11-14T22:13:20.000005Z|",
        ] {
            assert!(
                matches!(
                    decode_cursor(garbage),
                    Err(A2aStorageError::InvalidListRequest(_))
                ),
                "{garbage}"
            );
        }
    }
}
