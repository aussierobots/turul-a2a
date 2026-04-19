//! Centralized wire-format constants from the A2A v1.0 specification.
//!
//! All values derived from `a2a.proto` google.api.http annotations and
//! the spec's error code mapping table (Section 5.4).

/// JSON-RPC method names (PascalCase, from proto service definition)
pub mod jsonrpc {
    pub const SEND_MESSAGE: &str = "SendMessage";
    pub const SEND_STREAMING_MESSAGE: &str = "SendStreamingMessage";
    pub const GET_TASK: &str = "GetTask";
    pub const LIST_TASKS: &str = "ListTasks";
    pub const CANCEL_TASK: &str = "CancelTask";
    pub const SUBSCRIBE_TO_TASK: &str = "SubscribeToTask";
    pub const CREATE_TASK_PUSH_NOTIFICATION_CONFIG: &str = "CreateTaskPushNotificationConfig";
    pub const GET_TASK_PUSH_NOTIFICATION_CONFIG: &str = "GetTaskPushNotificationConfig";
    pub const LIST_TASK_PUSH_NOTIFICATION_CONFIGS: &str = "ListTaskPushNotificationConfigs";
    pub const DELETE_TASK_PUSH_NOTIFICATION_CONFIG: &str = "DeleteTaskPushNotificationConfig";
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
    pub const SEND_MESSAGE: &str = "/message:send";
    pub const SEND_STREAMING_MESSAGE: &str = "/message:stream";

    // Task operations (proto lines 47, 57, 66, 78)
    pub const GET_TASK: &str = "/tasks/{id=*}";
    pub const LIST_TASKS: &str = "/tasks";
    pub const CANCEL_TASK: &str = "/tasks/{id=*}:cancel";
    pub const SUBSCRIBE_TO_TASK: &str = "/tasks/{id=*}:subscribe";

    // Push notification config operations (proto lines 92, 104, 114, 133)
    pub const CREATE_PUSH_CONFIG: &str = "/tasks/{task_id=*}/pushNotificationConfigs";
    pub const GET_PUSH_CONFIG: &str =
        "/tasks/{task_id=*}/pushNotificationConfigs/{id=*}";
    pub const LIST_PUSH_CONFIGS: &str = "/tasks/{task_id=*}/pushNotificationConfigs";
    pub const DELETE_PUSH_CONFIG: &str =
        "/tasks/{task_id=*}/pushNotificationConfigs/{id=*}";

    // Agent card (proto line 124; well-known from discovery docs)
    pub const EXTENDED_AGENT_CARD: &str = "/extendedAgentCard";
    pub const WELL_KNOWN_AGENT_CARD: &str = "/.well-known/agent-card.json";

    // Tenant-prefixed variants (proto additional_bindings)
    pub const TENANT_SEND_MESSAGE: &str = "/{tenant}/message:send";
    pub const TENANT_SEND_STREAMING_MESSAGE: &str = "/{tenant}/message:stream";
    pub const TENANT_GET_TASK: &str = "/{tenant}/tasks/{id=*}";
    pub const TENANT_LIST_TASKS: &str = "/{tenant}/tasks";
    pub const TENANT_CANCEL_TASK: &str = "/{tenant}/tasks/{id=*}:cancel";
    pub const TENANT_SUBSCRIBE_TO_TASK: &str = "/{tenant}/tasks/{id=*}:subscribe";
    pub const TENANT_CREATE_PUSH_CONFIG: &str =
        "/{tenant}/tasks/{task_id=*}/pushNotificationConfigs";
    pub const TENANT_GET_PUSH_CONFIG: &str =
        "/{tenant}/tasks/{task_id=*}/pushNotificationConfigs/{id=*}";
    pub const TENANT_LIST_PUSH_CONFIGS: &str =
        "/{tenant}/tasks/{task_id=*}/pushNotificationConfigs";
    pub const TENANT_DELETE_PUSH_CONFIG: &str =
        "/{tenant}/tasks/{task_id=*}/pushNotificationConfigs/{id=*}";
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
    pub const TASK_NOT_CANCELABLE: &str = "TaskNotCancelableError";
    pub const PUSH_NOTIFICATION_NOT_SUPPORTED: &str = "PushNotificationNotSupportedError";
    pub const UNSUPPORTED_OPERATION: &str = "UnsupportedOperationError";
    pub const CONTENT_TYPE_NOT_SUPPORTED: &str = "ContentTypeNotSupportedError";
    pub const INVALID_AGENT_RESPONSE: &str = "InvalidAgentResponseError";
    pub const EXTENDED_AGENT_CARD_NOT_CONFIGURED: &str = "ExtendedAgentCardNotConfiguredError";
    pub const EXTENSION_SUPPORT_REQUIRED: &str = "ExtensionSupportRequiredError";
    pub const VERSION_NOT_SUPPORTED: &str = "VersionNotSupportedError";

    /// ErrorInfo reason strings (UPPER_SNAKE_CASE, no "Error" suffix).
    pub const REASON_TASK_NOT_FOUND: &str = "TASK_NOT_FOUND";
    pub const REASON_TASK_NOT_CANCELABLE: &str = "TASK_NOT_CANCELABLE";
    pub const REASON_PUSH_NOTIFICATION_NOT_SUPPORTED: &str = "PUSH_NOTIFICATION_NOT_SUPPORTED";
    pub const REASON_UNSUPPORTED_OPERATION: &str = "UNSUPPORTED_OPERATION";
    pub const REASON_CONTENT_TYPE_NOT_SUPPORTED: &str = "CONTENT_TYPE_NOT_SUPPORTED";
    pub const REASON_INVALID_AGENT_RESPONSE: &str = "INVALID_AGENT_RESPONSE";
    pub const REASON_EXTENDED_AGENT_CARD_NOT_CONFIGURED: &str =
        "EXTENDED_AGENT_CARD_NOT_CONFIGURED";
    pub const REASON_EXTENSION_SUPPORT_REQUIRED: &str = "EXTENSION_SUPPORT_REQUIRED";
    pub const REASON_VERSION_NOT_SUPPORTED: &str = "VERSION_NOT_SUPPORTED";

