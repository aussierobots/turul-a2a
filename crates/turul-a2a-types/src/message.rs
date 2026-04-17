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

    /// Returns the text content if this is a text part.
    pub fn as_text(&self) -> Option<&str> {
        match &self.inner.content {
            Some(pb::part::Content::Text(t)) => Some(t.as_str()),
            _ => None,
        }
    }

    /// Returns the URL if this is a URL part.
    pub fn as_url(&self) -> Option<&str> {
        match &self.inner.content {
            Some(pb::part::Content::Url(u)) => Some(u.as_str()),
            _ => None,
        }
    }

    /// Returns the raw bytes if this is a raw/binary part.
    pub fn as_raw(&self) -> Option<&[u8]> {
        match &self.inner.content {
            Some(pb::part::Content::Raw(r)) => Some(r.as_slice()),
            _ => None,
        }
    }

    /// Raw JSON view of a Data part. No number normalization.
    /// Returns `None` if this is not a Data part.
    pub fn as_data(&self) -> Option<serde_json::Value> {
        match &self.inner.content {
            Some(pb::part::Content::Data(proto_struct)) => {
                serde_json::to_value(proto_struct).ok()
            }
            _ => None,
        }
    }

    /// Deserialize a Data part into `T`, normalizing proto f64 integers first.
    ///
    /// Protobuf `Value` uses f64 for all numbers, so `25544` becomes `25544.0`.
    /// This normalizes whole-number f64s back to integers before deserializing,
    /// so `u32`/`i32`/`u8` fields work correctly.
    ///
    /// Returns `None` if not a Data part. Returns `Err` if deserialization fails.
    pub fn parse_data<T: serde::de::DeserializeOwned>(&self) -> Option<Result<T, crate::error::A2aTypeError>> {
        let json = self.as_data()?;
        let normalized = normalize_proto_numbers_for_deser(json);
        Some(
            serde_json::from_value(normalized)
                .map_err(|e| crate::error::A2aTypeError::Deserialization(e.to_string()))
        )
    }

    pub fn as_proto(&self) -> &pb::Part {
        &self.inner
    }

    pub fn into_proto(self) -> pb::Part {
        self.inner
    }
}

