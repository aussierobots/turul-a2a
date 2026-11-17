//! Centralized wire-format constants from the A2A v1.0 specification.
//!
//! All values derived from `a2a.proto` google.api.http annotations and
//! the spec's error code mapping table (Section 5.4).

/// JSON-RPC method names (PascalCase, from proto service definition)
pub mod jsonrpc {
    /// JSON-RPC method name for the `SendMessage` operation.
    pub const SEND_MESSAGE: &str = "SendMessage";
    /// JSON-RPC method name for the `SendStreamingMessage` operation.
    pub const SEND_STREAMING_MESSAGE: &str = "SendStreamingMessage";
    /// JSON-RPC method name for the `GetTask` operation.
    pub const GET_TASK: &str = "GetTask";
    /// JSON-RPC method name for the `ListTasks` operation.
    pub const LIST_TASKS: &str = "ListTasks";
    /// JSON-RPC method name for the `CancelTask` operation.
    pub const CANCEL_TASK: &str = "CancelTask";
    /// JSON-RPC method name for the `SubscribeToTask` operation.
    pub const SUBSCRIBE_TO_TASK: &str = "SubscribeToTask";
    /// JSON-RPC method name for the `CreateTaskPushNotificationConfig` operation.
    pub const CREATE_TASK_PUSH_NOTIFICATION_CONFIG: &str = "CreateTaskPushNotificationConfig";
    /// JSON-RPC method name for the `GetTaskPushNotificationConfig` operation.
    pub const GET_TASK_PUSH_NOTIFICATION_CONFIG: &str = "GetTaskPushNotificationConfig";
    /// JSON-RPC method name for the `ListTaskPushNotificationConfigs` operation.
    pub const LIST_TASK_PUSH_NOTIFICATION_CONFIGS: &str = "ListTaskPushNotificationConfigs";
    /// JSON-RPC method name for the `DeleteTaskPushNotificationConfig` operation.
    pub const DELETE_TASK_PUSH_NOTIFICATION_CONFIG: &str = "DeleteTaskPushNotificationConfig";
    /// JSON-RPC method name for the `GetExtendedAgentCard` operation.
    pub const GET_EXTENDED_AGENT_CARD: &str = "GetExtendedAgentCard";

    /// All JSON-RPC method names.
    pub const ALL_METHODS: &[&str] = &[
        SEND_MESSAGE,
        SEND_STREAMING_MESSAGE,
        GET_TASK,
        LIST_TASKS,
        CANCEL_TASK,
        SUBSCRIBE_TO_TASK,
        CREATE_TASK_PUSH_NOTIFICATION_CONFIG,
        GET_TASK_PUSH_NOTIFICATION_CONFIG,
        LIST_TASK_PUSH_NOTIFICATION_CONFIGS,
        DELETE_TASK_PUSH_NOTIFICATION_CONFIG,
        GET_EXTENDED_AGENT_CARD,
    ];
}

/// HTTP route templates — exact proto `google.api.http` annotation values.
///
/// These are the normative path templates from `a2a.proto` lines 21-139.
/// The `{id=*}` and `{task_id=*}` patterns are proto resource-name wildcards
/// that match any single path segment in practice.
pub mod http {
    // Message operations (proto lines 23, 35)
    /// HTTP+JSON route template for `SendMessage`.
    pub const SEND_MESSAGE: &str = "/message:send";
    /// HTTP+JSON route template for `SendStreamingMessage`.
    pub const SEND_STREAMING_MESSAGE: &str = "/message:stream";

    // Task operations (proto lines 47, 57, 66, 78)
    /// HTTP+JSON route template for `GetTask`.
    pub const GET_TASK: &str = "/tasks/{id=*}";
    /// HTTP+JSON route template for `ListTasks`.
    pub const LIST_TASKS: &str = "/tasks";
    /// HTTP+JSON route template for `CancelTask`.
    pub const CANCEL_TASK: &str = "/tasks/{id=*}:cancel";
    /// HTTP+JSON route template for `SubscribeToTask`.
    pub const SUBSCRIBE_TO_TASK: &str = "/tasks/{id=*}:subscribe";

