//! `A2aError` → `tonic::Status` mapping with `google.rpc.ErrorInfo`.
//!
//! Normative table in ADR-014 §2.5. Every A2A error MUST carry
//! `ErrorInfo { reason, domain = "a2a-protocol.org" }` in
//! `Status.details` (non-A2A errors — `InvalidRequest`, `Internal` —
//! carry no `ErrorInfo` per ADR-004).
//!
//! Mapping rationale:
//!
//! - `FAILED_PRECONDITION` covers state-based rejections where the
//!   operation exists and is implemented but the resource's current
//!   state makes it inapplicable (`TaskNotCancelable`,
//!   `UnsupportedOperation`, `ExtensionSupportRequired`,
//!   `VersionNotSupported`).
//! - `UNIMPLEMENTED` covers capability absence — the deployment has
//!   not wired the feature at all (`PushNotificationNotSupported`,
//!   `ExtendedAgentCardNotConfigured`). Using `UNIMPLEMENTED` for
//!   state rejections would be semantically wrong.
//! - Transport auth failures are produced by the auth layer before
//!   dispatch and surface as `UNAUTHENTICATED` / `PERMISSION_DENIED`
//!   without `ErrorInfo` (ADR-007).

use tonic::{Code, Status};
use tonic_types::{ErrorDetails, StatusExt};
use turul_a2a_types::wire::errors;

use crate::error::A2aError;

