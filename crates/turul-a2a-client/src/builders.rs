//! Ergonomic builders for client requests.
//!
//! Hides proto nesting so callers don't construct raw `SendMessageRequest`.

use std::collections::HashMap;

use turul_a2a_proto as pb;
use turul_a2a_proto::pbjson_types;
use turul_a2a_types::Part;
use turul_a2a_types::pbjson::json_object_to_struct;

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
    metadata: Option<pbjson_types::Struct>,
}

impl MessageBuilder {
    pub fn new() -> Self {
        Self {
            message_id: uuid::Uuid::now_v7().to_string(),
            role: pb::Role::User.into(),
            parts: vec![],
            context_id: String::new(),
            task_id: String::new(),
            metadata: None,
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

    /// Add a structured JSON data part to the message.
    pub fn data(mut self, value: serde_json::Value) -> Self {
        self.parts.push(Part::data(value).into_proto());
        self
    }

    /// Add a wrapper `Part` to the message. The builder handles proto conversion.
    pub fn part(mut self, part: Part) -> Self {
        self.parts.push(part.into_proto());
        self
    }

    /// Add multiple wrapper `Part`s to the message.
    pub fn parts<I>(mut self, parts: I) -> Self
    where
        I: IntoIterator<Item = Part>,
    {
        self.parts.extend(parts.into_iter().map(|p| p.into_proto()));
        self
    }

    /// Set the message role (default: User).
    /// Accepts the wrapper `Role` type from `turul_a2a_types`.
    pub fn role(mut self, role: turul_a2a_types::Role) -> Self {
        self.role = pb::Role::from(role).into();
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

    /// Attach `Message.metadata` from a flat `HashMap` of correlation
    /// fields. Each value is converted recursively via
    /// [`turul_a2a_types::pbjson::json_to_value`]. Calling this method
    /// replaces any previously set metadata (not merge).
    ///
    /// Typical use: propagate correlation keys (e.g., `trigger_id`,
    /// `cycle_id`) from the caller through the task's history so a
    /// push-notification receiver can extract them from the terminal
    /// Task body.
    ///
    /// ```ignore
    /// use std::collections::HashMap;
    /// use turul_a2a_client::builders::MessageBuilder;
    ///
    /// let mut fields: HashMap<String, serde_json::Value> = HashMap::new();
    /// fields.insert("trigger_id".into(), serde_json::json!("trig-1"));
    /// let req = MessageBuilder::new()
    ///     .text("assess this region")
    ///     .metadata_json(fields)
    ///     .build();
    /// ```
    ///
    /// An empty map clears `Message.metadata`. To set a pre-built
    /// proto `Struct` directly, use [`Self::metadata`].
    pub fn metadata_json(mut self, fields: HashMap<String, serde_json::Value>) -> Self {
        self.metadata = if fields.is_empty() {
            None
        } else {
            Some(json_object_to_struct(fields))
        };
        self
    }

    /// Attach `Message.metadata` from a pre-built proto `Struct`.
    /// Lower-level than [`Self::metadata_json`] — use when the caller
    /// already holds a `pbjson_types::Struct` (e.g., passing through
    /// from another proto layer) and wants to avoid the JSON
    /// conversion round-trip.
    pub fn metadata(mut self, s: pbjson_types::Struct) -> Self {
        self.metadata = Some(s);
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
                metadata: self.metadata,
                extensions: vec![],
                reference_task_ids: vec![],
            }),
            configuration: None,
            metadata: None,
            tenant: String::new(),
        }
    }
}

impl From<MessageBuilder> for pb::SendMessageRequest {
    fn from(builder: MessageBuilder) -> Self {
        builder.build()
    }
}

impl Default for MessageBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbjson_types::value::Kind;

    #[test]
    fn default_build_leaves_metadata_none() {
        let req: pb::SendMessageRequest = MessageBuilder::new().text("hi").build();
        let msg = req.message.expect("message set");
        assert!(msg.metadata.is_none());
    }

    #[test]
    fn metadata_json_populates_struct_with_converted_values() {
        let mut fields: HashMap<String, serde_json::Value> = HashMap::new();
        fields.insert("trigger_id".into(), serde_json::json!("trig-1"));
        fields.insert("attempt".into(), serde_json::json!(3));
        fields.insert("tags".into(), serde_json::json!(["a", "b"]));

        let req: pb::SendMessageRequest = MessageBuilder::new()
            .text("hi")
            .metadata_json(fields)
            .build();
        let s = req
            .message
            .expect("message set")
            .metadata
            .expect("metadata set");
        assert_eq!(s.fields.len(), 3);

        // trigger_id: string
        match &s.fields.get("trigger_id").unwrap().kind {
            Some(Kind::StringValue(v)) => assert_eq!(v, "trig-1"),
            other => panic!("trigger_id should be StringValue, got {other:?}"),
        }
        // attempt: number
        match &s.fields.get("attempt").unwrap().kind {
            Some(Kind::NumberValue(v)) => assert_eq!(*v, 3.0),
            other => panic!("attempt should be NumberValue, got {other:?}"),
        }
        // tags: list of strings
        match &s.fields.get("tags").unwrap().kind {
            Some(Kind::ListValue(list)) => {
                assert_eq!(list.values.len(), 2);
                match &list.values[0].kind {
                    Some(Kind::StringValue(v)) => assert_eq!(v, "a"),
                    other => panic!("tags[0], got {other:?}"),
                }
            }
            other => panic!("tags should be ListValue, got {other:?}"),
        }
    }

    #[test]
    fn metadata_json_empty_map_leaves_metadata_none() {
        // Explicitly passing an empty map is semantically "no metadata"
        // — equivalent to not calling the method. Prevents accidentally
        // sending an empty Struct on the wire when the caller computed
        // an empty correlation map.
        let req: pb::SendMessageRequest = MessageBuilder::new()
            .text("hi")
            .metadata_json(HashMap::new())
            .build();
        assert!(req.message.expect("message set").metadata.is_none());
    }

    #[test]
    fn metadata_json_replaces_previous_call() {
        let mut first: HashMap<String, serde_json::Value> = HashMap::new();
        first.insert("k1".into(), serde_json::json!("v1"));
        let mut second: HashMap<String, serde_json::Value> = HashMap::new();
        second.insert("k2".into(), serde_json::json!("v2"));

        let req: pb::SendMessageRequest = MessageBuilder::new()
            .text("hi")
            .metadata_json(first)
            .metadata_json(second)
            .build();
        let s = req
            .message
            .expect("message set")
            .metadata
            .expect("metadata set");
        assert_eq!(s.fields.len(), 1);
        assert!(
            s.fields.contains_key("k2"),
            "second call must replace first"
        );
        assert!(!s.fields.contains_key("k1"));
    }

    #[test]
    fn metadata_raw_struct_passthrough() {
        let mut fields: HashMap<String, pbjson_types::Value> = HashMap::new();
        fields.insert(
            "preset".into(),
            pbjson_types::Value {
                kind: Some(Kind::StringValue("advanced".into())),
            },
        );
        let raw = pbjson_types::Struct { fields };

        let req: pb::SendMessageRequest = MessageBuilder::new().text("hi").metadata(raw).build();
        let s = req
            .message
            .expect("message set")
            .metadata
            .expect("metadata set");
        match &s.fields.get("preset").unwrap().kind {
            Some(Kind::StringValue(v)) => assert_eq!(v, "advanced"),
            other => panic!("preset, got {other:?}"),
        }
    }
}
