//! SSE (Server-Sent Events) stream parser for A2A streaming responses.
//!
//! Parses raw SSE text from `reqwest::Response::bytes_stream()` into
//! typed `SseEvent` values with `id` and `data` fields.

use futures::stream::{Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::A2aClientError;

/// A parsed SSE event from the server.
#[derive(Debug, Clone)]
pub struct SseEvent {
    /// SSE event ID (from `id:` line). Format: `{task_id}:{sequence}`.
    pub id: Option<String>,
    /// Parsed JSON data (from `data:` line).
    pub data: serde_json::Value,
}

/// Async stream of SSE events from a server response.
///
/// Wraps a `reqwest::Response` byte stream and parses SSE format line-by-line.
pub struct SseStream {
    inner: Pin<Box<dyn Stream<Item = Result<SseEvent, A2aClientError>> + Send>>,
}

impl SseStream {
    /// Create an SSE stream from a reqwest response.
    ///
    /// The response must have `Content-Type: text/event-stream`.
    pub(crate) fn from_response(response: reqwest::Response) -> Self {
        let byte_stream = response.bytes_stream();

        let event_stream = futures::stream::unfold(
            (byte_stream, String::new()),
            |(mut stream, mut buffer)| async move {
                loop {
                    // Check if we have a complete event in the buffer
                    if let Some(pos) = buffer.find("\n\n") {
                        let event_text = buffer[..pos].to_string();
                        buffer = buffer[pos + 2..].to_string();

                        if let Some(event) = parse_sse_event(&event_text) {
                            return Some((Ok(event), (stream, buffer)));
                        }
                        // Empty event (keepalive comment) — continue
                        continue;
                    }

                    // Need more data from the stream
                    match stream.next().await {
                        Some(Ok(chunk)) => {
                            buffer.push_str(&String::from_utf8_lossy(&chunk));
                        }
                        Some(Err(e)) => {
                            return Some((
                                Err(A2aClientError::Request(e)),
                                (stream, buffer),
                            ));
                        }
                        None => {
                            // Stream ended — check if there's a final event in the buffer
                            let remaining = buffer.trim().to_string();
                            buffer.clear();
                            if !remaining.is_empty() {
                                if let Some(event) = parse_sse_event(&remaining) {
                                    return Some((Ok(event), (stream, buffer)));
                                }
                            }
                            return None; // Stream complete
                        }
                    }
                }
            },
        );

        Self {
            inner: Box::pin(event_stream),
        }
    }
}

impl Stream for SseStream {
    type Item = Result<SseEvent, A2aClientError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

/// Parse a single SSE event block into an `SseEvent`.
///
/// SSE format:
/// ```text
/// id: task-123:1
/// data: {"statusUpdate":{"taskId":"task-123",...}}
/// ```
fn parse_sse_event(text: &str) -> Option<SseEvent> {
    let mut id = None;
    let mut data = None;

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(':') {
            continue; // SSE comment (keepalive)
        }
        if let Some(value) = line.strip_prefix("id:") {
            id = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            let value = value.trim();
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(value) {
                data = Some(json);
            }
        }
    }

    data.map(|d| SseEvent { id, data: d })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_update_event() {
        let text = "id: task-1:1\ndata: {\"statusUpdate\":{\"taskId\":\"task-1\"}}";
        let event = parse_sse_event(text).unwrap();
        assert_eq!(event.id.as_deref(), Some("task-1:1"));
        assert!(event.data.get("statusUpdate").is_some());
    }

    #[test]
    fn parse_event_without_id() {
        let text = "data: {\"task\":{\"id\":\"t-1\"}}";
        let event = parse_sse_event(text).unwrap();
        assert!(event.id.is_none());
        assert!(event.data.get("task").is_some());
    }

    #[test]
    fn parse_comment_only_returns_none() {
        let text = ": keepalive";
        assert!(parse_sse_event(text).is_none());
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_sse_event("").is_none());
    }
}