    // Push notification config operations (proto lines 92, 104, 114, 133)
    /// HTTP+JSON route template for `CreateTaskPushNotificationConfig`.
    pub const CREATE_PUSH_CONFIG: &str = "/tasks/{task_id=*}/pushNotificationConfigs";
    /// HTTP+JSON route template for `GetTaskPushNotificationConfig`.
    pub const GET_PUSH_CONFIG: &str = "/tasks/{task_id=*}/pushNotificationConfigs/{id=*}";
    /// HTTP+JSON route template for `ListTaskPushNotificationConfigs`.
    pub const LIST_PUSH_CONFIGS: &str = "/tasks/{task_id=*}/pushNotificationConfigs";
    /// HTTP+JSON route template for `DeleteTaskPushNotificationConfig`.
    pub const DELETE_PUSH_CONFIG: &str = "/tasks/{task_id=*}/pushNotificationConfigs/{id=*}";

    // Agent card (proto line 124; well-known from discovery docs)
    /// HTTP+JSON route template for `GetExtendedAgentCard`.
    pub const EXTENDED_AGENT_CARD: &str = "/extendedAgentCard";
    /// Well-known path where the public agent card is served.
    pub const WELL_KNOWN_AGENT_CARD: &str = "/.well-known/agent-card.json";

    // Tenant-prefixed variants (proto additional_bindings)
    /// Tenant-prefixed HTTP+JSON route template for `SendMessage`.
    pub const TENANT_SEND_MESSAGE: &str = "/{tenant}/message:send";
    /// Tenant-prefixed HTTP+JSON route template for `SendStreamingMessage`.
    pub const TENANT_SEND_STREAMING_MESSAGE: &str = "/{tenant}/message:stream";
    /// Tenant-prefixed HTTP+JSON route template for `GetTask`.
    pub const TENANT_GET_TASK: &str = "/{tenant}/tasks/{id=*}";
    /// Tenant-prefixed HTTP+JSON route template for `ListTasks`.
    pub const TENANT_LIST_TASKS: &str = "/{tenant}/tasks";
    /// Tenant-prefixed HTTP+JSON route template for `CancelTask`.
    pub const TENANT_CANCEL_TASK: &str = "/{tenant}/tasks/{id=*}:cancel";
    /// Tenant-prefixed HTTP+JSON route template for `SubscribeToTask`.
    pub const TENANT_SUBSCRIBE_TO_TASK: &str = "/{tenant}/tasks/{id=*}:subscribe";
    /// Tenant-prefixed HTTP+JSON route template for `CreateTaskPushNotificationConfig`.
    pub const TENANT_CREATE_PUSH_CONFIG: &str =
        "/{tenant}/tasks/{task_id=*}/pushNotificationConfigs";
    /// Tenant-prefixed HTTP+JSON route template for `GetTaskPushNotificationConfig`.
    pub const TENANT_GET_PUSH_CONFIG: &str =
        "/{tenant}/tasks/{task_id=*}/pushNotificationConfigs/{id=*}";
    /// Tenant-prefixed HTTP+JSON route template for `ListTaskPushNotificationConfigs`.
    pub const TENANT_LIST_PUSH_CONFIGS: &str =
        "/{tenant}/tasks/{task_id=*}/pushNotificationConfigs";
    /// Tenant-prefixed HTTP+JSON route template for `DeleteTaskPushNotificationConfig`.
    pub const TENANT_DELETE_PUSH_CONFIG: &str =
        "/{tenant}/tasks/{task_id=*}/pushNotificationConfigs/{id=*}";
    /// Tenant-prefixed HTTP+JSON route template for `GetExtendedAgentCard`.
    pub const TENANT_EXTENDED_AGENT_CARD: &str = "/{tenant}/extendedAgentCard";
}

