//! Agent card HTTP responses: caching headers and media type.
//!
//! A2A v1.0.1 Section 8.6.1: agent card endpoints SHOULD send
//! `Cache-Control` with `max-age` and an `ETag` derived from the card; a
//! client revalidates with `If-None-Match`. The well-known card is
//! `application/a2a+json` or `application/json` by `Accept`, and also
//! `application/a2a-agent-card+json` (the AI Catalog agent card type) to a
//! client that names that type.

use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use turul_a2a::error::A2aError;
use turul_a2a::executor::{AgentExecutor, ExecutionContext};
use turul_a2a::server::A2aServer;
use turul_a2a::storage::InMemoryA2aStorage;
use turul_a2a_types::{Message, Task};

const CARD: &str = "/.well-known/agent-card.json";
const AGENT_CARD_TYPE: &str = "application/a2a-agent-card+json";

struct CardAgent {
    description: &'static str,
}

#[async_trait]
impl AgentExecutor for CardAgent {
    async fn execute(
        &self,
        _task: &mut Task,
        _msg: &Message,
        ctx: &ExecutionContext,
    ) -> Result<(), A2aError> {
        ctx.events.complete(None).await?;
        Ok(())
    }

    fn agent_card(&self) -> turul_a2a_proto::AgentCard {
        turul_a2a_proto::AgentCard {
            name: "card agent".into(),
            description: self.description.into(),
            version: "1.0.0".into(),
            capabilities: Some(turul_a2a_proto::AgentCapabilities {
                extended_agent_card: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn extended_agent_card(
        &self,
        _claims: Option<&serde_json::Value>,
    ) -> Option<turul_a2a_proto::AgentCard> {
        Some(self.agent_card())
    }
}

fn card_router(description: &'static str, max_age: Option<Duration>) -> axum::Router {
    let mut builder = A2aServer::builder()
        .executor(CardAgent { description })
        .storage(InMemoryA2aStorage::new());
    if let Some(max_age) = max_age {
        builder = builder.agent_card_max_age(max_age);
    }
    builder.build().expect("server build").into_router()
}

struct Reply {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: http::header::HeaderName) -> String {
        self.headers
            .get_all(name)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

async fn get(router: &axum::Router, uri: &str, headers: &[(&str, &str)]) -> Reply {
    let mut req = Request::get(uri).header("A2A-Version", "1.0");
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let resp = router
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Reply {
        status,
        headers,
        body,
    }
}

#[tokio::test]
async fn well_known_card_carries_cache_control_and_etag() {
    let router = card_router("one", None);
    let reply = get(&router, CARD, &[]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header(http::header::CACHE_CONTROL), "max-age=300");
    let etag = reply.header(http::header::ETAG);
    assert!(
        etag.starts_with("W/\"") && etag.ends_with('"') && etag.len() > 5,
        "{etag}"
    );
    let card: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(card["name"], "card agent");

    let again = get(&router, CARD, &[]).await;
    assert_eq!(again.header(http::header::ETAG), etag, "the tag is stable");

    let changed = get(&card_router("two", None), CARD, &[]).await;
    assert_ne!(
        changed.header(http::header::ETAG),
        etag,
        "a different card has a different tag"
    );
}

#[tokio::test]
async fn matching_if_none_match_is_not_modified() {
    let router = card_router("one", None);
    let etag = get(&router, CARD, &[]).await.header(http::header::ETAG);

    for if_none_match in [
        etag.clone(),
        etag.trim_start_matches("W/").to_string(),
        format!("\"other\", {etag}"),
        "*".to_string(),
    ] {
        let reply = get(&router, CARD, &[("if-none-match", &if_none_match)]).await;
        assert_eq!(reply.status, StatusCode::NOT_MODIFIED, "{if_none_match}");
        assert!(reply.body.is_empty(), "a 304 has no body");
        assert_eq!(reply.header(http::header::ETAG), etag);
        assert_eq!(reply.header(http::header::CACHE_CONTROL), "max-age=300");
        assert!(
            reply.header(http::header::VARY).contains("Accept"),
            "a 304 carries the Vary of the 200"
        );
    }

    let reply = get(&router, CARD, &[("if-none-match", "W/\"stale\"")]).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn max_age_is_configurable() {
    let router = card_router("one", Some(Duration::from_secs(60)));
    let reply = get(&router, CARD, &[]).await;
    assert_eq!(reply.header(http::header::CACHE_CONTROL), "max-age=60");
}

#[tokio::test]
async fn well_known_card_media_type_follows_accept() {
    let router = card_router("one", None);
    for (accept, expected) in [
        (None, "application/json"),
        (Some("application/json"), "application/json"),
        (Some("application/a2a+json"), "application/a2a+json"),
        (Some(AGENT_CARD_TYPE), AGENT_CARD_TYPE),
        (
            Some("application/a2a+json, application/a2a-agent-card+json"),
            AGENT_CARD_TYPE,
        ),
        (
            Some("application/a2a-agent-card+json;q=0, application/json"),
            "application/json",
        ),
        (Some("*/*"), "application/json"),
    ] {
        let headers: Vec<(&str, &str)> = accept.iter().map(|a| ("accept", *a)).collect();
        let reply = get(&router, CARD, &headers).await;
        assert_eq!(reply.status, StatusCode::OK, "{accept:?}");
        assert_eq!(
            reply.header(http::header::CONTENT_TYPE),
            expected,
            "{accept:?}"
        );
        assert!(reply.header(http::header::VARY).contains("Accept"));
    }
}

#[tokio::test]
async fn extended_card_is_private_and_keeps_the_a2a_types() {
    let router = card_router("one", None);
    let reply = get(
        &router,
        "/extendedAgentCard",
        &[("accept", AGENT_CARD_TYPE)],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header(http::header::CONTENT_TYPE),
        "application/json",
        "the agent card type applies to the well-known card only"
    );
    assert_eq!(
        reply.header(http::header::CACHE_CONTROL),
        "private, max-age=300"
    );
    let etag = reply.header(http::header::ETAG);
    let reply = get(&router, "/extendedAgentCard", &[("if-none-match", &etag)]).await;
    assert_eq!(reply.status, StatusCode::NOT_MODIFIED);
    assert!(reply.body.is_empty());
    assert!(reply.header(http::header::VARY).contains("Accept"));
}

#[tokio::test]
async fn an_error_on_the_card_url_never_takes_the_card_type() {
    let router = card_router("one", None);
    let req = Request::post(CARD)
        .header("A2A-Version", "1.0")
        .header("accept", AGENT_CARD_TYPE)
        .header("content-type", "text/plain")
        .body(Body::from("x"))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.headers()[http::header::CONTENT_TYPE],
        "application/json",
        "an error body is not an agent card"
    );
}