/// Convert an `A2aError` into a `tonic::Status` with `ErrorInfo`.
///
/// Reason strings and domain are read from
/// `turul_a2a_types::wire::errors` constants — do not hard-code them
/// here.
pub fn a2a_to_status(err: A2aError) -> Status {
    let (code, reason) = match &err {
        A2aError::TaskNotFound { .. } => (Code::NotFound, Some(errors::REASON_TASK_NOT_FOUND)),
        A2aError::TaskNotCancelable { .. } => (
            Code::FailedPrecondition,
            Some(errors::REASON_TASK_NOT_CANCELABLE),
        ),
        A2aError::PushNotificationNotSupported => (
            Code::Unimplemented,
            Some(errors::REASON_PUSH_NOTIFICATION_NOT_SUPPORTED),
        ),
        A2aError::UnsupportedOperation { .. } => (
            Code::FailedPrecondition,
            Some(errors::REASON_UNSUPPORTED_OPERATION),
        ),
        A2aError::ContentTypeNotSupported { .. } => (
            Code::InvalidArgument,
            Some(errors::REASON_CONTENT_TYPE_NOT_SUPPORTED),
        ),
        A2aError::InvalidAgentResponse { .. } => {
            (Code::Internal, Some(errors::REASON_INVALID_AGENT_RESPONSE))
        }
        A2aError::ExtendedAgentCardNotConfigured => (
            Code::Unimplemented,
            Some(errors::REASON_EXTENDED_AGENT_CARD_NOT_CONFIGURED),
        ),
        A2aError::ExtensionSupportRequired { .. } => (
            Code::FailedPrecondition,
            Some(errors::REASON_EXTENSION_SUPPORT_REQUIRED),
        ),
        A2aError::VersionNotSupported { .. } => (
            Code::FailedPrecondition,
            Some(errors::REASON_VERSION_NOT_SUPPORTED),
        ),
        // Non-A2A errors: no ErrorInfo, no domain (ADR-004).
        A2aError::InvalidRequest { .. } => (Code::InvalidArgument, None),
        A2aError::Internal(_) => (Code::Internal, None),
    };

    let message = err.to_string();
    match reason {
        Some(r) => {
            let details = ErrorDetails::with_error_info(r, errors::ERROR_DOMAIN, []);
            Status::with_error_details(code, message, details)
        }
        None => Status::new(code, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason_of(status: &Status) -> Option<String> {
        status.get_details_error_info().map(|ei| ei.reason)
    }

    fn domain_of(status: &Status) -> Option<String> {
        status.get_details_error_info().map(|ei| ei.domain)
    }

    #[test]
    fn task_not_found_maps_to_not_found_with_reason() {
        let s = a2a_to_status(A2aError::TaskNotFound {
            task_id: "t-1".into(),
        });
        assert_eq!(s.code(), Code::NotFound);
        assert_eq!(reason_of(&s).as_deref(), Some("TASK_NOT_FOUND"));
        assert_eq!(domain_of(&s).as_deref(), Some("a2a-protocol.org"));
    }

    #[test]
    fn task_not_cancelable_is_failed_precondition() {
        let s = a2a_to_status(A2aError::TaskNotCancelable {
            task_id: "t-1".into(),
        });
        assert_eq!(s.code(), Code::FailedPrecondition);
        assert_eq!(reason_of(&s).as_deref(), Some("TASK_NOT_CANCELABLE"));
    }

    #[test]
    fn push_not_supported_is_unimplemented_not_failed_precondition() {
        // Capability-absence, NOT state-based rejection.
        let s = a2a_to_status(A2aError::PushNotificationNotSupported);
        assert_eq!(s.code(), Code::Unimplemented);
        assert_eq!(
            reason_of(&s).as_deref(),
            Some("PUSH_NOTIFICATION_NOT_SUPPORTED")
        );
    }

    #[test]
    fn unsupported_operation_is_failed_precondition_not_unimplemented() {
        // State-based rejection (terminal SubscribeToTask).
        let s = a2a_to_status(A2aError::UnsupportedOperation {
            message: "terminal".into(),
        });
        assert_eq!(s.code(), Code::FailedPrecondition);
        assert_eq!(reason_of(&s).as_deref(), Some("UNSUPPORTED_OPERATION"));
    }

    #[test]
    fn content_type_not_supported_is_invalid_argument() {
        let s = a2a_to_status(A2aError::ContentTypeNotSupported {
            content_type: "image/jpeg".into(),
        });
        assert_eq!(s.code(), Code::InvalidArgument);
        assert_eq!(reason_of(&s).as_deref(), Some("CONTENT_TYPE_NOT_SUPPORTED"));
    }

    #[test]
    fn invalid_agent_response_is_internal() {
        let s = a2a_to_status(A2aError::InvalidAgentResponse {
            message: "bad".into(),
        });
        assert_eq!(s.code(), Code::Internal);
        assert_eq!(reason_of(&s).as_deref(), Some("INVALID_AGENT_RESPONSE"));
    }

    #[test]
    fn extended_card_not_configured_is_unimplemented() {
        // Also capability-absence, not state-based.
        let s = a2a_to_status(A2aError::ExtendedAgentCardNotConfigured);
        assert_eq!(s.code(), Code::Unimplemented);
        assert_eq!(
            reason_of(&s).as_deref(),
            Some("EXTENDED_AGENT_CARD_NOT_CONFIGURED")
        );
    }

    #[test]
    fn extension_support_required_is_failed_precondition() {
        let s = a2a_to_status(A2aError::ExtensionSupportRequired {
            extension: "x.y".into(),
        });
        assert_eq!(s.code(), Code::FailedPrecondition);
        assert_eq!(reason_of(&s).as_deref(), Some("EXTENSION_SUPPORT_REQUIRED"));
    }

    #[test]
    fn version_not_supported_is_failed_precondition() {
        let s = a2a_to_status(A2aError::VersionNotSupported {
            version: "0.99".into(),
        });
        assert_eq!(s.code(), Code::FailedPrecondition);
        assert_eq!(reason_of(&s).as_deref(), Some("VERSION_NOT_SUPPORTED"));
    }

    #[test]
    fn invalid_request_has_no_error_info() {
        let s = a2a_to_status(A2aError::InvalidRequest {
            message: "bad".into(),
        });
        assert_eq!(s.code(), Code::InvalidArgument);
        assert!(s.get_details_error_info().is_none());
    }

    #[test]
    fn internal_has_no_error_info() {
        let s = a2a_to_status(A2aError::Internal("boom".into()));
        assert_eq!(s.code(), Code::Internal);
        assert!(s.get_details_error_info().is_none());
    }

    #[test]
    fn every_a2a_variant_with_reason_uses_canonical_domain() {
        let variants = [
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
        for v in variants {
            let s = a2a_to_status(v);
            assert_eq!(
                domain_of(&s).as_deref(),
                Some("a2a-protocol.org"),
                "domain drift on {:?}",
                s.code()
            );
        }
    }
}