/// A2A error codes and their mappings (spec Section 5.4)
pub mod errors {
    /// ErrorInfo domain for all A2A errors.
    pub const ERROR_DOMAIN: &str = "a2a-protocol.org";

    /// ErrorInfo @type for google.rpc.ErrorInfo.
    pub const ERROR_INFO_TYPE: &str = "type.googleapis.com/google.rpc.ErrorInfo";

    /// A2A error type names (used in JSON-RPC, HTTP, gRPC error responses).
    pub const TASK_NOT_FOUND: &str = "TaskNotFoundError";
    /// Error type name for a cancel request on a task that cannot be canceled.
    pub const TASK_NOT_CANCELABLE: &str = "TaskNotCancelableError";
    /// Error type name for push-notification use against an agent that does not support it.
    pub const PUSH_NOTIFICATION_NOT_SUPPORTED: &str = "PushNotificationNotSupportedError";
    /// Error type name for an operation the agent does not support.
    pub const UNSUPPORTED_OPERATION: &str = "UnsupportedOperationError";
    /// Error type name for a content type the agent does not support.
    pub const CONTENT_TYPE_NOT_SUPPORTED: &str = "ContentTypeNotSupportedError";
    /// Error type name for an agent response that does not conform to the protocol.
    pub const INVALID_AGENT_RESPONSE: &str = "InvalidAgentResponseError";
    /// Error type name for an extended agent card request when none is configured.
    pub const EXTENDED_AGENT_CARD_NOT_CONFIGURED: &str = "ExtendedAgentCardNotConfiguredError";
    /// Error type name for a request missing a required extension.
    pub const EXTENSION_SUPPORT_REQUIRED: &str = "ExtensionSupportRequiredError";
    /// Error type name for an unsupported `A2A-Version`.
    pub const VERSION_NOT_SUPPORTED: &str = "VersionNotSupportedError";

    /// ErrorInfo reason strings (UPPER_SNAKE_CASE, no "Error" suffix).
    pub const REASON_TASK_NOT_FOUND: &str = "TASK_NOT_FOUND";
    /// `ErrorInfo.reason` for the task-not-cancelable error.
    pub const REASON_TASK_NOT_CANCELABLE: &str = "TASK_NOT_CANCELABLE";
    /// `ErrorInfo.reason` for the push-notification-not-supported error.
    pub const REASON_PUSH_NOTIFICATION_NOT_SUPPORTED: &str = "PUSH_NOTIFICATION_NOT_SUPPORTED";
    /// `ErrorInfo.reason` for the unsupported-operation error.
    pub const REASON_UNSUPPORTED_OPERATION: &str = "UNSUPPORTED_OPERATION";
    /// `ErrorInfo.reason` for the content-type-not-supported error.
    pub const REASON_CONTENT_TYPE_NOT_SUPPORTED: &str = "CONTENT_TYPE_NOT_SUPPORTED";
    /// `ErrorInfo.reason` for the invalid-agent-response error.
    pub const REASON_INVALID_AGENT_RESPONSE: &str = "INVALID_AGENT_RESPONSE";
    /// `ErrorInfo.reason` for the extended-agent-card-not-configured error.
    pub const REASON_EXTENDED_AGENT_CARD_NOT_CONFIGURED: &str =
        "EXTENDED_AGENT_CARD_NOT_CONFIGURED";
    /// `ErrorInfo.reason` for the extension-support-required error.
    pub const REASON_EXTENSION_SUPPORT_REQUIRED: &str = "EXTENSION_SUPPORT_REQUIRED";
    /// `ErrorInfo.reason` for the version-not-supported error.
    pub const REASON_VERSION_NOT_SUPPORTED: &str = "VERSION_NOT_SUPPORTED";

