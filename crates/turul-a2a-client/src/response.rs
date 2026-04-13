//! Wrapper-first response types for the client API.
//!
//! These types replace raw proto returns so callers never match on
//! generated oneofs or access proto fields directly.

use turul_a2a_proto as pb;
use turul_a2a_types::{Message, Task};

use crate::A2aClientError;

/// Response from `send_message()` — either a Task or a direct Message.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SendResponse {
    Task(Task),
    Message(Message),
}

impl SendResponse {
    /// Get the task if this is a Task response.
    pub fn task(&self) -> Option<&Task> {
        match self {
            Self::Task(t) => Some(t),
            _ => None,
        }
    }

    /// Consume and return the task if this is a Task response.
    pub fn into_task(self) -> Option<Task> {
        match self {
            Self::Task(t) => Some(t),
            _ => None,
        }
    }

    /// Get the message if this is a Message response.
    pub fn message(&self) -> Option<&Message> {
        match self {
            Self::Message(m) => Some(m),
            _ => None,
        }
    }

    /// Consume and return the message if this is a Message response.
    pub fn into_message(self) -> Option<Message> {
        match self {
            Self::Message(m) => Some(m),
            _ => None,
        }
    }

    /// Returns true if this response contains a Task.
    pub fn is_task(&self) -> bool {
        matches!(self, Self::Task(_))
    }
}

impl TryFrom<pb::SendMessageResponse> for SendResponse {
    type Error = A2aClientError;

    fn try_from(resp: pb::SendMessageResponse) -> Result<Self, Self::Error> {
        match resp.payload {
            Some(pb::send_message_response::Payload::Task(proto_task)) => {
                let task = Task::try_from(proto_task)
                    .map_err(|e| A2aClientError::Conversion(e.to_string()))?;
                Ok(Self::Task(task))
            }
            Some(pb::send_message_response::Payload::Message(proto_msg)) => {
                let msg = Message::try_from(proto_msg)
                    .map_err(|e| A2aClientError::Conversion(e.to_string()))?;
                Ok(Self::Message(msg))
            }
            None => Err(A2aClientError::Conversion(
                "SendMessageResponse has no payload".into(),
            )),
        }
    }
}

/// Response from `list_tasks()` with wrapper Task types.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ListResponse {
    pub tasks: Vec<Task>,
    pub next_page_token: String,
    pub total_size: i32,
    pub page_size: i32,
}

impl TryFrom<pb::ListTasksResponse> for ListResponse {
    type Error = A2aClientError;

    fn try_from(resp: pb::ListTasksResponse) -> Result<Self, Self::Error> {
        let tasks: Result<Vec<Task>, _> = resp
            .tasks
            .into_iter()
            .map(Task::try_from)
            .collect();
        let tasks = tasks.map_err(|e| A2aClientError::Conversion(e.to_string()))?;

        Ok(Self {
            tasks,
            next_page_token: resp.next_page_token,
            total_size: resp.total_size,
            page_size: resp.page_size,
        })
    }
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

/// Extract the first data artifact's value from a Task as JSON.
pub fn first_data_artifact(task: &Task) -> Option<serde_json::Value> {
    for artifact in task.artifacts() {
        for part in &artifact.parts {
            if let Some(pb::part::Content::Data(proto_struct)) = &part.content {
                if let Ok(value) = serde_json::to_value(proto_struct) {
                    return Some(value);
                }
            }
        }
    }
    None
}
