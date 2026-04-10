use serde::{Deserialize, Serialize};
use turul_a2a_proto as pb;

/// Ergonomic wrapper over proto `Part`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Part {
    pub(crate) inner: pb::Part,
}

impl Part {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            inner: pb::Part {
                content: Some(pb::part::Content::Text(text.into())),
                metadata: None,
                filename: String::new(),
                media_type: "text/plain".to_string(),
            },
        }
    }

    pub fn url(url: impl Into<String>, media_type: impl Into<String>) -> Self {
        Self {
            inner: pb::Part {
                content: Some(pb::part::Content::Url(url.into())),
                metadata: None,
                filename: String::new(),
                media_type: media_type.into(),
            },
        }
    }

    pub fn raw(data: Vec<u8>, media_type: impl Into<String>) -> Self {
        Self {
            inner: pb::Part {
                content: Some(pb::part::Content::Raw(data)),
                metadata: None,
                filename: String::new(),
                media_type: media_type.into(),
            },
        }
    }

    pub fn data(value: serde_json::Value) -> Self {
        Self {
            inner: pb::Part {
                content: Some(pb::part::Content::Data(json_to_proto_value(value))),
                metadata: None,
                filename: String::new(),
                media_type: "application/json".to_string(),
            },
        }
    }

    pub fn with_filename(mut self, filename: impl Into<String>) -> Self {
        self.inner.filename = filename.into();
        self
    }

    pub fn with_media_type(mut self, media_type: impl Into<String>) -> Self {
        self.inner.media_type = media_type.into();
        self
    }

    pub fn as_proto(&self) -> &pb::Part {
        &self.inner
    }

    pub fn into_proto(self) -> pb::Part {
        self.inner
    }
}

impl From<pb::Part> for Part {
    fn from(inner: pb::Part) -> Self {
        Self { inner }
    }
}

impl From<Part> for pb::Part {
    fn from(part: Part) -> Self {
        part.inner
    }
}

impl Serialize for Part {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.inner.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Part {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        pb::Part::deserialize(deserializer).map(Self::from)
    }
}

/// Ergonomic wrapper over proto `Message`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Message {
    pub(crate) inner: pb::Message,
}

/// Role of a message sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Role {
    User,
    Agent,
}

impl From<Role> for pb::Role {
    fn from(role: Role) -> Self {
        match role {
            Role::User => pb::Role::User,
            Role::Agent => pb::Role::Agent,
        }
    }
}

impl TryFrom<pb::Role> for Role {
    type Error = crate::error::A2aTypeError;

    fn try_from(value: pb::Role) -> Result<Self, Self::Error> {
        match value {
            pb::Role::User => Ok(Self::User),
            pb::Role::Agent => Ok(Self::Agent),
            pb::Role::Unspecified => Err(crate::error::A2aTypeError::InvalidState),
        }
    }
}

impl Message {
    pub fn new(message_id: impl Into<String>, role: Role, parts: Vec<Part>) -> Self {
        Self {
            inner: pb::Message {
                message_id: message_id.into(),
                role: pb::Role::from(role).into(),
                parts: parts.into_iter().map(|p| p.inner).collect(),
                context_id: String::new(),
                task_id: String::new(),
                metadata: None,
                extensions: vec![],
                reference_task_ids: vec![],
            },
        }
    }

    pub fn with_context_id(mut self, context_id: impl Into<String>) -> Self {
        self.inner.context_id = context_id.into();
        self
    }

    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.inner.task_id = task_id.into();
        self
    }

    pub fn message_id(&self) -> &str {
        &self.inner.message_id
    }

    pub fn as_proto(&self) -> &pb::Message {
        &self.inner
    }

    pub fn into_proto(self) -> pb::Message {
        self.inner
    }
}

impl From<pb::Message> for Message {
    fn from(inner: pb::Message) -> Self {
        Self { inner }
    }
}

impl From<Message> for pb::Message {
    fn from(msg: Message) -> Self {
        msg.inner
    }
}