    /// JSON-RPC error codes (spec Section 5.4).
    pub const JSONRPC_TASK_NOT_FOUND: i32 = -32001;
    pub const JSONRPC_TASK_NOT_CANCELABLE: i32 = -32002;
    pub const JSONRPC_PUSH_NOTIFICATION_NOT_SUPPORTED: i32 = -32003;
    pub const JSONRPC_UNSUPPORTED_OPERATION: i32 = -32004;
    pub const JSONRPC_CONTENT_TYPE_NOT_SUPPORTED: i32 = -32005;
    pub const JSONRPC_INVALID_AGENT_RESPONSE: i32 = -32006;
    pub const JSONRPC_EXTENDED_AGENT_CARD_NOT_CONFIGURED: i32 = -32007;
    pub const JSONRPC_EXTENSION_SUPPORT_REQUIRED: i32 = -32008;
    pub const JSONRPC_VERSION_NOT_SUPPORTED: i32 = -32009;

    /// HTTP status codes for A2A errors (spec Section 5.4).
    pub const HTTP_TASK_NOT_FOUND: u16 = 404;
    pub const HTTP_TASK_NOT_CANCELABLE: u16 = 409;
    pub const HTTP_PUSH_NOTIFICATION_NOT_SUPPORTED: u16 = 400;
    pub const HTTP_UNSUPPORTED_OPERATION: u16 = 400;
    pub const HTTP_CONTENT_TYPE_NOT_SUPPORTED: u16 = 415;
    pub const HTTP_INVALID_AGENT_RESPONSE: u16 = 502;
    pub const HTTP_EXTENDED_AGENT_CARD_NOT_CONFIGURED: u16 = 400;
    pub const HTTP_EXTENSION_SUPPORT_REQUIRED: u16 = 400;
    pub const HTTP_VERSION_NOT_SUPPORTED: u16 = 400;
}

/// Protocol binding identifiers (from AgentInterface.protocol_binding)
pub mod bindings {
    pub const JSONRPC: &str = "JSONRPC";
    pub const GRPC: &str = "GRPC";
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
        assert_eq!(http::TENANT_SEND_STREAMING_MESSAGE, "/{tenant}/message:stream");
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
    fn error_domain_is_a2a_protocol_org() {
        assert_eq!(errors::ERROR_DOMAIN, "a2a-protocol.org");
    }

    #[test]
    fn error_info_type_is_google_rpc() {
        assert_eq!(
            errors::ERROR_INFO_TYPE,
            "type.googleapis.com/google.rpc.ErrorInfo"
        );
    }

    #[test]
    fn task_not_cancelable_is_409() {
        assert_eq!(errors::HTTP_TASK_NOT_CANCELABLE, 409);
    }

    #[test]
    fn content_type_not_supported_is_415() {
        assert_eq!(errors::HTTP_CONTENT_TYPE_NOT_SUPPORTED, 415);
    }

    #[test]
    fn invalid_agent_response_is_502() {
        assert_eq!(errors::HTTP_INVALID_AGENT_RESPONSE, 502);
    }

    #[test]
    fn jsonrpc_error_codes_are_in_range() {
        let codes = [
            errors::JSONRPC_TASK_NOT_FOUND,
            errors::JSONRPC_TASK_NOT_CANCELABLE,
            errors::JSONRPC_PUSH_NOTIFICATION_NOT_SUPPORTED,
            errors::JSONRPC_UNSUPPORTED_OPERATION,
            errors::JSONRPC_CONTENT_TYPE_NOT_SUPPORTED,
            errors::JSONRPC_INVALID_AGENT_RESPONSE,
            errors::JSONRPC_EXTENDED_AGENT_CARD_NOT_CONFIGURED,
            errors::JSONRPC_EXTENSION_SUPPORT_REQUIRED,
            errors::JSONRPC_VERSION_NOT_SUPPORTED,
        ];
        assert_eq!(codes.len(), 9);
        for code in codes {
            assert!(
                (-32099..=-32001).contains(&code),
                "JSON-RPC code {code} must be in -32001 to -32099 range"
            );
        }
    }

    #[test]
    fn error_reasons_are_upper_snake_case_without_error_suffix() {
        let reasons = [
            errors::REASON_TASK_NOT_FOUND,
            errors::REASON_TASK_NOT_CANCELABLE,
            errors::REASON_PUSH_NOTIFICATION_NOT_SUPPORTED,
            errors::REASON_UNSUPPORTED_OPERATION,
            errors::REASON_CONTENT_TYPE_NOT_SUPPORTED,
            errors::REASON_INVALID_AGENT_RESPONSE,
            errors::REASON_EXTENDED_AGENT_CARD_NOT_CONFIGURED,
            errors::REASON_EXTENSION_SUPPORT_REQUIRED,
            errors::REASON_VERSION_NOT_SUPPORTED,
        ];
        for reason in reasons {
            assert!(
                !reason.ends_with("_ERROR"),
                "Reason {reason} must not end with _ERROR"
            );
            assert_eq!(
                reason,
                reason.to_uppercase(),
                "Reason {reason} must be UPPER_SNAKE_CASE"
            );
        }
    }
}
