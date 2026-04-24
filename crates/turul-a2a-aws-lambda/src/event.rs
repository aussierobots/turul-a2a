//! Event-shape classification for single-function Lambdas that
//! receive more than one trigger type.
//!
//! Lambda invokes the binary with a raw JSON event; AWS does not
//! surface the trigger type as a separate parameter. When a single
//! function is wired to multiple event sources (HTTP plus SQS, HTTP
//! plus EventBridge, HTTP plus DynamoDB streams, …) the binary has
//! to classify the event shape up front and route accordingly.
//!
//! [`classify_event`] and [`LambdaEvent`] are the framework's
//! classifier. They are available without the `sqs` feature so
//! adopters with HTTP + a non-SQS third trigger (e.g. EventBridge
//! Scheduler) can route in their own `main.rs` without re-deriving
//! the shape heuristics.

/// Coarse Lambda event-shape discriminator returned by
/// [`classify_event`].
///
/// Adopters pattern-match on this in their own
/// `lambda_runtime::run<serde_json::Value>` service function and
/// dispatch each arm. The framework's own [`LambdaA2aHandler::run`],
/// [`LambdaA2aHandler::run_http_only`],
/// [`LambdaA2aHandler::run_http_and_sqs`], and
/// [`LambdaA2aHandler::run_sqs_only`] use this internally.
///
/// [`LambdaA2aHandler::run`]: crate::LambdaA2aHandler::run
/// [`LambdaA2aHandler::run_http_only`]: crate::LambdaA2aHandler::run_http_only
/// [`LambdaA2aHandler::run_http_and_sqs`]: crate::LambdaA2aHandler::run_http_and_sqs
/// [`LambdaA2aHandler::run_sqs_only`]: crate::LambdaA2aHandler::run_sqs_only
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LambdaEvent {
    /// HTTP event (API Gateway REST, HTTP API, Function URL, ALB).
    Http,
    /// SQS trigger event from an event source mapping.
    Sqs,
    /// Unrecognised shape (DynamoDB stream, EventBridge scheduler,
    /// custom invocation payloads, etc.). Adopter-owned.
    Unknown,
}

/// Classify a raw Lambda event JSON shape.
///
/// Heuristic:
/// - SQS event JSON has a top-level `"Records"` array whose entries
///   contain `"eventSource": "aws:sqs"`.
/// - HTTP event JSON has one of `httpMethod`,
///   `requestContext.http.method`, or `routeKey`.
/// - Anything else is [`LambdaEvent::Unknown`].
///
/// Adopter usage for a composite Lambda (HTTP + SQS + EventBridge
/// nightly schedule):
///
/// ```ignore
/// lambda_runtime::run(lambda_runtime::service_fn(
///     move |event: lambda_runtime::LambdaEvent<serde_json::Value>| {
///         let handler = handler.clone();
///         async move {
///             let (value, _ctx) = event.into_parts();
///             match turul_a2a_aws_lambda::classify_event(&value) {
///                 turul_a2a_aws_lambda::LambdaEvent::Http => {
///                     handler.handle_http_event_value(value).await
///                 }
///                 turul_a2a_aws_lambda::LambdaEvent::Sqs => {
///                     let sqs = serde_json::from_value(value)?;
///                     Ok(serde_json::to_value(handler.handle_sqs(sqs).await)?)
///                 }
///                 turul_a2a_aws_lambda::LambdaEvent::Unknown => {
///                     // Adopter-owned — the framework does not know
///                     // what this event means.
///                     run_nightly_audit(/* adopter context */).await
///                 }
///             }
///         }
///     }
/// ))
/// ```
pub fn classify_event(event: &serde_json::Value) -> LambdaEvent {
    if is_sqs_event_shape(event) {
        return LambdaEvent::Sqs;
    }
    if is_http_event_shape(event) {
        return LambdaEvent::Http;
    }
    LambdaEvent::Unknown
}

fn is_sqs_event_shape(event: &serde_json::Value) -> bool {
    event
        .get("Records")
        .and_then(|r| r.as_array())
        .and_then(|arr| arr.first())
        .and_then(|rec| rec.get("eventSource"))
        .and_then(|s| s.as_str())
        .map(|s| s == "aws:sqs")
        .unwrap_or(false)
}

fn is_http_event_shape(event: &serde_json::Value) -> bool {
    if event.get("httpMethod").is_some() {
        return true;
    }
    if let Some(req_ctx) = event.get("requestContext") {
        if req_ctx.pointer("/http/method").is_some() {
            return true;
        }
    }
    if event.get("routeKey").is_some() {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_apigw_v2_http_event() {
        let v = serde_json::json!({
            "version": "2.0",
            "routeKey": "GET /.well-known/agent-card.json",
            "requestContext": {
                "http": {"method": "GET", "path": "/.well-known/agent-card.json"}
            }
        });
        assert_eq!(classify_event(&v), LambdaEvent::Http);
    }

    #[test]
    fn classify_apigw_v1_http_event() {
        let v = serde_json::json!({
            "httpMethod": "POST",
            "path": "/message:send",
            "resource": "/{proxy+}"
        });
        assert_eq!(classify_event(&v), LambdaEvent::Http);
    }

    #[test]
    fn classify_sqs_event() {
        let v = serde_json::json!({
            "Records": [
                {"eventSource": "aws:sqs", "messageId": "m1", "body": "{}"}
            ]
        });
        assert_eq!(classify_event(&v), LambdaEvent::Sqs);
    }

    #[test]
    fn classify_eventbridge_scheduled_is_unknown() {
        let v = serde_json::json!({
            "source": "aws.events",
            "detail-type": "Scheduled Event",
            "detail": {}
        });
        assert_eq!(classify_event(&v), LambdaEvent::Unknown);
    }

    #[test]
    fn classify_dynamodb_stream_is_unknown_not_sqs() {
        let v = serde_json::json!({
            "Records": [
                {"eventSource": "aws:dynamodb", "dynamodb": {}}
            ]
        });
        assert_eq!(classify_event(&v), LambdaEvent::Unknown);
    }

    #[test]
    fn classify_unknown_payload() {
        let v = serde_json::json!({"custom": "invoke"});
        assert_eq!(classify_event(&v), LambdaEvent::Unknown);
    }
}
