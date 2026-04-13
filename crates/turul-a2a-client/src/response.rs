//! Response helpers for extracting wrapper types from proto responses.
//!
//! Eliminates raw proto oneof matching in client code.

use turul_a2a_proto as pb;
use turul_a2a_types::Task;

/// Extract the Task from a `SendMessageResponse`, converting to a wrapper type.
///
/// Returns `None` if the response contains a Message instead of a Task,
/// or if the proto Task fails validation.
pub fn response_task(resp: &pb::SendMessageResponse) -> Option<Task> {
    match resp.payload.as_ref()? {
        pb::send_message_response::Payload::Task(proto_task) => {
            Task::try_from(proto_task.clone()).ok()
        }
        pb::send_message_response::Payload::Message(_) => None,
    }
}

/// Extract the task ID from a `SendMessageResponse`.
pub fn response_task_id(resp: &pb::SendMessageResponse) -> Option<&str> {
    match resp.payload.as_ref()? {
        pb::send_message_response::Payload::Task(t) => Some(&t.id),
        pb::send_message_response::Payload::Message(_) => None,
    }
}

/// Check if the response contains a Task (vs a Message).
pub fn response_is_task(resp: &pb::SendMessageResponse) -> bool {
    matches!(
        resp.payload.as_ref(),
        Some(pb::send_message_response::Payload::Task(_))
    )
}

/// Extract the first data artifact's value from a Task as JSON.
///
/// Walks the task's artifacts, finds the first part with structured data content,
/// and returns it as a `serde_json::Value`.
pub fn first_data_artifact(task: &Task) -> Option<serde_json::Value> {
    for artifact in task.artifacts() {
        for part in &artifact.parts {
            if let Some(pb::part::Content::Data(proto_struct)) = &part.content {
                // Convert pbjson_types::Struct back to serde_json::Value
                if let Ok(value) = serde_json::to_value(proto_struct) {
                    return Some(value);
                }
            }
        }
    }
    None
}

/// Extract all text from a Task's artifacts, concatenated.
pub fn artifact_text(task: &Task) -> String {
    task.artifacts()
        .iter()
        .flat_map(|a| a.parts.iter())
        .filter_map(|p| match &p.content {
            Some(pb::part::Content::Text(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}
