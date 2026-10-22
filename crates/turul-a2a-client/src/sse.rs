//! SSE (Server-Sent Events) stream parser for A2A streaming responses.
//!
//! Parses the bytes of `reqwest::Response::bytes_stream()` into
//! `SseEvent` values following the W3C Server-Sent Events event-stream
//! format: CRLF, LF and CR line endings, multi-line `data:` fields
//! joined with `\n`, comment lines, and one optional space after the
//! field colon.

use futures::stream::{Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};

use turul_a2a_types::{Message, Task};

use crate::A2aClientError;

/// A parsed SSE event from the server.
#[derive(Debug, Clone)]
pub struct SseEvent {
    /// SSE event ID (from an `id:` line), when the server sent one.
    pub id: Option<String>,
    /// Parsed JSON data (from the event's `data:` lines).
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
        Self {
            inner: Box::pin(sse_events(response.bytes_stream())),
        }
    }
}

/// Decode a byte stream into SSE events. A body chunk may end anywhere —
/// inside a line, between the CR and LF of a line ending, or inside a
/// multi-byte UTF-8 character. An event whose data is not JSON is an
/// error item; the stream continues after it.
fn sse_events<S, B>(bytes: S) -> impl Stream<Item = Result<SseEvent, A2aClientError>> + Send
where
    S: Stream<Item = Result<B, reqwest::Error>> + Send + Unpin,
    B: AsRef<[u8]>,
{
    futures::stream::unfold(
        (bytes, SseDecoder::default(), false),
        |(mut bytes, mut decoder, mut ended)| async move {
            loop {
                if let Some(block) = decoder.next_block() {
                    match parse_sse_event(&block) {
                        Ok(Some(event)) => return Some((Ok(event), (bytes, decoder, ended))),
                        // Comment-only block (keep-alive) or no data.
                        Ok(None) => continue,
                        Err(e) => return Some((Err(e), (bytes, decoder, ended))),
                    }
                }
                if ended {
                    return None;
                }
                match bytes.next().await {
                    Some(Ok(chunk)) => decoder.push(chunk.as_ref()),
                    Some(Err(e)) => {
                        return Some((Err(A2aClientError::Request(e)), (bytes, decoder, ended)));
                    }
                    None => {
                        // An event the server did not terminate with a
                        // blank line is still delivered.
                        decoder.finish();
                        ended = true;
                    }
                }
            }
        },
    )
}

/// Splits an SSE byte stream into event blocks.
///
/// Line endings are normalised to LF as bytes arrive: CRLF, LF and a
/// lone CR each end a line, and a CRLF split across two chunks still
/// counts once. Blocks are split on bytes and decoded only when
/// complete, so a UTF-8 character split across chunks survives.
#[derive(Default)]
struct SseDecoder {
    buf: Vec<u8>,
    after_cr: bool,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) {
        for &b in chunk {
            let after_cr = std::mem::take(&mut self.after_cr);
            match b {
                b'\n' if after_cr => {}
                b'\r' => {
                    self.buf.push(b'\n');
                    self.after_cr = true;
                }
                _ => self.buf.push(b),
            }
        }
    }

    /// The next complete event block, without its terminating blank line.
    fn next_block(&mut self) -> Option<String> {
        let end = self.buf.windows(2).position(|w| w == b"\n\n")?;
        let block: Vec<u8> = self.buf.drain(..end + 2).take(end).collect();
        Some(String::from_utf8_lossy(&block).into_owned())
    }

    /// The byte stream ended: terminate whatever is buffered so
    /// `next_block` returns it.
    fn finish(&mut self) {
        if !self.buf.is_empty() {
            self.buf.extend_from_slice(b"\n\n");
        }
    }
}

