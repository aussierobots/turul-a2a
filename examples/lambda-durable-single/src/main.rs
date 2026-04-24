//! ADR-018 single-Lambda demo — one function handles both HTTP and SQS.
//!
//! This is the simplest possible ADR-018 end-to-end demo. One Lambda
//! function, deployed with `ReservedConcurrency=1`, receives **both**:
//!
//! - HTTP events (from a Function URL) — `POST /message:send` with
//!   `configuration.returnImmediately = true`. The agent path
//!   enqueues a `QueuedExecutorJob` on SQS and returns `200 WORKING`.
//! - SQS trigger events — the event source mapping re-invokes the same
//!   function (in the same warm container thanks to the reserved
//!   concurrency) with the enqueued record. The worker path loads the
//!   task from the container's in-memory state, runs the executor,
//!   commits the terminal.
//!
//! Because `ReservedConcurrency=1` pins a single container,
//! `InMemoryA2aStorage` works — the HTTP and SQS invocations share
//! process memory. This is NOT suitable for production (one container
//! means no horizontal scale) but is the tightest self-contained
//! demo of the ADR-018 pattern.
//!
//! For a production-shaped example with shared DynamoDB storage and
//! no concurrency limit, see `examples/lambda-durable-agent` +
//! `examples/lambda-durable-worker`.
//!
//! ## Why this is NOT idiomatic `lambda_http::run`
//!
//! The dual-event shape means we can't use `lambda_http::run`
//! directly (it only knows HTTP). Instead we drop to
//! `lambda_runtime::run` + generic `serde_json::Value`, classify the
//! event shape, and for HTTP events deserialize the request via
//! `lambda_http::request::LambdaRequest` (which has a custom
//! `Deserialize` covering all API Gateway / Function URL / ALB
//! shapes) and serialize the response as an API Gateway v2 payload.
//! ~30 lines of envelope handling that `lambda_http::run` normally
//! does for you.

use std::sync::Arc;

use async_trait::async_trait;
use aws_lambda_events::apigw::ApiGatewayV2httpResponse;
use aws_lambda_events::event::sqs::SqsEvent;
use base64::Engine;
use http_body_util::BodyExt;
use lambda_runtime::{Error, LambdaEvent, service_fn};
use turul_a2a::card_builder::AgentCardBuilder;
use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_aws_lambda::{LambdaA2aServerBuilder, LambdaEvent as RoutedEvent, classify_event};
use turul_a2a_types::{Message, Task};

struct DurableEchoExecutor;

#[async_trait]
impl AgentExecutor for DurableEchoExecutor {
    async fn execute(
        &self,
        task: &mut Task,
        _msg: &Message,
        _ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        task.push_text_artifact(
            "durable-echo",
            "Durable Echo",
            "Hello from the SQS-invoked executor!",
        );
        task.complete();
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        AgentCardBuilder::new("Durable Echo Agent (single Lambda)", "0.1.0")
            .description(
                "ADR-018 single-Lambda demo: one function handles both \
                 HTTP and SQS. ReservedConcurrency=1 pins a single container \
                 so in-memory storage works across the HTTP → SQS hand-off.",
            )
            .url("https://lambda.example.com", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .streaming(false)
            .build()
            .expect("single-lambda agent card should be valid")
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .without_time()
        .init();

    let queue_url = std::env::var("A2A_EXECUTOR_QUEUE_URL").map_err(|_| {
        Error::from(
            "A2A_EXECUTOR_QUEUE_URL is required — set it to the SQS queue URL \
             (default name: turul-a2a-durable-executor-demo)",
        )
    })?;

    let aws = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let sqs = Arc::new(aws_sdk_sqs::Client::new(&aws));

    let handler = LambdaA2aServerBuilder::new()
        .executor(DurableEchoExecutor)
        .storage(InMemoryA2aStorage::new())
        .with_sqs_return_immediately(queue_url, sqs)
        .build()
        .map_err(|e| Error::from(format!("builder error: {e}")))?;

    lambda_runtime::run(service_fn(move |event: LambdaEvent<serde_json::Value>| {
        let handler = handler.clone();
        async move {
            let (value, _ctx) = event.into_parts();
            match classify_event(&value) {
                RoutedEvent::Sqs => {
                    let sqs_event: SqsEvent = serde_json::from_value(value)
                        .map_err(|e| Error::from(format!("invalid SQS event: {e}")))?;
                    let resp = handler.handle_sqs(sqs_event).await;
                    serde_json::to_value(resp)
                        .map_err(|e| Error::from(format!("serialise SqsBatchResponse: {e}")))
                }
                RoutedEvent::Http => http_invoke(&handler, value).await,
                RoutedEvent::Unknown => Err(Error::from(
                    "unknown Lambda event shape — expected HTTP or SQS",
                )),
            }
        }
    }))
    .await
}

/// Convert the generic `serde_json::Value` into a typed HTTP request,
/// dispatch through the handler, and build an API Gateway v2 / Function
/// URL response JSON. This is the ~30 lines `lambda_http::run` normally
/// hides; we open-code it because we need `lambda_runtime::run` for the
/// dual-event dispatch.
async fn http_invoke(
    handler: &turul_a2a_aws_lambda::LambdaA2aHandler,
    value: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    // `LambdaRequest` has a custom `Deserialize` that attempts each of
    // API Gateway REST, API Gateway HTTP API, ALB, and Function URL
    // shapes in order (`lambda_http/src/deserializer.rs`).
    let lambda_req: lambda_http::request::LambdaRequest = serde_json::from_value(value)
        .map_err(|e| Error::from(format!("invalid HTTP event: {e}")))?;
    let req: lambda_http::Request = lambda_req.into();

    let resp = handler
        .handle(req)
        .await
        .map_err(|e| Error::from(format!("handler error: {e}")))?;

    let (parts, body) = resp.into_parts();

    // Collect the body. `lambda_http::Body` is Empty / Text / Binary.
    let bytes = body
        .collect()
        .await
        .map_err(|e| Error::from(format!("body collect: {e}")))?
        .to_bytes();

    // Decide text vs base64 by Content-Type: any utf8-decodable bytes
    // on a textual media type go as text; otherwise base64.
    let ct = parts
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_ascii_lowercase();
    let is_text = ct.starts_with("text/")
        || ct.starts_with("application/json")
        || ct.starts_with("application/xml")
        || ct.starts_with("application/javascript")
        || ct.contains("charset=");

    let (body_str, is_base64) = if bytes.is_empty() {
        (None, false)
    } else if is_text {
        match std::str::from_utf8(&bytes) {
            Ok(s) => (Some(s.to_string()), false),
            Err(_) => (
                Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
                true,
            ),
        }
    } else {
        (
            Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
            true,
        )
    };

    let mut api_resp = ApiGatewayV2httpResponse::default();
    api_resp.status_code = parts.status.as_u16() as i64;
    api_resp.headers = parts.headers;
    api_resp.body = body_str.map(aws_lambda_events::encodings::Body::Text);
    api_resp.is_base64_encoded = is_base64;

    serde_json::to_value(api_resp).map_err(|e| Error::from(format!("serialise HTTP response: {e}")))
}
