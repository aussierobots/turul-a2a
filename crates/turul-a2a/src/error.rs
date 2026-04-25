//! Server-level A2A error types with HTTP and JSON-RPC mapping.
//!
//! Each variant maps to an exact HTTP status code, JSON-RPC error code,
//! and google.rpc.ErrorInfo reason string using wire constants from
//! `turul_a2a_types::wire::errors`.

use serde_json::{Value, json};
use turul_a2a_types::wire::errors;

/// Server-level error for A2A operations.
///
/// Each variant corresponds to an A2A-specific error type from the spec
/// with exact HTTP status, JSON-RPC code, and ErrorInfo reason mappings.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum A2aError {
    #[error("Task not found: {task_id}")]
    TaskNotFound { task_id: String },

    #[error("Task not cancelable: {task_id}")]
    TaskNotCancelable { task_id: String },

    #[error("Push notifications not supported")]
    PushNotificationNotSupported,

    #[error("Unsupported operation: {message}")]
    UnsupportedOperation { message: String },

    #[error("Content type not supported: {content_type}")]
    ContentTypeNotSupported { content_type: String },

    #[error("Invalid agent response: {message}")]
    InvalidAgentResponse { message: String },

    #[error("Extended agent card not configured")]
    ExtendedAgentCardNotConfigured,

    #[error("Extension support required: {extension}")]
    ExtensionSupportRequired { extension: String },

    #[error("Version not supported: {version}")]
    VersionNotSupported { version: String },

    #[error("Invalid request: {message}")]
    InvalidRequest { message: String },

    #[error("Internal error: {0}")]
    Internal(String),
}

impl A2aError {
    /// HTTP status code per spec Section 5.4.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::TaskNotFound { .. } => errors::HTTP_TASK_NOT_FOUND,
            Self::TaskNotCancelable { .. } => errors::HTTP_TASK_NOT_CANCELABLE,
            Self::PushNotificationNotSupported => errors::HTTP_PUSH_NOTIFICATION_NOT_SUPPORTED,
            Self::UnsupportedOperation { .. } => errors::HTTP_UNSUPPORTED_OPERATION,
            Self::ContentTypeNotSupported { .. } => errors::HTTP_CONTENT_TYPE_NOT_SUPPORTED,
            Self::InvalidAgentResponse { .. } => errors::HTTP_INVALID_AGENT_RESPONSE,
            Self::ExtendedAgentCardNotConfigured => errors::HTTP_EXTENDED_AGENT_CARD_NOT_CONFIGURED,
            Self::ExtensionSupportRequired { .. } => errors::HTTP_EXTENSION_SUPPORT_REQUIRED,
            Self::VersionNotSupported { .. } => errors::HTTP_VERSION_NOT_SUPPORTED,
            Self::InvalidRequest { .. } => 400,
            Self::Internal(_) => 500,
        }
    }

    /// JSON-RPC error code per spec Section 5.4 (-32001 to -32009).
    /// Non-A2A errors use standard JSON-RPC codes.
    pub fn jsonrpc_code(&self) -> i32 {
        match self {
            Self::TaskNotFound { .. } => errors::JSONRPC_TASK_NOT_FOUND,
            Self::TaskNotCancelable { .. } => errors::JSONRPC_TASK_NOT_CANCELABLE,
            Self::PushNotificationNotSupported => errors::JSONRPC_PUSH_NOTIFICATION_NOT_SUPPORTED,
            Self::UnsupportedOperation { .. } => errors::JSONRPC_UNSUPPORTED_OPERATION,
            Self::ContentTypeNotSupported { .. } => errors::JSONRPC_CONTENT_TYPE_NOT_SUPPORTED,
            Self::InvalidAgentResponse { .. } => errors::JSONRPC_INVALID_AGENT_RESPONSE,
            Self::ExtendedAgentCardNotConfigured => {
                errors::JSONRPC_EXTENDED_AGENT_CARD_NOT_CONFIGURED
            }
            Self::ExtensionSupportRequired { .. } => errors::JSONRPC_EXTENSION_SUPPORT_REQUIRED,
            Self::VersionNotSupported { .. } => errors::JSONRPC_VERSION_NOT_SUPPORTED,
            Self::InvalidRequest { .. } => -32602, // Invalid params
            Self::Internal(_) => -32603,           // Internal error
        }
    }

    /// ErrorInfo reason string (UPPER_SNAKE_CASE, no "Error" suffix).
    /// Returns `None` for non-A2A errors (InvalidRequest, Internal).
    pub fn error_reason(&self) -> Option<&'static str> {
        match self {
            Self::TaskNotFound { .. } => Some(errors::REASON_TASK_NOT_FOUND),
            Self::TaskNotCancelable { .. } => Some(errors::REASON_TASK_NOT_CANCELABLE),
            Self::PushNotificationNotSupported => {
                Some(errors::REASON_PUSH_NOTIFICATION_NOT_SUPPORTED)
            }
            Self::UnsupportedOperation { .. } => Some(errors::REASON_UNSUPPORTED_OPERATION),
            Self::ContentTypeNotSupported { .. } => Some(errors::REASON_CONTENT_TYPE_NOT_SUPPORTED),
            Self::InvalidAgentResponse { .. } => Some(errors::REASON_INVALID_AGENT_RESPONSE),
            Self::ExtendedAgentCardNotConfigured => {
                Some(errors::REASON_EXTENDED_AGENT_CARD_NOT_CONFIGURED)
            }
            Self::ExtensionSupportRequired { .. } => {
                Some(errors::REASON_EXTENSION_SUPPORT_REQUIRED)
            }
            Self::VersionNotSupported { .. } => Some(errors::REASON_VERSION_NOT_SUPPORTED),
            _ => None,
        }
    }

    /// Build the google.rpc.ErrorInfo JSON object for this error.
    /// Returns `None` for non-A2A errors.
    pub fn error_info(&self) -> Option<Value> {
        self.error_reason().map(|reason| {
            json!({
                "@type": errors::ERROR_INFO_TYPE,
                "reason": reason,
                "domain": errors::ERROR_DOMAIN,
            })
        })
    }

    /// Build the HTTP error response body per AIP-193.
    pub fn to_http_error_body(&self) -> Value {
        let mut body = json!({
            "error": {
                "code": self.http_status(),
                "message": self.to_string(),
            }
        });

        if let Some(info) = self.error_info() {
            body["error"]["details"] = json!([info]);
        }

        body
    }

    /// Build the JSON-RPC error object.
    pub fn to_jsonrpc_error(&self, id: Option<&Value>) -> Value {
        let mut error = json!({
            "code": self.jsonrpc_code(),
            "message": self.to_string(),
        });

        if let Some(info) = self.error_info() {
            error["data"] = info;
        }

        json!({
            "jsonrpc": "2.0",
            "id": id.cloned().unwrap_or(Value::Null),
            "error": error,
        })
    }
}