impl Serialize for Message {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.inner.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        pb::Message::deserialize(deserializer).map(Self::from)
    }
}

/// Convert serde_json::Value to pbjson_types::Value.
fn json_to_proto_value(value: serde_json::Value) -> pbjson_types::Value {
    match value {
        serde_json::Value::Null => pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::NullValue(0)),
        },
        serde_json::Value::Bool(b) => pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::BoolValue(b)),
        },
        serde_json::Value::Number(n) => pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::NumberValue(n.as_f64().unwrap_or(0.0))),
        },
        serde_json::Value::String(s) => pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::StringValue(s)),
        },
        serde_json::Value::Array(arr) => pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::ListValue(pbjson_types::ListValue {
                values: arr.into_iter().map(json_to_proto_value).collect(),
            })),
        },
        serde_json::Value::Object(map) => pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::StructValue(pbjson_types::Struct {
                fields: map
                    .into_iter()
                    .map(|(k, v)| (k, json_to_proto_value(v)))
                    .collect(),
            })),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_text_constructor() {
        let part = Part::text("hello");
        let proto = part.as_proto();
        assert!(matches!(proto.content, Some(pb::part::Content::Text(ref s)) if s == "hello"));
        assert_eq!(proto.media_type, "text/plain");
    }

    #[test]
    fn part_url_constructor() {
        let part = Part::url("https://example.com/file.pdf", "application/pdf")
            .with_filename("file.pdf");
        let proto = part.as_proto();
        assert!(matches!(proto.content, Some(pb::part::Content::Url(ref u)) if u == "https://example.com/file.pdf"));
        assert_eq!(proto.filename, "file.pdf");
        assert_eq!(proto.media_type, "application/pdf");
    }

    #[test]
    fn part_raw_constructor() {
        let part = Part::raw(vec![0x48, 0x65], "image/png");
        let proto = part.as_proto();
        assert!(matches!(proto.content, Some(pb::part::Content::Raw(ref b)) if b == &[0x48, 0x65]));
    }

    #[test]
    fn part_data_constructor() {
        let part = Part::data(serde_json::json!({"key": "val"}));
        let proto = part.as_proto();
        assert!(matches!(proto.content, Some(pb::part::Content::Data(_))));
    }

    #[test]
    fn part_serde_round_trip() {
        let part = Part::text("round-trip");
        let json = serde_json::to_string(&part).unwrap();
        let back: Part = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back.as_proto().content,
            Some(pb::part::Content::Text(ref s)) if s == "round-trip"
        ));
    }

    #[test]
    fn message_constructor() {
        let msg = Message::new(
            "msg-1",
            Role::User,
            vec![Part::text("hello")],
        );
        assert_eq!(msg.message_id(), "msg-1");
        assert_eq!(msg.as_proto().role, i32::from(pb::Role::User));
        assert_eq!(msg.as_proto().parts.len(), 1);
    }

    #[test]
    fn message_with_context_and_task() {
        let msg = Message::new("msg-2", Role::Agent, vec![])
            .with_context_id("ctx-1")
            .with_task_id("task-1");
        assert_eq!(msg.as_proto().context_id, "ctx-1");
        assert_eq!(msg.as_proto().task_id, "task-1");
    }

    #[test]
    fn message_serde_round_trip() {
        let msg = Message::new("msg-rt", Role::User, vec![Part::text("hi")]);
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.message_id(), "msg-rt");
    }

    #[test]
    fn role_conversions() {
        assert_eq!(pb::Role::from(Role::User), pb::Role::User);
        assert_eq!(pb::Role::from(Role::Agent), pb::Role::Agent);
        assert_eq!(Role::try_from(pb::Role::User).unwrap(), Role::User);
        assert_eq!(Role::try_from(pb::Role::Agent).unwrap(), Role::Agent);
        assert!(Role::try_from(pb::Role::Unspecified).is_err());
    }
}
