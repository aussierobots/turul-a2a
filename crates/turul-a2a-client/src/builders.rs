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
    reference_task_ids: Vec<String>,
    metadata: Option<pbjson_types::Struct>,
    // Request-root fields.
    tenant: String,
    // Fields on `SendMessageRequest.configuration`. The builder only
    // emits a `Some(SendMessageConfiguration)` when one of these is
    // populated; callers that never touch them see `configuration: None`
    // on the wire.
    return_immediately: bool,
    accepted_output_modes: Vec<String>,
    history_length: Option<i32>,
    task_push_notification_config: Option<pb::TaskPushNotificationConfig>,
}

impl MessageBuilder {
    pub fn new() -> Self {
        Self {
            message_id: uuid::Uuid::now_v7().to_string(),
            role: pb::Role::User.into(),
            parts: vec![],
            context_id: String::new(),
            task_id: String::new(),
            reference_task_ids: vec![],
            metadata: None,
            tenant: String::new(),
            return_immediately: false,
            accepted_output_modes: vec![],
            history_length: None,
            task_push_notification_config: None,
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

    /// Set the task ID to continue an existing task. The framework
    /// only accepts continuations on tasks in the `INPUT_REQUIRED` or
    /// `AUTH_REQUIRED` interrupted states (per A2A spec §3.4).
    pub fn task_id(mut self, id: impl Into<String>) -> Self {
        self.task_id = id.into();
        self
    }

    /// Mark this message as a refinement of one or more prior tasks
    /// in the same `contextId`. The serving agent uses these ids as a
    /// hint to resolve which prior artifacts/results the refinement
    /// is operating on. Refinement always creates a *new* task — see
    /// the A2A "Life of a Task" / Task Immutability section.
    pub fn reference_task_ids<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.reference_task_ids = ids.into_iter().map(Into::into).collect();
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

    /// Set the request-root `SendMessageRequest.tenant`.
    pub fn tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
        self
    }

    /// Set `SendMessageConfiguration.return_immediately`. When `true`,
    /// the server creates the task, returns `WORKING` immediately, and
    /// continues executor work in the background.
    pub fn return_immediately(mut self, flag: bool) -> Self {
        self.return_immediately = flag;
        self
    }

    /// Set `SendMessageConfiguration.accepted_output_modes`. Populates
    /// the list in full; calling again replaces the previous value.
    pub fn accepted_output_modes<I, S>(mut self, modes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.accepted_output_modes = modes.into_iter().map(Into::into).collect();
        self
    }

    /// Set `SendMessageConfiguration.history_length`. Preserves the
    /// proto tri-state (unset = unbounded, `Some(0)` = empty,
    /// `Some(n)` = last n messages); pass an `i32` to set, or call
    /// [`Self::clear_history_length`] to unset after having set it.
    pub fn history_length(mut self, n: i32) -> Self {
        self.history_length = Some(n);
        self
    }

    /// Unset `SendMessageConfiguration.history_length` (restore
    /// unbounded default).
    pub fn clear_history_length(mut self) -> Self {
        self.history_length = None;
        self
    }

    /// Set `SendMessageConfiguration.task_push_notification_config`
    /// for inline (atomic) registration. `task_id` is left empty for
    /// inline registration; the server assigns it during task
    /// creation. `authentication` is left unset; use
    /// [`Self::push_config`] for a pre-built struct when you need it.
    pub fn inline_push_config(mut self, url: impl Into<String>, token: impl Into<String>) -> Self {
        self.task_push_notification_config = Some(pb::TaskPushNotificationConfig {
            tenant: String::new(),
            id: String::new(),
            task_id: String::new(),
            url: url.into(),
            token: token.into(),
            authentication: None,
        });
        self
    }

    /// Set `SendMessageConfiguration.task_push_notification_config`
    /// from a pre-built proto struct. Use this when you need to
    /// populate `authentication` or pre-mint a config `id`;
    /// [`Self::inline_push_config`] covers the 80% case.
    pub fn push_config(mut self, cfg: pb::TaskPushNotificationConfig) -> Self {
        self.task_push_notification_config = Some(cfg);
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
        // Only emit `Some(SendMessageConfiguration)` when the caller
        // actually populated one of its fields; otherwise the server
        // sees `configuration: None` — behaviour-identical to the
        // pre-0.1.15 builder for callers not touching these methods.
        let has_config = self.return_immediately
            || !self.accepted_output_modes.is_empty()
            || self.history_length.is_some()
            || self.task_push_notification_config.is_some();
        let configuration = if has_config {
            Some(pb::SendMessageConfiguration {
                accepted_output_modes: self.accepted_output_modes,
                task_push_notification_config: self.task_push_notification_config,
                history_length: self.history_length,
                return_immediately: self.return_immediately,
            })
        } else {
            None
        };
        pb::SendMessageRequest {
            message: Some(pb::Message {
                message_id: self.message_id,
                role: self.role,
                parts: self.parts,
                context_id: self.context_id,
                task_id: self.task_id,
                metadata: self.metadata,
                extensions: vec![],
                reference_task_ids: self.reference_task_ids,
            }),
            configuration,
            metadata: None,
            tenant: self.tenant,
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
    fn default_build_leaves_configuration_none() {
        let req: pb::SendMessageRequest = MessageBuilder::new().text("hi").build();
        assert!(req.configuration.is_none());
        assert_eq!(req.tenant, "");
    }

    #[test]
    fn tenant_sets_request_root() {
        let req: pb::SendMessageRequest =
            MessageBuilder::new().text("hi").tenant("tenant-a").build();
        assert_eq!(req.tenant, "tenant-a");
    }

    #[test]
    fn return_immediately_populates_configuration() {
        let req: pb::SendMessageRequest = MessageBuilder::new()
            .text("hi")
            .return_immediately(true)
            .build();
        let cfg = req.configuration.expect("configuration set");
        assert!(cfg.return_immediately);
        assert!(cfg.accepted_output_modes.is_empty());
        assert!(cfg.history_length.is_none());
        assert!(cfg.task_push_notification_config.is_none());
    }

    #[test]
    fn accepted_output_modes_populates_configuration() {
        let req: pb::SendMessageRequest = MessageBuilder::new()
            .text("hi")
            .accepted_output_modes(["text/plain", "application/json"])
            .build();
        let cfg = req.configuration.expect("configuration set");
        assert_eq!(
            cfg.accepted_output_modes,
            vec!["text/plain", "application/json"]
        );
    }

    #[test]
    fn history_length_preserves_tri_state() {
        // history_length is tri-state on the proto: None / Some(0) / Some(n>0).
        let req: pb::SendMessageRequest =
            MessageBuilder::new().text("hi").history_length(0).build();
        let cfg = req.configuration.expect("configuration set");
        assert_eq!(cfg.history_length, Some(0), "Some(0) must round-trip");

        let req_unset: pb::SendMessageRequest = MessageBuilder::new()
            .text("hi")
            .history_length(5)
            .clear_history_length()
            .build();
        assert!(req_unset.configuration.is_none(), "cleared => no config");

        let req_n: pb::SendMessageRequest =
            MessageBuilder::new().text("hi").history_length(42).build();
        let cfg = req_n.configuration.expect("configuration set");
        assert_eq!(cfg.history_length, Some(42));
    }

    #[test]
    fn inline_push_config_leaves_task_id_empty() {
        let req: pb::SendMessageRequest = MessageBuilder::new()
            .text("hi")
            .inline_push_config("https://cb.example.test/webhook", "tok-1")
            .build();
        let cfg = req.configuration.expect("configuration set");
        let push = cfg
            .task_push_notification_config
            .expect("inline push config set");
        assert_eq!(push.url, "https://cb.example.test/webhook");
        assert_eq!(push.token, "tok-1");
        assert_eq!(
            push.task_id, "",
            "task_id is left empty for inline registration; the server assigns it during task creation"
        );
        assert_eq!(push.id, "");
        assert_eq!(push.tenant, "");
        assert!(push.authentication.is_none());
    }

    #[test]
    fn push_config_raw_struct_passthrough() {
        let custom = pb::TaskPushNotificationConfig {
            tenant: String::new(),
            id: "pre-minted-id".into(),
            task_id: String::new(),
            url: "https://cb.example.test/webhook".into(),
            token: "tok-1".into(),
            authentication: None,
        };
        let req: pb::SendMessageRequest =
            MessageBuilder::new().text("hi").push_config(custom).build();
        let cfg = req.configuration.expect("configuration set");
        let push = cfg.task_push_notification_config.expect("push config set");
        assert_eq!(push.id, "pre-minted-id");
    }

    #[test]
    fn configuration_fields_compose() {
        let req: pb::SendMessageRequest = MessageBuilder::new()
            .text("hi")
            .tenant("tenant-a")
            .return_immediately(true)
            .accepted_output_modes(["text/plain"])
            .history_length(10)
            .inline_push_config("https://cb.example.test/webhook", "tok-1")
            .build();
        assert_eq!(req.tenant, "tenant-a");
        let cfg = req.configuration.expect("configuration set");
        assert!(cfg.return_immediately);
        assert_eq!(cfg.accepted_output_modes, vec!["text/plain"]);
        assert_eq!(cfg.history_length, Some(10));
        assert!(cfg.task_push_notification_config.is_some());
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