    /// JSON-RPC error codes (spec Section 5.4).
    pub const JSONRPC_TASK_NOT_FOUND: i32 = -32001;
    /// JSON-RPC error code for the task-not-cancelable error.
    pub const JSONRPC_TASK_NOT_CANCELABLE: i32 = -32002;
    /// JSON-RPC error code for the push-notification-not-supported error.
    pub const JSONRPC_PUSH_NOTIFICATION_NOT_SUPPORTED: i32 = -32003;
    /// JSON-RPC error code for the unsupported-operation error.
    pub const JSONRPC_UNSUPPORTED_OPERATION: i32 = -32004;
    /// JSON-RPC error code for the content-type-not-supported error.
    pub const JSONRPC_CONTENT_TYPE_NOT_SUPPORTED: i32 = -32005;
    /// JSON-RPC error code for the invalid-agent-response error.
    pub const JSONRPC_INVALID_AGENT_RESPONSE: i32 = -32006;
    /// JSON-RPC error code for the extended-agent-card-not-configured error.
    pub const JSONRPC_EXTENDED_AGENT_CARD_NOT_CONFIGURED: i32 = -32007;
    /// JSON-RPC error code for the extension-support-required error.
    pub const JSONRPC_EXTENSION_SUPPORT_REQUIRED: i32 = -32008;
    /// JSON-RPC error code for the version-not-supported error.
    pub const JSONRPC_VERSION_NOT_SUPPORTED: i32 = -32009;
    /// JSON-RPC 2.0 "Internal error".
    pub const JSONRPC_INTERNAL_ERROR: i32 = -32603;

    /// JSON-RPC codes for transport authentication failures. The A2A spec
    /// leaves these to the implementation ("JSON-RPC custom error"); they
    /// sit in the JSON-RPC server-error range outside the A2A error codes.
    pub const JSONRPC_UNAUTHENTICATED: i32 = -32040;
    /// JSON-RPC error code for a request the caller is authenticated for but not permitted to make.
    pub const JSONRPC_PERMISSION_DENIED: i32 = -32041;
    /// `ErrorInfo.domain` of authentication-failure details. These are not
    /// A2A protocol errors, so they do not use [`ERROR_DOMAIN`].
    pub const AUTH_ERROR_DOMAIN: &str = "turul-a2a";

    /// HTTP status codes for A2A errors (spec Section 5.4). Most A2A errors
    /// share 400; clients tell them apart by the `ErrorInfo.reason`.
    pub const HTTP_TASK_NOT_FOUND: u16 = 404;
    /// HTTP status for the task-not-cancelable error.
    pub const HTTP_TASK_NOT_CANCELABLE: u16 = 400;
    /// HTTP status for the push-notification-not-supported error.
    pub const HTTP_PUSH_NOTIFICATION_NOT_SUPPORTED: u16 = 400;
    /// HTTP status for the unsupported-operation error.
    pub const HTTP_UNSUPPORTED_OPERATION: u16 = 400;
    /// HTTP status for the content-type-not-supported error.
    pub const HTTP_CONTENT_TYPE_NOT_SUPPORTED: u16 = 400;
    /// HTTP status for the invalid-agent-response error.
    pub const HTTP_INVALID_AGENT_RESPONSE: u16 = 500;
    /// HTTP status for the extended-agent-card-not-configured error.
    pub const HTTP_EXTENDED_AGENT_CARD_NOT_CONFIGURED: u16 = 400;
    /// HTTP status for the extension-support-required error.
    pub const HTTP_EXTENSION_SUPPORT_REQUIRED: u16 = 400;
    /// HTTP status for the version-not-supported error.
    pub const HTTP_VERSION_NOT_SUPPORTED: u16 = 400;