impl Stream for SseStream {
    type Item = Result<SseEvent, A2aClientError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

/// Parse one LF-separated SSE event block into an `SseEvent`.
///
/// ```text
/// id: 7
/// data: {"statusUpdate":
/// data:  {"taskId":"task-123", ...}}
/// ```
///
/// Each line is `field: value` (one leading space of the value is
/// dropped) or a `:` comment; `data` lines are joined with `\n`. A block
/// without data (a keep-alive comment) is `Ok(None)`; data that is not
/// JSON is an error.
fn parse_sse_event(block: &str) -> Result<Option<SseEvent>, A2aClientError> {
    let mut id = None;
    let mut data: Option<String> = None;

    for line in block.split('\n') {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => match &mut data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(value);
                }
                None => data = Some(value.to_string()),
            },
            "id" => id = Some(value.to_string()),
            _ => {}
        }
    }

    match data {
        Some(data) if !data.is_empty() => Ok(Some(SseEvent {
            id,
            data: serde_json::from_str(&data)?,
        })),
        _ => Ok(None),
    }
}

// =========================================================
// Typed streaming events
// =========================================================

/// A typed streaming event from the server.
///
/// Replaces raw `serde_json::Value` navigation with concrete variants
/// matching the proto `StreamResponse` oneof.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum StreamEvent {
    /// Full task snapshot (first event in both send and subscribe streams).
    Task(Task),
    /// Direct message from the agent.
    Message(Message),
    /// Task status changed.
    StatusUpdate {
        task_id: String,
        context_id: String,
        status: serde_json::Value,
    },
    /// Artifact produced or updated.
    ArtifactUpdate {
        task_id: String,
        context_id: String,
        artifact: serde_json::Value,
        append: bool,
        last_chunk: bool,
    },
}

/// A typed SSE event with parsed event data.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TypedSseEvent {
    /// SSE event ID (from an `id:` line), when the server sent one.
    pub id: Option<String>,
    /// Parsed and typed event data.
    pub event: StreamEvent,
}

