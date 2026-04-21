/// Raw prost-generated types for the A2A Protocol v1.0 (package lf.a2a.v1).
///
/// These types are generated from the normative `a2a.proto` definition.
/// Use `turul-a2a-types` for ergonomic wrapper types with state machine
/// enforcement and builder patterns.
pub mod lf {
    pub mod a2a {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/lf.a2a.v1.rs"));
            include!(concat!(env!("OUT_DIR"), "/lf.a2a.v1.serde.rs"));
        }
    }
}

// Convenience re-export
pub use lf::a2a::v1::*;

/// Tonic-generated gRPC service + client stubs for `lf.a2a.v1.A2AService`.
///
/// Enabled by the `grpc` Cargo feature (ADR-014 §2.2). Default HTTP+JSON
/// builds do not pull in tonic.
#[cfg(feature = "grpc")]
pub mod grpc {
    pub use crate::lf::a2a::v1::a2a_service_server::{A2aService, A2aServiceServer};
    pub use crate::lf::a2a::v1::a2a_service_client::A2aServiceClient;
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================
    // Proto generation verification tests
    // Validates that prost generated all expected types from a2a.proto
    // =========================================================

    #[test]
    fn task_state_has_nine_variants() {
        // Verify all 9 TaskState values from proto (lines 187-208)
        assert_eq!(TaskState::Unspecified as i32, 0);
        assert_eq!(TaskState::Submitted as i32, 1);
        assert_eq!(TaskState::Working as i32, 2);
        assert_eq!(TaskState::Completed as i32, 3);
        assert_eq!(TaskState::Failed as i32, 4);
        assert_eq!(TaskState::Canceled as i32, 5);
        assert_eq!(TaskState::InputRequired as i32, 6);
        assert_eq!(TaskState::Rejected as i32, 7);
        assert_eq!(TaskState::AuthRequired as i32, 8);
    }

    #[test]
    fn role_has_three_variants() {
        // Verify all 3 Role values from proto (lines 244-252)
        assert_eq!(Role::Unspecified as i32, 0);
        assert_eq!(Role::User as i32, 1);
        assert_eq!(Role::Agent as i32, 2);
    }

    #[test]
    fn task_has_required_fields() {
        // Verify Task struct exists with correct field names (proto lines 167-184)
        let task = Task {
            id: "test-id".to_string(),
            context_id: "ctx-1".to_string(),
            status: Some(TaskStatus {
                state: TaskState::Submitted.into(),
                message: None,
                timestamp: None,
            }),
            artifacts: vec![],
            history: vec![], // NOT "messages" — proto field is "history"
            metadata: None,
        };
        assert_eq!(task.id, "test-id");
        assert!(task.history.is_empty()); // field is called "history"
    }

    #[test]
    fn message_has_required_fields() {
        // Verify Message struct (proto lines 260-277)
        let msg = Message {
            message_id: "msg-1".to_string(),
            context_id: "ctx-1".to_string(),
            task_id: "task-1".to_string(),
            role: Role::User.into(),
            parts: vec![],
            metadata: None,
            extensions: vec![],
            reference_task_ids: vec![],
        };
        assert_eq!(msg.message_id, "msg-1");
        assert_eq!(msg.role, i32::from(Role::User));
    }

    #[test]
    fn part_oneof_has_four_content_variants() {
        // Verify Part oneof content: text, raw, url, data (proto lines 224-234)
        let text_part = Part {
            content: Some(part::Content::Text("hello".to_string())),
            metadata: None,
            filename: String::new(),
            media_type: String::new(),
        };

        let raw_part = Part {
            content: Some(part::Content::Raw(vec![0x00, 0x01])),
            metadata: None,
            filename: "file.bin".to_string(),
            media_type: "application/octet-stream".to_string(),
        };

        let url_part = Part {
            content: Some(part::Content::Url(
                "https://example.com/doc.pdf".to_string(),
            )),
            metadata: None,
            filename: "doc.pdf".to_string(),
            media_type: "application/pdf".to_string(),
        };

        let data_part = Part {
            content: Some(part::Content::Data(pbjson_types::Value {
                kind: Some(pbjson_types::value::Kind::StructValue(
                    pbjson_types::Struct {
                        fields: std::collections::HashMap::new(),
                    },
                )),
            })),
            metadata: None,
            filename: String::new(),
            media_type: "application/json".to_string(),
        };

        // All four variants should be constructable
        assert!(matches!(text_part.content, Some(part::Content::Text(_))));
        assert!(matches!(raw_part.content, Some(part::Content::Raw(_))));
        assert!(matches!(url_part.content, Some(part::Content::Url(_))));
        assert!(matches!(data_part.content, Some(part::Content::Data(_))));
    }

