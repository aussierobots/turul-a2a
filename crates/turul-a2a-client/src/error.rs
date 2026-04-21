//! Client error types.

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum A2aClientError {
    /// HTTP-level error with status and body.
    #[error("HTTP {status}: {message}")]
    Http { status: u16, message: String },

    /// A2A-specific error with ErrorInfo reason.
    #[error("A2A error {status}: {message}")]
    A2aError {
        status: u16,
        message: String,
        reason: Option<String>,
    },

    /// Request/transport error.
    #[error("Request error: {0}")]
    Request(#[from] reqwest::Error),

    /// JSON parsing error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Proto-to-wrapper type conversion error.
    #[error("Type conversion error: {0}")]
    Conversion(String),

    /// SSE stream error.
    #[error("SSE error: {0}")]
    Sse(String),

    /// SSE stream closed unexpectedly.
    #[error("SSE stream closed")]
    StreamClosed,

    /// gRPC transport error surfaced as a `tonic::Status`. The error
    /// carries the full status — call `reason()` to retrieve the
    /// `ErrorInfo.reason` set by the server per ADR-014 §2.5 (or
    /// `None` if this is a non-A2A error).
    #[cfg(feature = "grpc")]
    #[error("gRPC error {0:?}: {1}")]
    Grpc(tonic::Code, String, Option<String>),

    /// gRPC transport failure before a response arrived (connection
    /// refused, TLS failure, etc.).
    #[cfg(feature = "grpc")]
    #[error("gRPC transport error: {0}")]
    GrpcTransport(String),
}

impl A2aClientError {
    /// Get the ErrorInfo reason if this is an A2A error.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::A2aError { reason, .. } => reason.as_deref(),
            #[cfg(feature = "grpc")]
            Self::Grpc(_, _, reason) => reason.as_deref(),
            _ => None,
        }
    }

    /// Get the HTTP status code if available.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } | Self::A2aError { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Return the gRPC status code when this is a `Grpc` variant.
    #[cfg(feature = "grpc")]
    pub fn grpc_code(&self) -> Option<tonic::Code> {
        match self {
            Self::Grpc(code, _, _) => Some(*code),
            _ => None,
        }
    }
}

#[cfg(feature = "grpc")]
impl From<tonic::Status> for A2aClientError {
    fn from(status: tonic::Status) -> Self {
        // Pull the ErrorInfo reason the server attached per ADR-014 §2.5.
        // Uses tonic_types::StatusExt which is re-exported through tonic.
        let reason = {
            use tonic_types::StatusExt;
            status.get_details_error_info().map(|info| info.reason)
        };
        A2aClientError::Grpc(status.code(), status.message().to_string(), reason)
    }
}

#[cfg(feature = "grpc")]
impl From<tonic::transport::Error> for A2aClientError {
    fn from(err: tonic::transport::Error) -> Self {
        A2aClientError::GrpcTransport(err.to_string())
    }
}
