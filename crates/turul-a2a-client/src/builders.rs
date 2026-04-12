//! Ergonomic builders for client requests.
//!
//! Hides proto nesting so callers don't construct raw `SendMessageRequest`.

use turul_a2a_proto as pb;

/// Builder for `SendMessageRequest`.
///
/// ```ignore
/// let request = MessageBuilder::new()
///     .text("hello agent")
///     .build();
/// client.send_message(request).await?;
/// ```
pub struct MessageBuilder {
    message_id: String,
    role: i32,
    parts: Vec<pb::Part>,
    context_id: String,
    task_id: String,
}

impl MessageBuilder {
    pub fn new() -> Self {
        Self {
            message_id: uuid::Uuid::now_v7().to_string(),
            role: pb::Role::User.into(),
            parts: vec![],
            context_id: String::new(),
            task_id: String::new(),
        }
    }

    /// Add a text part to the message.
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.parts.push(pb::Part {
            content: Some(pb::part::Content::Text(text.into())),
            metadata: None,
            filename: String::new(),
            media_type: String::new(),
        });
        self
    }

    /// Set the message role (default: User).
    pub fn role(mut self, role: pb::Role) -> Self {
        self.role = role.into();
        self
    }

    /// Set a specific message ID (default: auto-generated UUID v7).
    pub fn message_id(mut self, id: impl Into<String>) -> Self {
        self.message_id = id.into();
        self
    }

    /// Set the context ID for conversation continuity.
    pub fn context_id(mut self, id: impl Into<String>) -> Self {
        self.context_id = id.into();
        self
    }

    /// Set the task ID to continue an existing task.
    pub fn task_id(mut self, id: impl Into<String>) -> Self {
        self.task_id = id.into();
        self
    }

    /// Build the `SendMessageRequest`.
    pub fn build(self) -> pb::SendMessageRequest {
        pb::SendMessageRequest {
            message: Some(pb::Message {
                message_id: self.message_id,
                role: self.role,
                parts: self.parts,
                context_id: self.context_id,
                task_id: self.task_id,
                metadata: None,
                extensions: vec![],
                reference_task_ids: vec![],
            }),
            configuration: None,
            metadata: None,
            tenant: String::new(),
        }
    }
}

impl Default for MessageBuilder {
    fn default() -> Self {
        Self::new()
    }
}
