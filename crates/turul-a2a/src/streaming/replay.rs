//! Last-Event-ID parsing for SSE reconnection replay.
//!
//! Format: `{task_id}:{sequence}` — tenant is implicit from the request path.

/// Parsed Last-Event-ID header.
#[derive(Debug, Clone, PartialEq)]
pub struct LastEventId {
    pub task_id: String,
    pub sequence: u64,
}

/// Parse a Last-Event-ID header value.
///
/// Format: `{task_id}:{sequence}`
/// Returns `None` for invalid formats.
pub fn parse_last_event_id(header: &str) -> Option<LastEventId> {
    let header = header.trim();
    if header.is_empty() {
        return None;
    }

    // Find the LAST colon — task_id may contain colons (UUIDs don't, but be safe)
    let colon_pos = header.rfind(':')?;
    if colon_pos == 0 || colon_pos == header.len() - 1 {
        return None;
    }

    let task_id = &header[..colon_pos];
    let seq_str = &header[colon_pos + 1..];

    let sequence = seq_str.parse::<u64>().ok()?;

    if task_id.is_empty() {
        return None;
    }

    Some(LastEventId {
        task_id: task_id.to_string(),
        sequence,
    })
}

/// Format an SSE event ID from task_id and sequence.
pub fn format_event_id(task_id: &str, sequence: u64) -> String {
    format!("{task_id}:{sequence}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================
    // Valid formats
    // =========================================================

    #[test]
    fn valid_task_id_and_sequence() {
        let result = parse_last_event_id("task-123:42").unwrap();
        assert_eq!(result.task_id, "task-123");
        assert_eq!(result.sequence, 42);
    }

    #[test]
    fn valid_uuid_task_id() {
        let result =
            parse_last_event_id("019d7b4d-e360-7430-a4ba-b121d3387beb:100").unwrap();
        assert_eq!(
            result.task_id,
            "019d7b4d-e360-7430-a4ba-b121d3387beb"
        );
        assert_eq!(result.sequence, 100);
    }

    #[test]
    fn valid_zero_sequence() {
        let result = parse_last_event_id("task-1:0").unwrap();
        assert_eq!(result.sequence, 0);
    }

    #[test]
    fn valid_large_sequence() {
        let result = parse_last_event_id("task-1:18446744073709551615").unwrap();
        assert_eq!(result.sequence, u64::MAX);
    }

    #[test]
    fn whitespace_trimmed() {
        let result = parse_last_event_id("  task-1:5  ").unwrap();
        assert_eq!(result.task_id, "task-1");
        assert_eq!(result.sequence, 5);
    }

    // =========================================================
    // Invalid formats
    // =========================================================

    #[test]
    fn empty_string() {
        assert!(parse_last_event_id("").is_none());
    }

    #[test]
    fn whitespace_only() {
        assert!(parse_last_event_id("   ").is_none());
    }

    #[test]
    fn no_colon() {
        assert!(parse_last_event_id("task123").is_none());
    }

    #[test]
    fn colon_at_start() {
        assert!(parse_last_event_id(":42").is_none());
    }

    #[test]
    fn colon_at_end() {
        assert!(parse_last_event_id("task-1:").is_none());
    }

    #[test]
    fn non_numeric_sequence() {
        assert!(parse_last_event_id("task-1:abc").is_none());
    }

    #[test]
    fn negative_sequence() {
        assert!(parse_last_event_id("task-1:-1").is_none());
    }

    #[test]
    fn just_colon() {
        assert!(parse_last_event_id(":").is_none());
    }

    // =========================================================
    // Task ID mismatch (caller's responsibility, not parser's)
    // =========================================================

    #[test]
    fn parser_does_not_validate_task_id_against_request() {
        // The parser returns whatever task_id is in the header.
        // The handler checks if it matches the requested task.
        let result = parse_last_event_id("wrong-task:5").unwrap();
        assert_eq!(result.task_id, "wrong-task");
    }

    // =========================================================
    // Format round-trip
    // =========================================================

    #[test]
    fn format_and_parse_round_trip() {
        let formatted = format_event_id("my-task-uuid", 42);
        let parsed = parse_last_event_id(&formatted).unwrap();
        assert_eq!(parsed.task_id, "my-task-uuid");
        assert_eq!(parsed.sequence, 42);
    }
}