    #[test]
    fn security_scheme_oneof_has_five_variants() {
        // Verify SecurityScheme oneof (proto lines 497-510)
        let api_key = security_scheme::Scheme::ApiKeySecurityScheme(ApiKeySecurityScheme {
            description: String::new(),
            location: "header".to_string(),
            name: "X-API-Key".to_string(),
        });

        let http_auth = security_scheme::Scheme::HttpAuthSecurityScheme(HttpAuthSecurityScheme {
            description: String::new(),
            scheme: "Bearer".to_string(),
            bearer_format: "JWT".to_string(),
        });

        let oauth2 = security_scheme::Scheme::Oauth2SecurityScheme(OAuth2SecurityScheme {
            description: String::new(),
            flows: None,
            oauth2_metadata_url: String::new(),
        });

        let oidc =
            security_scheme::Scheme::OpenIdConnectSecurityScheme(OpenIdConnectSecurityScheme {
                description: String::new(),
                open_id_connect_url: "https://example.com/.well-known/openid-configuration"
                    .to_string(),
            });

        let mtls = security_scheme::Scheme::MtlsSecurityScheme(MutualTlsSecurityScheme {
            description: String::new(),
        });

        // All five constructable
        assert!(matches!(
            api_key,
            security_scheme::Scheme::ApiKeySecurityScheme(_)
        ));
        assert!(matches!(
            http_auth,
            security_scheme::Scheme::HttpAuthSecurityScheme(_)
        ));
        assert!(matches!(
            oauth2,
            security_scheme::Scheme::Oauth2SecurityScheme(_)
        ));
        assert!(matches!(
            oidc,
            security_scheme::Scheme::OpenIdConnectSecurityScheme(_)
        ));
        assert!(matches!(
            mtls,
            security_scheme::Scheme::MtlsSecurityScheme(_)
        ));
    }

    #[test]
    fn task_status_message_is_message_type() {
        // TaskStatus.message is a Message, NOT a string (proto lines 211-219)
        let status = TaskStatus {
            state: TaskState::Working.into(),
            message: Some(Message {
                message_id: "status-msg".to_string(),
                role: Role::Agent.into(),
                parts: vec![Part {
                    content: Some(part::Content::Text("Processing...".to_string())),
                    metadata: None,
                    filename: String::new(),
                    media_type: String::new(),
                }],
                context_id: String::new(),
                task_id: String::new(),
                metadata: None,
                extensions: vec![],
                reference_task_ids: vec![],
            }),
            timestamp: None,
        };
        assert!(status.message.is_some());
        assert_eq!(status.message.unwrap().message_id, "status-msg");
    }

    #[test]
    fn task_artifact_update_event_has_append_and_last_chunk() {
        // TaskArtifactUpdateEvent has append + last_chunk (proto lines 307-322)
        let event = TaskArtifactUpdateEvent {
            task_id: "task-1".to_string(),
            context_id: "ctx-1".to_string(),
            artifact: Some(Artifact {
                artifact_id: "art-1".to_string(),
                name: String::new(),
                description: String::new(),
                parts: vec![],
                metadata: None,
                extensions: vec![],
            }),
            append: true,
            last_chunk: false,
            metadata: None,
        };
        assert!(event.append);
        assert!(!event.last_chunk);
    }