    /// gRPC status for A2A errors (spec Section 5.4), as the
    /// `google.rpc.Code` enum name. The HTTP+JSON binding also emits this
    /// name as the AIP-193 `error.status` field.
    pub const GRPC_TASK_NOT_FOUND: &str = "NOT_FOUND";
    /// gRPC status name for the task-not-cancelable error.
    pub const GRPC_TASK_NOT_CANCELABLE: &str = "FAILED_PRECONDITION";
    /// gRPC status name for the push-notification-not-supported error.
    pub const GRPC_PUSH_NOTIFICATION_NOT_SUPPORTED: &str = "FAILED_PRECONDITION";
    /// gRPC status name for the unsupported-operation error.
    pub const GRPC_UNSUPPORTED_OPERATION: &str = "FAILED_PRECONDITION";
    /// gRPC status name for the content-type-not-supported error.
    pub const GRPC_CONTENT_TYPE_NOT_SUPPORTED: &str = "INVALID_ARGUMENT";
    /// gRPC status name for the invalid-agent-response error.
    pub const GRPC_INVALID_AGENT_RESPONSE: &str = "INTERNAL";
    /// gRPC status name for the extended-agent-card-not-configured error.
    pub const GRPC_EXTENDED_AGENT_CARD_NOT_CONFIGURED: &str = "FAILED_PRECONDITION";
    /// gRPC status name for the extension-support-required error.
    pub const GRPC_EXTENSION_SUPPORT_REQUIRED: &str = "FAILED_PRECONDITION";
    /// gRPC status name for the version-not-supported error.
    pub const GRPC_VERSION_NOT_SUPPORTED: &str = "FAILED_PRECONDITION";
}

/// Protocol version and media types (spec Sections 3.6 and 14).
pub mod protocol {
    /// Name of the `A2A-Version` service parameter. Sent as an HTTP header,
    /// gRPC metadata key (lowercased), or HTTP query parameter.
    pub const VERSION_PARAM: &str = "A2A-Version";
    /// The `Major.Minor` protocol version this implementation speaks.
    pub const VERSION: &str = "1.0";
    /// Name of the `A2A-Extensions` service parameter, in the lowercase
    /// form gRPC metadata requires (HTTP header names are
    /// case-insensitive). Carries activated extension URIs on requests and
    /// the echo on responses.
    pub const EXTENSIONS_PARAM: &str = "a2a-extensions";
    /// Registered A2A media type (spec Section 14.1).
    pub const MEDIA_TYPE_A2A_JSON: &str = "application/a2a+json";
    /// Plain JSON media type, accepted on every JSON binding.
    pub const MEDIA_TYPE_JSON: &str = "application/json";
}

/// Protocol binding identifiers (from AgentInterface.protocol_binding)
pub mod bindings {
    /// `AgentInterface.protocol_binding` value for the JSON-RPC binding.
    pub const JSONRPC: &str = "JSONRPC";
    /// `AgentInterface.protocol_binding` value for the gRPC binding.
    pub const GRPC: &str = "GRPC";
    /// `AgentInterface.protocol_binding` value for the HTTP+JSON binding.
    pub const HTTP_JSON: &str = "HTTP+JSON";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_eleven_jsonrpc_methods_defined() {
        assert_eq!(jsonrpc::ALL_METHODS.len(), 11);
    }

    #[test]
    fn jsonrpc_method_names_are_pascal_case() {
        for method in jsonrpc::ALL_METHODS {
            assert!(
                method.chars().next().unwrap().is_uppercase(),
                "JSON-RPC method {method} must be PascalCase"
            );
            assert!(
                !method.contains('_'),
                "JSON-RPC method {method} must not contain underscores"
            );
        }
    }

    #[test]
    fn http_routes_match_proto_annotations_exactly() {
        // Verified against proto/a2a.proto google.api.http annotations
        assert_eq!(http::SEND_MESSAGE, "/message:send"); // proto line 23
        assert_eq!(http::SEND_STREAMING_MESSAGE, "/message:stream"); // proto line 35
        assert_eq!(http::GET_TASK, "/tasks/{id=*}"); // proto line 47
        assert_eq!(http::LIST_TASKS, "/tasks"); // proto line 57
        assert_eq!(http::CANCEL_TASK, "/tasks/{id=*}:cancel"); // proto line 66
        assert_eq!(http::SUBSCRIBE_TO_TASK, "/tasks/{id=*}:subscribe"); // proto line 78
        assert_eq!(
            http::CREATE_PUSH_CONFIG,
            "/tasks/{task_id=*}/pushNotificationConfigs"
        ); // proto line 92
        assert_eq!(
            http::GET_PUSH_CONFIG,
            "/tasks/{task_id=*}/pushNotificationConfigs/{id=*}"
        ); // proto line 104
        assert_eq!(
            http::LIST_PUSH_CONFIGS,
            "/tasks/{task_id=*}/pushNotificationConfigs"
        ); // proto line 114
        assert_eq!(
            http::DELETE_PUSH_CONFIG,
            "/tasks/{task_id=*}/pushNotificationConfigs/{id=*}"
        ); // proto line 133
        assert_eq!(http::EXTENDED_AGENT_CARD, "/extendedAgentCard"); // proto line 124
        assert_eq!(http::WELL_KNOWN_AGENT_CARD, "/.well-known/agent-card.json"); // discovery docs
    }

