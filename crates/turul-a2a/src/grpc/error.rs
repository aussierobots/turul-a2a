//! `A2aError` → `tonic::Status` mapping with `google.rpc.ErrorInfo`.
//!
//! Every A2A error MUST carry `ErrorInfo { reason, domain =
//! "a2a-protocol.org" }` in `Status.details` (non-A2A errors —
//! `InvalidRequest`, `Internal` — carry no `ErrorInfo`).
//!
//! The status code, reason and domain all come from [`A2aError`]
//! (`grpc_status`, `error_reason`), which owns the spec Section 5.4 table;
//! this module only translates the `google.rpc.Code` name into a
//! [`tonic::Code`].
//! - Transport auth failures are produced by the auth layer before
//!   dispatch and surface as `UNAUTHENTICATED` / `PERMISSION_DENIED`
//!   without `ErrorInfo`.

use tonic::{Code, Status};
use tonic_types::{ErrorDetails, StatusExt};
use turul_a2a_types::wire::errors;

use crate::error::A2aError;

/// Convert an `A2aError` into a `tonic::Status` with `ErrorInfo`.
pub fn a2a_to_status(err: A2aError) -> Status {
    let code = match err.grpc_status() {
        "NOT_FOUND" => Code::NotFound,
        "FAILED_PRECONDITION" => Code::FailedPrecondition,
        "INVALID_ARGUMENT" => Code::InvalidArgument,
        _ => Code::Internal,
    };
    let reason = err.error_reason();
    let message = err.to_string();
    match reason {
        Some(r) => {
            let details = ErrorDetails::with_error_info(r, errors::ERROR_DOMAIN, []);
            Status::with_error_details(code, message, details)
        }
        None => Status::new(code, message),
    }
}

// The per-error code/reason/domain mapping is asserted by the spec table
// test in `crate::error`, which also runs this conversion under the `grpc`
// feature.
#[cfg(test)]
mod tests {
    use super::*;

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
}