/// Wrap a proto Part without validation (content may be None).
/// Use `TryFrom` when you need to validate content is present.
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

    /// Returns individual text parts, preserving part boundaries.
    /// This is the primary safe accessor for message text content.
    /// Callers decide how to combine parts.
    pub fn text_parts(&self) -> Vec<&str> {
        self.inner
            .parts
            .iter()
            .filter_map(|p| match &p.content {
                Some(pb::part::Content::Text(t)) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Convenience: joins all text parts with a single space.
    /// Use `text_parts()` when part boundaries matter (e.g., multi-part prompts).
    pub fn joined_text(&self) -> String {
        self.text_parts().join(" ")
    }

    /// Raw JSON data parts (no number normalization).
    pub fn data_parts(&self) -> Vec<serde_json::Value> {
        self.inner
            .parts
            .iter()
            .filter_map(|p| match &p.content {
                Some(pb::part::Content::Data(proto_struct)) => {
                    serde_json::to_value(proto_struct).ok()
                }
                _ => None,
            })
            .collect()
    }

    /// Deserialize the first Data part into `T`, normalizing proto f64 integers.
    ///
    /// Returns `None` if no Data part exists. Returns `Err` if deserialization fails.
    pub fn parse_first_data<T: serde::de::DeserializeOwned>(&self) -> Option<Result<T, crate::error::A2aTypeError>> {
        for part in &self.inner.parts {
            if let Some(pb::part::Content::Data(proto_struct)) = &part.content {
                if let Ok(json) = serde_json::to_value(proto_struct) {
                    let normalized = normalize_proto_numbers_for_deser(json);
                    return Some(
                        serde_json::from_value(normalized)
                            .map_err(|e| crate::error::A2aTypeError::Deserialization(e.to_string()))
                    );
                }
            }
        }
        None
    }

    /// Deserialize from the first Data part, falling back to parsing the first
    /// Text part as JSON.
    ///
    /// A2A protocol v0.3 clients (e.g., Strands `A2AAgent` via a2a-sdk) send
    /// typed JSON as a text part (`{"kind":"text","text":"{...}"}`), while
    /// Turul clients send it as a data part. This method handles both.
    ///
    /// Preference order: Data part (proto struct) → Text part (JSON string).
    /// Returns `None` if no Data or parseable Text part exists.
    pub fn parse_first_data_or_text<T: serde::de::DeserializeOwned>(
        &self,
    ) -> Option<Result<T, crate::error::A2aTypeError>> {
        // Try Data part first (Turul clients)
        if let Some(result) = self.parse_first_data() {
            return Some(result);
        }

        // Fall back to first Text part parsed as JSON (a2a-sdk / Strands clients)
        for part in &self.inner.parts {
            if let Some(pb::part::Content::Text(text)) = &part.content {
                if text.trim_start().starts_with('{') {
                    return Some(
                        serde_json::from_str(text)
                            .map_err(|e| crate::error::A2aTypeError::Deserialization(e.to_string()))
                    );
                }
            }
        }

        None
    }

    pub fn as_proto(&self) -> &pb::Message {
        &self.inner
    }

    pub fn into_proto(self) -> pb::Message {
        self.inner
    }
}

impl TryFrom<pb::Message> for Message {
    type Error = crate::error::A2aTypeError;

    fn try_from(inner: pb::Message) -> Result<Self, Self::Error> {
        // Validate role is not UNSPECIFIED
        let role_val = pb::Role::try_from(inner.role)
            .unwrap_or(pb::Role::Unspecified);
        if role_val == pb::Role::Unspecified {
            return Err(crate::error::A2aTypeError::MissingField("role"));
        }
        Ok(Self { inner })
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
        let proto = pb::Message::deserialize(deserializer)?;
        Message::try_from(proto).map_err(serde::de::Error::custom)
    }
}

/// Normalize proto f64 whole numbers to integers for typed deserialization.
///
/// Protobuf `Value.number_value` is always f64. This converts finite whole-number
/// f64s back to JSON integers so serde can deserialize into u32/i32/u8/etc.
/// Fractional values and out-of-range numbers are left unchanged.
///
/// Internal — callers use `Part::parse_data::<T>()` or `Message::parse_first_data::<T>()`.
fn normalize_proto_numbers_for_deser(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f.is_finite() && f.fract() == 0.0 {
                    if f >= 0.0 && f <= u64::MAX as f64 {
                        return serde_json::Value::Number((f as u64).into());
                    } else if f >= i64::MIN as f64 && f <= i64::MAX as f64 {
                        return serde_json::Value::Number((f as i64).into());
                    }
                }
            }
            serde_json::Value::Number(n)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(normalize_proto_numbers_for_deser).collect())
        }
        serde_json::Value::Object(map) => {
            serde_json::Value::Object(
                map.into_iter()
                    .map(|(k, v)| (k, normalize_proto_numbers_for_deser(v)))
                    .collect(),
            )
        }
        other => other,
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

    #[test]
    fn message_try_from_proto_rejects_unspecified_role() {
        let proto_msg = pb::Message {
            message_id: "m-1".to_string(),
            role: pb::Role::Unspecified.into(),
            parts: vec![],
            context_id: String::new(),
            task_id: String::new(),
            metadata: None,
            extensions: vec![],
            reference_task_ids: vec![],
        };
        assert!(Message::try_from(proto_msg).is_err());
    }

    #[test]
    fn message_try_from_proto_accepts_valid_role() {
        let proto_msg = pb::Message {
            message_id: "m-2".to_string(),
            role: pb::Role::User.into(),
            parts: vec![],
            context_id: String::new(),
            task_id: String::new(),
            metadata: None,
            extensions: vec![],
            reference_task_ids: vec![],
        };
        let msg = Message::try_from(proto_msg).unwrap();
        assert_eq!(msg.message_id(), "m-2");
    }

    #[test]
    fn message_json_deserialization_rejects_unspecified_role() {
        let json = r#"{"messageId":"m-bad","role":"ROLE_UNSPECIFIED","parts":[]}"#;
        let result: Result<Message, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // =========================================================
    // Proto number normalization tests
    // =========================================================

    #[test]
    fn as_data_returns_raw_json_without_normalization() {
        let part = Part::data(serde_json::json!({"count": 25544}));
        let json = part.as_data().unwrap();
        // Proto f64: 25544 → 25544.0 in raw JSON
        let count = json.get("count").unwrap();
        assert!(count.is_f64() || count.is_u64(), "Raw JSON may be f64 from proto: {count}");
    }

    #[test]
    fn parse_data_normalizes_integers_for_typed_deser() {
        #[derive(serde::Deserialize)]
        struct MyData {
            count: u32,
            name: String,
        }

        let part = Part::data(serde_json::json!({"count": 25544, "name": "test"}));
        let result: MyData = part.parse_data().unwrap().unwrap();
        assert_eq!(result.count, 25544);
        assert_eq!(result.name, "test");
    }

    #[test]
    fn parse_data_preserves_fractional_numbers() {
        #[derive(serde::Deserialize)]
        struct MyData {
            ratio: f64,
        }

        let part = Part::data(serde_json::json!({"ratio": 1.5}));
        let result: MyData = part.parse_data().unwrap().unwrap();
        assert!((result.ratio - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_data_handles_nested_structures() {
        #[derive(serde::Deserialize)]
        struct Inner {
            value: u16,
        }
        #[derive(serde::Deserialize)]
        struct Outer {
            items: Vec<Inner>,
        }

        let part = Part::data(serde_json::json!({
            "items": [{"value": 42}, {"value": 100}]
        }));
        let result: Outer = part.parse_data().unwrap().unwrap();
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].value, 42);
        assert_eq!(result.items[1].value, 100);
    }

    #[test]
    fn parse_data_returns_none_for_non_data_part() {
        let part = Part::text("hello");
        assert!(part.parse_data::<serde_json::Value>().is_none());
    }

    #[test]
    fn message_parse_first_data_works() {
        #[derive(serde::Deserialize)]
        struct Req {
            id: u32,
        }

        let msg = Message::new(
            "m-1",
            Role::User,
            vec![
                Part::text("some text"),
                Part::data(serde_json::json!({"id": 12345})),
            ],
        );

        let result: Req = msg.parse_first_data().unwrap().unwrap();
        assert_eq!(result.id, 12345);
    }

    #[test]
    fn normalize_whole_numbers_to_integers() {
        let input = serde_json::json!({"a": 25544.0, "b": 1.5, "c": -10.0});
        let output = normalize_proto_numbers_for_deser(input);
        assert!(output["a"].is_u64(), "25544.0 should become integer");
        assert!(output["b"].is_f64(), "1.5 should stay f64");
        assert!(output["c"].is_i64(), "-10.0 should become negative integer");
    }

    #[test]
    fn parse_first_data_or_text_prefers_data() {
        #[derive(serde::Deserialize)]
        struct Req {
            id: u32,
        }

        let msg = Message::new(
            "m-1",
            Role::User,
            vec![
                Part::text(r#"{"id": 99}"#),
                Part::data(serde_json::json!({"id": 42})),
            ],
        );

        // Data part (id=42) should be preferred over text part (id=99)
        let result: Req = msg.parse_first_data_or_text().unwrap().unwrap();
        assert_eq!(result.id, 42);
    }

    #[test]
    fn parse_first_data_or_text_falls_back_to_text() {
        // Simulates Strands A2AAgent sending JSON as a text part
        #[derive(serde::Deserialize)]
        struct Req {
            skill: String,
            version: String,
        }

        let msg = Message::new(
            "m-1",
            Role::User,
            vec![Part::text(r#"{"skill": "solar_elevation", "version": "1.0"}"#)],
        );

        let result: Req = msg.parse_first_data_or_text().unwrap().unwrap();
        assert_eq!(result.skill, "solar_elevation");
        assert_eq!(result.version, "1.0");
    }

    #[test]
    fn parse_first_data_or_text_ignores_non_json_text() {
        let msg = Message::new(
            "m-1",
            Role::User,
            vec![Part::text("hello world")],
        );

        assert!(msg.parse_first_data_or_text::<serde_json::Value>().is_none());
    }
}
