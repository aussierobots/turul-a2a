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
            Self::Internal(_) => errors::JSONRPC_INTERNAL_ERROR,
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

    /// gRPC status per spec Section 5.4, as the `google.rpc.Code` enum name.
    /// The gRPC binding maps it to a status code; the HTTP+JSON binding
    /// emits it as the AIP-193 `error.status` field.
    pub fn grpc_status(&self) -> &'static str {
        match self {
            Self::TaskNotFound { .. } => errors::GRPC_TASK_NOT_FOUND,
            Self::TaskNotCancelable { .. } => errors::GRPC_TASK_NOT_CANCELABLE,
            Self::PushNotificationNotSupported => errors::GRPC_PUSH_NOTIFICATION_NOT_SUPPORTED,
            Self::UnsupportedOperation { .. } => errors::GRPC_UNSUPPORTED_OPERATION,
            Self::ContentTypeNotSupported { .. } => errors::GRPC_CONTENT_TYPE_NOT_SUPPORTED,
            Self::InvalidAgentResponse { .. } => errors::GRPC_INVALID_AGENT_RESPONSE,
            Self::ExtendedAgentCardNotConfigured => errors::GRPC_EXTENDED_AGENT_CARD_NOT_CONFIGURED,
            Self::ExtensionSupportRequired { .. } => errors::GRPC_EXTENSION_SUPPORT_REQUIRED,
            Self::VersionNotSupported { .. } => errors::GRPC_VERSION_NOT_SUPPORTED,
            Self::InvalidRequest { .. } => "INVALID_ARGUMENT",
            Self::Internal(_) => "INTERNAL",
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

    /// Build the HTTP error response body: the `google.rpc.Status` JSON
    /// shape of AIP-193 (`code`, `status`, `message`, `details`), as in the
    /// spec Section 11.6 example.
    pub fn to_http_error_body(&self) -> Value {
        let mut body = json!({
            "error": {
                "code": self.http_status(),
                "status": self.grpc_status(),
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
            error["data"] = json!([info]);
        }

        json!({
            "jsonrpc": "2.0",
            "id": id.cloned().unwrap_or(Value::Null),
            "error": error,
        })
    }
}

/// Map A2aError onto turul-rpc's `JsonRpcErrorObject` (code/message/data only).
/// The full JSON-RPC envelope (`jsonrpc`/`id`/`error`) is built by the
/// dispatcher or by `to_jsonrpc_error` above for non-dispatcher paths.
impl turul_rpc::r#async::ToJsonRpcError for A2aError {
    fn to_error_object(&self) -> turul_rpc::error::JsonRpcErrorObject {
        turul_rpc::error::JsonRpcErrorObject {
            code: self.jsonrpc_code() as i64,
            message: self.to_string(),
            data: self.error_info().map(|info| json!([info])),
        }
    }
}

impl From<crate::storage::A2aStorageError> for A2aError {
    fn from(err: crate::storage::A2aStorageError) -> Self {
        use crate::storage::A2aStorageError;
        match err {
            A2aStorageError::TaskNotFound(id) => A2aError::TaskNotFound { task_id: id },
            // A task already terminal (raw state-machine signal or the
            // terminal-write CAS loser) or an illegal transition cannot be
            // moved by the caller: TaskNotCancelable, naming the task.
            A2aStorageError::TerminalState { task_id, .. }
            | A2aStorageError::TerminalStateAlreadySet { task_id, .. }
            | A2aStorageError::InvalidTransition { task_id, .. } => {
                A2aError::TaskNotCancelable { task_id }
            }
            A2aStorageError::InvalidListRequest(message) => A2aError::InvalidRequest { message },
            other => A2aError::Internal(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::A2aStorageError;
    use turul_a2a_types::TaskState;

    /// One A2A error with its expected wire mapping, transcribed literally
    /// from the spec Section 5.4 table (gRPC status by `google.rpc.Code`
    /// name).
    struct Row {
        err: A2aError,
        http: u16,
        jsonrpc: i32,
        grpc: &'static str,
        reason: &'static str,
    }

    fn spec_table() -> Vec<Row> {
        vec![
            Row {
                err: A2aError::TaskNotFound {
                    task_id: "t".into(),
                },
                http: 404,
                jsonrpc: -32001,
                grpc: "NOT_FOUND",
                reason: "TASK_NOT_FOUND",
            },
            Row {
                err: A2aError::TaskNotCancelable {
                    task_id: "t".into(),
                },
                http: 400,
                jsonrpc: -32002,
                grpc: "FAILED_PRECONDITION",
                reason: "TASK_NOT_CANCELABLE",
            },
            Row {
                err: A2aError::PushNotificationNotSupported,
                http: 400,
                jsonrpc: -32003,
                grpc: "FAILED_PRECONDITION",
                reason: "PUSH_NOTIFICATION_NOT_SUPPORTED",
            },
            Row {
                err: A2aError::UnsupportedOperation {
                    message: "x".into(),
                },
                http: 400,
                jsonrpc: -32004,
                grpc: "FAILED_PRECONDITION",
                reason: "UNSUPPORTED_OPERATION",
            },
            Row {
                err: A2aError::ContentTypeNotSupported {
                    content_type: "x".into(),
                },
                http: 400,
                jsonrpc: -32005,
                grpc: "INVALID_ARGUMENT",
                reason: "CONTENT_TYPE_NOT_SUPPORTED",
            },
            Row {
                err: A2aError::InvalidAgentResponse {
                    message: "x".into(),
                },
                http: 500,
                jsonrpc: -32006,
                grpc: "INTERNAL",
                reason: "INVALID_AGENT_RESPONSE",
            },
            Row {
                err: A2aError::ExtendedAgentCardNotConfigured,
                http: 400,
                jsonrpc: -32007,
                grpc: "FAILED_PRECONDITION",
                reason: "EXTENDED_AGENT_CARD_NOT_CONFIGURED",
            },
            Row {
                err: A2aError::ExtensionSupportRequired {
                    extension: "x".into(),
                },
                http: 400,
                jsonrpc: -32008,
                grpc: "FAILED_PRECONDITION",
                reason: "EXTENSION_SUPPORT_REQUIRED",
            },
            Row {
                err: A2aError::VersionNotSupported {
                    version: "x".into(),
                },
                http: 400,
                jsonrpc: -32009,
                grpc: "FAILED_PRECONDITION",
                reason: "VERSION_NOT_SUPPORTED",
            },
        ]
    }

    #[test]
    fn every_a2a_error_maps_exactly_per_spec_table() {
        let table = spec_table();
        assert_eq!(table.len(), 9, "spec Section 5.4 defines nine A2A errors");
        for row in table {
            let err = &row.err;
            assert_eq!(err.http_status(), row.http, "HTTP status for {err}");
            assert_eq!(err.jsonrpc_code(), row.jsonrpc, "JSON-RPC code for {err}");
            assert_eq!(err.grpc_status(), row.grpc, "gRPC status for {err}");
            assert_eq!(err.error_reason(), Some(row.reason), "reason for {err}");

            let info = json!({
                "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                "reason": row.reason,
                "domain": "a2a-protocol.org",
            });

            let body = err.to_http_error_body();
            assert_eq!(body["error"]["code"], row.http, "{err}");
            assert_eq!(body["error"]["status"], row.grpc, "{err}");
            assert_eq!(body["error"]["details"], json!([info]), "{err}");

            let rpc = err.to_jsonrpc_error(Some(&json!(7)));
            assert_eq!(rpc["error"]["code"], row.jsonrpc, "{err}");
            assert_eq!(rpc["error"]["data"], json!([info]), "{err}");

            #[cfg(feature = "grpc")]
            {
                use tonic_types::StatusExt;
                let status = crate::grpc::error::a2a_to_status(row.err);
                let code = match row.grpc {
                    "NOT_FOUND" => tonic::Code::NotFound,
                    "FAILED_PRECONDITION" => tonic::Code::FailedPrecondition,
                    "INVALID_ARGUMENT" => tonic::Code::InvalidArgument,
                    "INTERNAL" => tonic::Code::Internal,
                    other => panic!("unexpected status {other}"),
                };
                assert_eq!(status.code(), code, "gRPC code for {}", row.reason);
                let info = status.get_details_error_info().expect("ErrorInfo");
                assert_eq!(info.reason, row.reason);
                assert_eq!(info.domain, "a2a-protocol.org");
            }
        }
    }

    #[test]
    fn non_a2a_errors_map_to_standard_codes_without_error_info() {
        let invalid = A2aError::InvalidRequest {
            message: "x".into(),
        };
        assert_eq!(invalid.http_status(), 400);
        assert_eq!(invalid.jsonrpc_code(), -32602);
        assert_eq!(invalid.grpc_status(), "INVALID_ARGUMENT");
        assert!(invalid.error_info().is_none());
        assert!(
            invalid.to_http_error_body()["error"]
                .get("details")
                .is_none()
        );

        let internal = A2aError::Internal("x".into());
        assert_eq!(internal.http_status(), 500);
        assert_eq!(internal.jsonrpc_code(), -32603);
        assert_eq!(internal.grpc_status(), "INTERNAL");
        assert!(internal.error_info().is_none());
    }

    #[test]
    fn http_error_body_matches_spec_example_shape() {
        // Spec Section 11.6 example: code, status, message, details.
        let body = A2aError::TaskNotFound {
            task_id: "t-123".into(),
        }
        .to_http_error_body();
        let error = body["error"].as_object().unwrap();
        let mut keys: Vec<&str> = error.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["code", "details", "message", "status"]);
        assert_eq!(body["error"]["status"], "NOT_FOUND");
        assert!(body["error"]["message"].as_str().unwrap().contains("t-123"));
    }

    #[test]
    fn jsonrpc_error_envelope_carries_request_id() {
        let resp = A2aError::TaskNotCancelable {
            task_id: "t-456".into(),
        }
        .to_jsonrpc_error(Some(&json!(42)));
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 42);
    }

    #[test]
    fn jsonrpc_error_null_id_when_none() {
        let resp = A2aError::Internal("oops".into()).to_jsonrpc_error(None);
        assert!(resp["id"].is_null());
        assert!(resp["error"].get("data").is_none());
    }

    // =========================================================
    // Storage error mapping — every arm
    // =========================================================

    fn not_cancelable_id(err: A2aError) -> String {
        match err {
            A2aError::TaskNotCancelable { task_id } => task_id,
            other => panic!("expected TaskNotCancelable, got {other:?}"),
        }
    }

    #[test]
    fn storage_task_not_found_maps_to_task_not_found() {
        let err = A2aError::from(A2aStorageError::TaskNotFound("t-1".into()));
        assert!(matches!(err, A2aError::TaskNotFound { ref task_id } if task_id == "t-1"));
    }

    #[test]
    fn storage_terminal_state_maps_to_not_cancelable_with_task_id() {
        let err = A2aError::from(A2aStorageError::TerminalState {
            task_id: "t-2".into(),
            state: TaskState::Completed,
        });
        assert_eq!(not_cancelable_id(err), "t-2");
    }

    #[test]
    fn storage_terminal_cas_loser_maps_to_not_cancelable_with_task_id() {
        let err = A2aError::from(A2aStorageError::TerminalStateAlreadySet {
            task_id: "t-3".into(),
            current_state: "TASK_STATE_CANCELED".into(),
        });
        assert_eq!(not_cancelable_id(err), "t-3");
    }

    #[test]
    fn storage_invalid_transition_maps_to_not_cancelable_with_task_id() {
        let err = A2aError::from(A2aStorageError::InvalidTransition {
            task_id: "t-4".into(),
            current: TaskState::Submitted,
            requested: TaskState::InputRequired,
        });
        assert_eq!(not_cancelable_id(err), "t-4");
    }

    #[test]
    fn other_storage_errors_map_to_internal() {
        for storage_err in [
            A2aStorageError::OwnerMismatch {
                task_id: "t".into(),
            },
            A2aStorageError::TenantMismatch {
                task_id: "t".into(),
            },
            A2aStorageError::ConcurrentModification("c".into()),
            A2aStorageError::PushConfigNotFound("p".into()),
            A2aStorageError::DatabaseError("d".into()),
            A2aStorageError::SerializationError("s".into()),
            A2aStorageError::Generic("g".into()),
        ] {
            let text = storage_err.to_string();
            match A2aError::from(storage_err) {
                A2aError::Internal(msg) => assert_eq!(msg, text),
                other => panic!("expected Internal for {text}, got {other:?}"),
            }
        }
    }
}
