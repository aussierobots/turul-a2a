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
}

impl A2aClientError {
    /// Get the ErrorInfo reason if this is an A2A error.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::A2aError { reason, .. } => reason.as_deref(),
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
}