    #[test]
    fn http_send_routes_are_message_not_tasks() {
        assert!(http::SEND_MESSAGE.starts_with("/message:"));
        assert!(!http::SEND_MESSAGE.starts_with("/tasks"));
    }

    #[test]
    fn http_task_routes_use_wildcard_id_syntax() {
        // Proto uses {id=*} not {id} — resource-name wildcard
        assert!(http::GET_TASK.contains("{id=*}"));
        assert!(http::CANCEL_TASK.contains("{id=*}"));
        assert!(http::SUBSCRIBE_TO_TASK.contains("{id=*}"));
    }

    #[test]
    fn http_push_config_routes_use_wildcard_syntax() {
        assert!(http::CREATE_PUSH_CONFIG.contains("{task_id=*}"));
        assert!(http::GET_PUSH_CONFIG.contains("{id=*}"));
    }

    #[test]
    fn http_tenant_prefixed_routes_exist() {
        // All proto additional_bindings (proto lines 26,38,49,59,69,80,95,106,116,126,135)
        assert_eq!(http::TENANT_SEND_MESSAGE, "/{tenant}/message:send");
        assert_eq!(
            http::TENANT_SEND_STREAMING_MESSAGE,
            "/{tenant}/message:stream"
        );
        assert_eq!(http::TENANT_GET_TASK, "/{tenant}/tasks/{id=*}");
        assert_eq!(http::TENANT_LIST_TASKS, "/{tenant}/tasks");
        assert_eq!(http::TENANT_CANCEL_TASK, "/{tenant}/tasks/{id=*}:cancel");
        assert_eq!(
            http::TENANT_SUBSCRIBE_TO_TASK,
            "/{tenant}/tasks/{id=*}:subscribe"
        );
        assert_eq!(
            http::TENANT_CREATE_PUSH_CONFIG,
            "/{tenant}/tasks/{task_id=*}/pushNotificationConfigs"
        );
        assert_eq!(
            http::TENANT_GET_PUSH_CONFIG,
            "/{tenant}/tasks/{task_id=*}/pushNotificationConfigs/{id=*}"
        );
        assert_eq!(
            http::TENANT_LIST_PUSH_CONFIGS,
            "/{tenant}/tasks/{task_id=*}/pushNotificationConfigs"
        );
        assert_eq!(
            http::TENANT_DELETE_PUSH_CONFIG,
            "/{tenant}/tasks/{task_id=*}/pushNotificationConfigs/{id=*}"
        );
        assert_eq!(
            http::TENANT_EXTENDED_AGENT_CARD,
            "/{tenant}/extendedAgentCard"
        );
    }

    #[test]
    fn error_info_identity_matches_spec() {
        assert_eq!(errors::ERROR_DOMAIN, "a2a-protocol.org");
        assert_eq!(
            errors::ERROR_INFO_TYPE,
            "type.googleapis.com/google.rpc.ErrorInfo"
        );
    }

    #[test]
    fn protocol_constants_match_spec() {
        assert_eq!(protocol::VERSION, "1.0");
        assert_eq!(protocol::VERSION_PARAM, "A2A-Version");
        assert_eq!(protocol::MEDIA_TYPE_A2A_JSON, "application/a2a+json");
    }
}