    #[test]
    fn agent_card_has_required_fields() {
        // AgentCard REQUIRED fields (proto lines 356-393)
        let card = AgentCard {
            name: "Test Agent".to_string(),
            description: "A test agent".to_string(),
            supported_interfaces: vec![AgentInterface {
                url: "https://example.com/a2a".to_string(),
                protocol_binding: "JSONRPC".to_string(),
                tenant: String::new(),
                protocol_version: "1.0".to_string(),
            }],
            provider: None,
            version: "1.0.0".to_string(),
            documentation_url: None,
            capabilities: Some(AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(false),
                extensions: vec![],
                extended_agent_card: Some(false),
            }),
            security_schemes: std::collections::HashMap::new(),
            security_requirements: vec![],
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
            skills: vec![AgentSkill {
                id: "echo".to_string(),
                name: "Echo".to_string(),
                description: "Echoes input".to_string(),
                tags: vec!["echo".to_string()],
                examples: vec![],
                input_modes: vec![],
                output_modes: vec![],
                security_requirements: vec![],
            }],
            signatures: vec![],
            icon_url: None,
        };
        assert_eq!(card.name, "Test Agent");
        assert!(!card.default_input_modes.is_empty());
        assert!(!card.default_output_modes.is_empty());
    }

    #[test]
    fn list_tasks_response_has_required_fields() {
        // ListTasksResponse REQUIRED: tasks, next_page_token, page_size, total_size
        // (proto lines 694-703)
        let response = ListTasksResponse {
            tasks: vec![],
            next_page_token: "".to_string(),
            page_size: 10,
            total_size: 0,
        };
        assert_eq!(response.page_size, 10);
        assert_eq!(response.total_size, 0);
        assert!(response.next_page_token.is_empty());
    }

    #[test]
    fn send_message_request_has_tenant_field() {
        // Most request types have tenant (proto line 644)
        let req = SendMessageRequest {
            tenant: "my-tenant".to_string(),
            message: None,
            configuration: None,
            metadata: None,
        };
        assert_eq!(req.tenant, "my-tenant");
    }

    #[test]
    fn send_message_response_is_oneof() {
        // SendMessageResponse wraps oneof { task, message } (proto lines 764-772)
        let task_response = SendMessageResponse {
            payload: Some(send_message_response::Payload::Task(Task {
                id: "t-1".to_string(),
                context_id: String::new(),
                status: None,
                artifacts: vec![],
                history: vec![],
                metadata: None,
            })),
        };
        assert!(matches!(
            task_response.payload,
            Some(send_message_response::Payload::Task(_))
        ));

        let msg_response = SendMessageResponse {
            payload: Some(send_message_response::Payload::Message(Message {
                message_id: "m-1".to_string(),
                context_id: String::new(),
                task_id: String::new(),
                role: Role::Agent.into(),
                parts: vec![],
                metadata: None,
                extensions: vec![],
                reference_task_ids: vec![],
            })),
        };
        assert!(matches!(
            msg_response.payload,
            Some(send_message_response::Payload::Message(_))
        ));
    }

    #[test]
    fn stream_response_has_four_variants() {
        // StreamResponse oneof payload (proto lines 774-787)
        let _task_variant = stream_response::Payload::Task(Task::default());
        let _msg_variant = stream_response::Payload::Message(Message::default());
        let _status_variant =
            stream_response::Payload::StatusUpdate(TaskStatusUpdateEvent::default());
        let _artifact_variant =
            stream_response::Payload::ArtifactUpdate(TaskArtifactUpdateEvent::default());
    }

    // =========================================================
    // Proto REQUIRED field verification
    // Reads a2a.proto source to verify field_behavior annotations
    // haven't been removed — catches spec drift at test time
    // =========================================================

    #[test]
    fn proto_required_annotations_present() {
        let proto_source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../proto/a2a.proto"
        ))
        .expect("should read a2a.proto");

        // Each tuple: (message context, field pattern that must have REQUIRED)
        let required_fields = [
            // Task (proto lines 170-176)
            ("Task", "string id = 1", "REQUIRED"),
            ("Task", "TaskStatus status = 3", "REQUIRED"),
            // Message (proto lines 262-270)
            ("Message", "string message_id = 1", "REQUIRED"),
            ("Message", "Role role = 4", "REQUIRED"),
            ("Message", "repeated Part parts = 5", "REQUIRED"),
            // Artifact (proto lines 282-288)
            ("Artifact", "string artifact_id = 1", "REQUIRED"),
            ("Artifact", "repeated Part parts = 4", "REQUIRED"),
            // TaskStatus (proto lines 213)
            ("TaskStatus", "TaskState state = 1", "REQUIRED"),
            // TaskStatusUpdateEvent (proto lines 298-302)
            ("TaskStatusUpdateEvent", "string task_id = 1", "REQUIRED"),
            ("TaskStatusUpdateEvent", "string context_id = 2", "REQUIRED"),
            ("TaskStatusUpdateEvent", "TaskStatus status = 3", "REQUIRED"),
            // TaskArtifactUpdateEvent (proto lines 310-314)
            ("TaskArtifactUpdateEvent", "string task_id = 1", "REQUIRED"),
            (
                "TaskArtifactUpdateEvent",
                "string context_id = 2",
                "REQUIRED",
            ),
            (
                "TaskArtifactUpdateEvent",
                "Artifact artifact = 3",
                "REQUIRED",
            ),
            // AgentCard (proto lines 359-388)
            ("AgentCard", "string name = 1", "REQUIRED"),
            ("AgentCard", "string description = 2", "REQUIRED"),
            (
                "AgentCard",
                "repeated AgentInterface supported_interfaces = 3",
                "REQUIRED",
            ),
            ("AgentCard", "string version = 5", "REQUIRED"),
            (
                "AgentCard",
                "AgentCapabilities capabilities = 7",
                "REQUIRED",
            ),
            (
                "AgentCard",
                "repeated string default_input_modes = 10",
                "REQUIRED",
            ),
            (
                "AgentCard",
                "repeated string default_output_modes = 11",
                "REQUIRED",
            ),
            ("AgentCard", "repeated AgentSkill skills = 12", "REQUIRED"),
            // AgentInterface (proto lines 339-349)
            ("AgentInterface", "string url = 1", "REQUIRED"),
            ("AgentInterface", "string protocol_binding = 2", "REQUIRED"),
            ("AgentInterface", "string protocol_version = 4", "REQUIRED"),
            // AgentSkill (proto lines 432-438)
            ("AgentSkill", "string id = 1", "REQUIRED"),
            ("AgentSkill", "string name = 2", "REQUIRED"),
            ("AgentSkill", "string description = 3", "REQUIRED"),
            ("AgentSkill", "repeated string tags = 4", "REQUIRED"),
            // ListTasksResponse (proto lines 696-702)
            ("ListTasksResponse", "repeated Task tasks = 1", "REQUIRED"),
            (
                "ListTasksResponse",
                "string next_page_token = 2",
                "REQUIRED",
            ),
            ("ListTasksResponse", "int32 page_size = 3", "REQUIRED"),
            ("ListTasksResponse", "int32 total_size = 4", "REQUIRED"),
            // SendMessageRequest (proto line 646)
            ("SendMessageRequest", "Message message = 2", "REQUIRED"),
            // GetTaskRequest (proto line 658)
            ("GetTaskRequest", "string id = 2", "REQUIRED"),
        ];

        for (msg, field_pattern, _annotation) in &required_fields {
            // Find lines containing the field pattern and verify they have REQUIRED
            let found = proto_source.lines().any(|line| {
                let trimmed = line.trim();
                trimmed.contains(field_pattern) && trimmed.contains("REQUIRED")
            });
            assert!(
                found,
                "Proto field '{field_pattern}' in {msg} must have field_behavior = REQUIRED annotation"
            );
        }
    }

    // =========================================================
    // JSON wire format tests
    // Verify pbjson generates correct camelCase JSON field names
    // =========================================================

    #[test]
    fn task_json_uses_camel_case_field_names() {
        let task = Task {
            id: "task-123".to_string(),
            context_id: "ctx-456".to_string(),
            status: Some(TaskStatus {
                state: TaskState::Submitted.into(),
                message: None,
                timestamp: None,
            }),
            artifacts: vec![],
            history: vec![],
            metadata: None,
        };
        let json = serde_json::to_value(&task).unwrap();
        // Verify camelCase: contextId (not context_id)
        assert!(
            json.get("contextId").is_some(),
            "expected camelCase 'contextId'"
        );
        // Proto3 JSON: empty repeated fields are omitted, but field name is "history" not "messages"
        // When history is populated, the field should appear as "history"
        assert!(
            json.get("messages").is_none(),
            "'messages' field should NOT exist"
        );

        // Verify history appears when populated
        let task_with_history = Task {
            id: "test-2".to_string(),
            context_id: String::new(),
            status: None,
            artifacts: vec![],
            history: vec![Message {
                message_id: "m-1".to_string(),
                role: Role::User.into(),
                parts: vec![],
                context_id: String::new(),
                task_id: String::new(),
                metadata: None,
                extensions: vec![],
                reference_task_ids: vec![],
            }],
            metadata: None,
        };
        let json2 = serde_json::to_value(&task_with_history).unwrap();
        assert!(
            json2.get("history").is_some(),
            "expected 'history' field when populated"
        );
    }

    #[test]
    fn task_state_serializes_as_string() {
        // TaskState should serialize as string enum name
        let status = TaskStatus {
            state: TaskState::Submitted.into(),
            message: None,
            timestamp: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        let state_val = json.get("state").unwrap();
        assert_eq!(state_val, "TASK_STATE_SUBMITTED");
    }

    #[test]
    fn all_task_states_serialize_correctly() {
        // Proto3 JSON: UNSPECIFIED (0) is the default and may be omitted from JSON.
        // Non-default states MUST serialize as their string names.
        let non_default_states = vec![
            (TaskState::Submitted, "TASK_STATE_SUBMITTED"),
            (TaskState::Working, "TASK_STATE_WORKING"),
            (TaskState::Completed, "TASK_STATE_COMPLETED"),
            (TaskState::Failed, "TASK_STATE_FAILED"),
            (TaskState::Canceled, "TASK_STATE_CANCELED"),
            (TaskState::InputRequired, "TASK_STATE_INPUT_REQUIRED"),
            (TaskState::Rejected, "TASK_STATE_REJECTED"),
            (TaskState::AuthRequired, "TASK_STATE_AUTH_REQUIRED"),
        ];

        for (state, expected_str) in non_default_states {
            let status = TaskStatus {
                state: state.into(),
                message: None,
                timestamp: None,
            };
            let json = serde_json::to_value(&status).unwrap();
            assert_eq!(
                json.get("state").unwrap(),
                expected_str,
                "TaskState::{state:?} should serialize as {expected_str}"
            );
        }

        // UNSPECIFIED (0) is the proto3 default — it may be omitted or present as the string
        let unspecified_status = TaskStatus {
            state: TaskState::Unspecified.into(),
            message: None,
            timestamp: None,
        };
        let json = serde_json::to_value(&unspecified_status).unwrap();
        // If present, it should be the correct string; if absent, that's valid proto3 JSON
        if let Some(state_val) = json.get("state") {
            assert_eq!(state_val, "TASK_STATE_UNSPECIFIED");
        }
    }

    #[test]
    fn part_text_json_structure() {
        let part = Part {
            content: Some(part::Content::Text("hello world".to_string())),
            metadata: None,
            filename: String::new(),
            media_type: "text/plain".to_string(),
        };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json.get("text").unwrap(), "hello world");
        assert_eq!(json.get("mediaType").unwrap(), "text/plain");
    }

    #[test]
    fn part_url_json_structure() {
        let part = Part {
            content: Some(part::Content::Url(
                "https://example.com/file.pdf".to_string(),
            )),
            metadata: None,
            filename: "file.pdf".to_string(),
            media_type: "application/pdf".to_string(),
        };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json.get("url").unwrap(), "https://example.com/file.pdf");
        assert_eq!(json.get("filename").unwrap(), "file.pdf");
        assert_eq!(json.get("mediaType").unwrap(), "application/pdf");
    }

    #[test]
    fn part_raw_json_is_base64() {
        let part = Part {
            content: Some(part::Content::Raw(vec![0x48, 0x65, 0x6c, 0x6c, 0x6f])),
            metadata: None,
            filename: String::new(),
            media_type: "application/octet-stream".to_string(),
        };
        let json = serde_json::to_value(&part).unwrap();
        // bytes are base64 in JSON per proto spec
        let raw_val = json.get("raw").unwrap().as_str().unwrap();
        assert!(!raw_val.is_empty(), "raw should be base64 encoded");
    }

    #[test]
    fn agent_card_json_has_default_modes() {
        let card = AgentCard {
            name: "Test".to_string(),
            description: "Test".to_string(),
            supported_interfaces: vec![],
            provider: None,
            version: "1.0".to_string(),
            documentation_url: None,
            capabilities: None,
            security_schemes: std::collections::HashMap::new(),
            security_requirements: vec![],
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["application/json".to_string()],
            skills: vec![],
            signatures: vec![],
            icon_url: None,
        };
        let json = serde_json::to_value(&card).unwrap();
        assert!(
            json.get("defaultInputModes").is_some(),
            "expected camelCase 'defaultInputModes'"
        );
        assert!(
            json.get("defaultOutputModes").is_some(),
            "expected camelCase 'defaultOutputModes'"
        );
    }

    #[test]
    fn list_tasks_response_json_has_required_fields() {
        let response = ListTasksResponse {
            tasks: vec![],
            next_page_token: "abc".to_string(),
            page_size: 50,
            total_size: 100,
        };
        let json = serde_json::to_value(&response).unwrap();
        assert!(json.get("nextPageToken").is_some());
        assert!(json.get("pageSize").is_some());
        assert!(json.get("totalSize").is_some());
    }

    #[test]
    fn task_artifact_update_event_json_has_chunk_fields() {
        let event = TaskArtifactUpdateEvent {
            task_id: "t-1".to_string(),
            context_id: "c-1".to_string(),
            artifact: None,
            append: true,
            last_chunk: true,
            metadata: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json.get("append").unwrap(), true);
        assert_eq!(json.get("lastChunk").unwrap(), true);
    }

    #[test]
    fn send_message_request_json_has_tenant() {
        let req = SendMessageRequest {
            tenant: "test-tenant".to_string(),
            message: None,
            configuration: None,
            metadata: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json.get("tenant").unwrap(), "test-tenant");
    }

    #[test]
    fn task_status_message_serializes_as_object() {
        // TaskStatus.message is a Message object, NOT a string
        let status = TaskStatus {
            state: TaskState::Working.into(),
            message: Some(Message {
                message_id: "msg-1".to_string(),
                role: Role::Agent.into(),
                parts: vec![Part {
                    content: Some(part::Content::Text("Working on it".to_string())),
                    metadata: None,
                    filename: String::new(),
                    media_type: String::new(),
                }],
                context_id: String::new(),
                task_id: String::new(),
                metadata: None,
                extensions: vec![],
                reference_task_ids: vec![],
            }),
            timestamp: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        let message_val = json.get("message").unwrap();
        // Must be an object, not a string
        assert!(
            message_val.is_object(),
            "TaskStatus.message must be a Message object, not a string"
        );
        assert!(message_val.get("messageId").is_some());
        assert!(message_val.get("role").is_some());
        assert!(message_val.get("parts").is_some());
    }

    #[test]
    fn json_round_trip_task() {
        let task = Task {
            id: "rt-1".to_string(),
            context_id: "ctx-rt".to_string(),
            status: Some(TaskStatus {
                state: TaskState::Working.into(),
                message: None,
                timestamp: None,
            }),
            artifacts: vec![],
            history: vec![Message {
                message_id: "m-1".to_string(),
                context_id: String::new(),
                task_id: String::new(),
                role: Role::User.into(),
                parts: vec![Part {
                    content: Some(part::Content::Text("hello".to_string())),
                    metadata: None,
                    filename: String::new(),
                    media_type: String::new(),
                }],
                metadata: None,
                extensions: vec![],
                reference_task_ids: vec![],
            }],
            metadata: None,
        };

        let json_str = serde_json::to_string(&task).unwrap();
        let deserialized: Task = serde_json::from_str(&json_str).unwrap();
        assert_eq!(deserialized.id, "rt-1");
        assert_eq!(deserialized.history.len(), 1);
        assert_eq!(deserialized.history[0].message_id, "m-1");
    }
}