/// Parse a raw `SseEvent` into a typed `TypedSseEvent`.
///
/// Inspects the JSON keys to determine the variant:
/// `task` → Task, `message` → Message, `statusUpdate` → StatusUpdate, `artifactUpdate` → ArtifactUpdate
fn parse_stream_event(raw: &SseEvent) -> Result<TypedSseEvent, A2aClientError> {
    let data = &raw.data;

    let event = if let Some(task_json) = data.get("task") {
        let proto: turul_a2a_proto::Task = serde_json::from_value(task_json.clone())
            .map_err(|e| A2aClientError::Conversion(format!("Invalid Task: {e}")))?;
        let task = Task::try_from(proto).map_err(|e| A2aClientError::Conversion(e.to_string()))?;
        StreamEvent::Task(task)
    } else if let Some(msg_json) = data.get("message") {
        let proto: turul_a2a_proto::Message = serde_json::from_value(msg_json.clone())
            .map_err(|e| A2aClientError::Conversion(format!("Invalid Message: {e}")))?;
        let msg =
            Message::try_from(proto).map_err(|e| A2aClientError::Conversion(e.to_string()))?;
        StreamEvent::Message(msg)
    } else if let Some(su) = data.get("statusUpdate") {
        StreamEvent::StatusUpdate {
            task_id: su
                .get("taskId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            context_id: su
                .get("contextId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            status: su.get("status").cloned().unwrap_or_default(),
        }
    } else if let Some(au) = data.get("artifactUpdate") {
        StreamEvent::ArtifactUpdate {
            task_id: au
                .get("taskId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            context_id: au
                .get("contextId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            artifact: au.get("artifact").cloned().unwrap_or_default(),
            append: au.get("append").and_then(|v| v.as_bool()).unwrap_or(false),
            last_chunk: au
                .get("lastChunk")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }
    } else {
        return Err(A2aClientError::Conversion(format!(
            "Unknown stream event shape: {data}"
        )));
    };

    Ok(TypedSseEvent {
        id: raw.id.clone(),
        event,
    })
}

/// Async stream of typed SSE events.
///
/// Wraps `SseStream` and parses raw JSON events into typed `StreamEvent` variants.
pub struct TypedSseStream {
    inner: Pin<Box<dyn Stream<Item = Result<TypedSseEvent, A2aClientError>> + Send>>,
}

impl TypedSseStream {
    pub(crate) fn from_raw(raw: SseStream) -> Self {
        let typed = futures::stream::unfold(raw, |mut raw_stream| async move {
            match raw_stream.next().await? {
                Ok(raw_event) => {
                    let typed = parse_stream_event(&raw_event);
                    Some((typed, raw_stream))
                }
                Err(e) => Some((Err(e), raw_stream)),
            }
        });
        Self {
            inner: Box::pin(typed),
        }
    }
}

impl Stream for TypedSseStream {
    type Item = Result<TypedSseEvent, A2aClientError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    #[test]
    fn parse_status_update_event() {
        let text = "id: task-1:1\ndata: {\"statusUpdate\":{\"taskId\":\"task-1\"}}";
        let event = parse_sse_event(text).unwrap().unwrap();
        assert_eq!(event.id.as_deref(), Some("task-1:1"));
        assert!(event.data.get("statusUpdate").is_some());
    }

    #[test]
    fn parse_event_without_id() {
        let text = "data: {\"task\":{\"id\":\"t-1\"}}";
        let event = parse_sse_event(text).unwrap().unwrap();
        assert!(event.id.is_none());
        assert!(event.data.get("task").is_some());
    }

    #[test]
    fn parse_comment_only_returns_none() {
        assert!(parse_sse_event(": keepalive").unwrap().is_none());
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_sse_event("").unwrap().is_none());
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        let text = "data: {\"task\":\ndata: {\"id\":\"t-1\"}}";
        let event = parse_sse_event(text).unwrap().unwrap();
        assert_eq!(event.data, json!({"task": {"id": "t-1"}}));
    }

    #[test]
    fn only_one_space_after_the_colon_is_dropped_and_no_space_is_fine() {
        let event = parse_sse_event("data:{\"a\":1}\nid:7").unwrap().unwrap();
        assert_eq!(event.data, json!({"a": 1}));
        assert_eq!(event.id.as_deref(), Some("7"));
        let event = parse_sse_event("data:  \"x\"").unwrap().unwrap();
        assert_eq!(event.data, json!("x"));
    }

    #[test]
    fn invalid_json_data_is_an_error() {
        let err = parse_sse_event("data: {not json").unwrap_err();
        assert!(matches!(err, A2aClientError::Json(_)), "got {err:?}");
    }

    /// Run `chunks` through the byte-stream decoder.
    async fn decode(chunks: &[&[u8]]) -> Vec<Result<SseEvent, A2aClientError>> {
        let bytes = futures::stream::iter(
            chunks
                .iter()
                .map(|c| Ok::<_, reqwest::Error>(c.to_vec()))
                .collect::<Vec<_>>(),
        );
        sse_events(bytes).collect().await
    }

    fn data(events: Vec<Result<SseEvent, A2aClientError>>) -> Vec<serde_json::Value> {
        events.into_iter().map(|e| e.unwrap().data).collect()
    }

    #[tokio::test]
    async fn crlf_line_endings_separate_events() {
        let events = decode(&[b"data: 1\r\n\r\ndata: 2\r\n\r\n"]).await;
        assert_eq!(data(events), [json!(1), json!(2)]);
    }

    #[tokio::test]
    async fn crlf_split_across_chunks_counts_once() {
        let events = decode(&[b"data: 1\r", b"\n\r", b"\ndata: 2\r\n", b"\r\n"]).await;
        assert_eq!(data(events), [json!(1), json!(2)]);
    }

    #[tokio::test]
    async fn lone_cr_line_endings_separate_events() {
        let events = decode(&[b"data: 1\r\rdata: 2\r\r"]).await;
        assert_eq!(data(events), [json!(1), json!(2)]);
    }

    #[tokio::test]
    async fn multi_line_data_across_chunks_with_crlf() {
        let events = decode(&[b"data: {\"a\":\r\nda", b"ta: 1}\r\n\r\n"]).await;
        assert_eq!(data(events), [json!({"a": 1})]);
    }

    #[tokio::test]
    async fn utf8_character_split_across_chunks_survives() {
        let text = "data: \"caf\u{e9}\"\n\n".as_bytes();
        let split = text.len() - 4; // inside the two-byte \u{e9}
        let events = decode(&[&text[..split], &text[split..]]).await;
        assert_eq!(data(events), [json!("caf\u{e9}")]);
    }

    #[tokio::test]
    async fn keep_alive_comments_are_skipped() {
        let events = decode(&[b":\n\ndata: 1\n\n: ping\n\n"]).await;
        assert_eq!(data(events), [json!(1)]);
    }

    #[tokio::test]
    async fn invalid_json_is_an_error_item_and_the_stream_continues() {
        let events = decode(&[b"data: nope\n\ndata: 2\n\n"]).await;
        assert!(matches!(events[0], Err(A2aClientError::Json(_))));
        assert_eq!(events[1].as_ref().unwrap().data, json!(2));
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn unterminated_final_event_is_delivered_at_end_of_stream() {
        let events = decode(&[b"data: 1\n\ndata: 2"]).await;
        assert_eq!(data(events), [json!(1), json!(2)]);
    }

    fn typed(data: serde_json::Value) -> Result<TypedSseEvent, A2aClientError> {
        parse_stream_event(&SseEvent { id: None, data })
    }

    #[test]
    fn stream_event_task() {
        let event = typed(json!({"task": {
            "id": "t-1",
            "contextId": "c-1",
            "status": {"state": "TASK_STATE_WORKING"}
        }}))
        .unwrap();
        match event.event {
            StreamEvent::Task(task) => assert_eq!(task.id(), "t-1"),
            other => panic!("expected Task, got {other:?}"),
        }
    }

    #[test]
    fn stream_event_message() {
        let event = typed(json!({"message": {
            "messageId": "m-1",
            "role": "ROLE_AGENT",
            "parts": [{"text": "hi"}]
        }}))
        .unwrap();
        match event.event {
            StreamEvent::Message(msg) => assert_eq!(msg.joined_text(), "hi"),
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn stream_event_status_update() {
        let event = typed(json!({"statusUpdate": {
            "taskId": "t-1",
            "contextId": "c-1",
            "status": {"state": "TASK_STATE_COMPLETED"}
        }}))
        .unwrap();
        match event.event {
            StreamEvent::StatusUpdate {
                task_id,
                context_id,
                status,
            } => {
                assert_eq!(task_id, "t-1");
                assert_eq!(context_id, "c-1");
                assert_eq!(status["state"], "TASK_STATE_COMPLETED");
            }
            other => panic!("expected StatusUpdate, got {other:?}"),
        }
    }

    #[test]
    fn stream_event_artifact_update_keeps_append_and_last_chunk() {
        let event = typed(json!({"artifactUpdate": {
            "taskId": "t-1",
            "contextId": "c-1",
            "artifact": {"artifactId": "a-1", "parts": [{"text": "x"}]},
            "append": true,
            "lastChunk": true
        }}))
        .unwrap();
        match event.event {
            StreamEvent::ArtifactUpdate {
                artifact,
                append,
                last_chunk,
                ..
            } => {
                assert_eq!(artifact["artifactId"], "a-1");
                assert!(append);
                assert!(last_chunk);
            }
            other => panic!("expected ArtifactUpdate, got {other:?}"),
        }

        // proto3 JSON omits false booleans: absent means false.
        let event = typed(json!({"artifactUpdate": {
            "taskId": "t-1",
            "contextId": "c-1",
            "artifact": {"artifactId": "a-1", "parts": []}
        }}))
        .unwrap();
        assert!(matches!(
            event.event,
            StreamEvent::ArtifactUpdate {
                append: false,
                last_chunk: false,
                ..
            }
        ));
    }

    #[test]
    fn stream_event_unknown_shape_is_an_error() {
        let err = typed(json!({"somethingElse": {}})).unwrap_err();
        assert!(matches!(err, A2aClientError::Conversion(_)), "got {err:?}");
    }
}