impl From<crate::storage::A2aStorageError> for A2aError {
    fn from(err: crate::storage::A2aStorageError) -> Self {
        use crate::storage::A2aStorageError;
        match err {
            A2aStorageError::TaskNotFound(id) => A2aError::TaskNotFound { task_id: id },
            A2aStorageError::TerminalState(_) => A2aError::TaskNotCancelable {
                task_id: String::new(),
            },
            // CAS loser from atomic-store terminal-CAS:
            // maps to the same wire error as TerminalState — HTTP 409 /
            // JSON-RPC -32002. Carry the task_id through for better
            // diagnostics at the wire layer.
            A2aStorageError::TerminalStateAlreadySet { task_id, .. } => {
                A2aError::TaskNotCancelable { task_id }
            }
            A2aStorageError::InvalidTransition { .. } => A2aError::TaskNotCancelable {
                task_id: String::new(),
            },
            other => A2aError::Internal(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================
    // Error model tests — exact HTTP/JSON-RPC/ErrorInfo mapping
    // =========================================================

    #[test]
    fn task_not_found_maps_to_404() {
        let err = A2aError::TaskNotFound {
            task_id: "t-1".into(),
        };
        assert_eq!(err.http_status(), 404);
        assert_eq!(err.jsonrpc_code(), errors::JSONRPC_TASK_NOT_FOUND);
        assert_eq!(err.error_reason(), Some(errors::REASON_TASK_NOT_FOUND));
    }

    #[test]
    fn task_not_cancelable_maps_to_409() {
        let err = A2aError::TaskNotCancelable {
            task_id: "t-1".into(),
        };
        assert_eq!(err.http_status(), 409);
        assert_eq!(err.jsonrpc_code(), errors::JSONRPC_TASK_NOT_CANCELABLE);
        assert_eq!(err.error_reason(), Some(errors::REASON_TASK_NOT_CANCELABLE));
    }

    #[test]
    fn content_type_not_supported_maps_to_415() {
        let err = A2aError::ContentTypeNotSupported {
            content_type: "text/xml".into(),
        };
        assert_eq!(err.http_status(), 415);
    }

    #[test]
    fn invalid_agent_response_maps_to_502() {
        let err = A2aError::InvalidAgentResponse {
            message: "bad".into(),
        };
        assert_eq!(err.http_status(), 502);
    }

    #[test]
    fn push_notification_not_supported_maps_to_400() {
        let err = A2aError::PushNotificationNotSupported;
        assert_eq!(err.http_status(), 400);
        assert_eq!(
            err.jsonrpc_code(),
            errors::JSONRPC_PUSH_NOTIFICATION_NOT_SUPPORTED
        );
    }

    #[test]
    fn all_a2a_errors_have_error_info() {
        let a2a_errors: Vec<A2aError> = vec![
            A2aError::TaskNotFound {
                task_id: "t".into(),
            },
            A2aError::TaskNotCancelable {
                task_id: "t".into(),
            },
            A2aError::PushNotificationNotSupported,
            A2aError::UnsupportedOperation {
                message: "x".into(),
            },
            A2aError::ContentTypeNotSupported {
                content_type: "x".into(),
            },
            A2aError::InvalidAgentResponse {
                message: "x".into(),
            },
            A2aError::ExtendedAgentCardNotConfigured,
            A2aError::ExtensionSupportRequired {
                extension: "x".into(),
            },
            A2aError::VersionNotSupported {
                version: "x".into(),
            },
        ];

        for err in &a2a_errors {
            let info = err.error_info();
            assert!(info.is_some(), "{err} should have ErrorInfo");
            let info = info.unwrap();
            assert_eq!(
                info["@type"],
                errors::ERROR_INFO_TYPE,
                "{err} ErrorInfo @type"
            );
            assert_eq!(
                info["domain"],
                errors::ERROR_DOMAIN,
                "{err} ErrorInfo domain"
            );
            assert!(
                info["reason"].is_string(),
                "{err} ErrorInfo reason should be string"
            );
        }
    }

    #[test]
    fn non_a2a_errors_have_no_error_info() {
        assert!(
            A2aError::InvalidRequest {
                message: "x".into()
            }
            .error_info()
            .is_none()
        );
        assert!(A2aError::Internal("x".into()).error_info().is_none());
    }

    #[test]
    fn http_error_body_follows_aip193() {
        let err = A2aError::TaskNotFound {
            task_id: "t-123".into(),
        };
        let body = err.to_http_error_body();

        assert_eq!(body["error"]["code"], 404);
        assert!(body["error"]["message"].as_str().unwrap().contains("t-123"));
        let details = body["error"]["details"].as_array().unwrap();
        assert_eq!(details.len(), 1);
        assert_eq!(details[0]["@type"], errors::ERROR_INFO_TYPE);
        assert_eq!(details[0]["reason"], errors::REASON_TASK_NOT_FOUND);
        assert_eq!(details[0]["domain"], errors::ERROR_DOMAIN);
    }

    #[test]
    fn jsonrpc_error_follows_spec() {
        let err = A2aError::TaskNotCancelable {
            task_id: "t-456".into(),
        };
        let id = json!(42);
        let resp = err.to_jsonrpc_error(Some(&id));

        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 42);
        assert_eq!(resp["error"]["code"], errors::JSONRPC_TASK_NOT_CANCELABLE);
        let data = &resp["error"]["data"];
        assert!(data.is_object(), "JSON-RPC error data should be an object");
        assert_eq!(data["@type"], errors::ERROR_INFO_TYPE);
        assert_eq!(data["reason"], errors::REASON_TASK_NOT_CANCELABLE);
        assert_eq!(data["domain"], errors::ERROR_DOMAIN);
    }

    #[test]
    fn jsonrpc_error_null_id_when_none() {
        let err = A2aError::Internal("oops".into());
        let resp = err.to_jsonrpc_error(None);
        assert!(resp["id"].is_null());
        // Internal error has no ErrorInfo
        assert!(resp["error"].get("data").is_none());
    }

    #[test]
    fn all_nine_a2a_jsonrpc_codes_in_range() {
        let a2a_errors: Vec<A2aError> = vec![
            A2aError::TaskNotFound {
                task_id: "t".into(),
            },
            A2aError::TaskNotCancelable {
                task_id: "t".into(),
            },
            A2aError::PushNotificationNotSupported,
            A2aError::UnsupportedOperation {
                message: "x".into(),
            },
            A2aError::ContentTypeNotSupported {
                content_type: "x".into(),
            },
            A2aError::InvalidAgentResponse {
                message: "x".into(),
            },
            A2aError::ExtendedAgentCardNotConfigured,
            A2aError::ExtensionSupportRequired {
                extension: "x".into(),
            },
            A2aError::VersionNotSupported {
                version: "x".into(),
            },
        ];

        let codes: Vec<i32> = a2a_errors.iter().map(|e| e.jsonrpc_code()).collect();
        assert_eq!(codes.len(), 9);
        for code in &codes {
            assert!(
                (-32099..=-32001).contains(code),
                "JSON-RPC code {code} out of A2A range"
            );
        }
        // All unique
        let unique: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(unique.len(), 9, "All 9 A2A error codes must be unique");
    }
}
