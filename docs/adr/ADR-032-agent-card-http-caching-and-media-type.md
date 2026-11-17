# ADR-032: Agent Card HTTP Caching and Media Type

- **Status:** Accepted
- **Date:** 2026-11-17
- **Related:** ADR-004 (error model and response media types), ADR-008 (Lambda adapter)

## Context

1. **Caching.** A2A v1.0.1 §8.6.1: agent card HTTP endpoints **SHOULD** send `Cache-Control` with a `max-age` suited to how often the card changes, **SHOULD** send an `ETag` derived from the card's `version` or a hash of its content, and **MAY** send `Last-Modified`; clients **SHOULD** revalidate an expired card with `If-None-Match`. The well-known card (`GET /.well-known/agent-card.json`) and the extended card (`GET /extendedAgentCard`) sent neither header, so every client re-downloaded the card on every discovery.
2. **Media type.** The card is served as `application/a2a+json` when `Accept` names it, otherwise `application/json` (the HTTP+JSON negotiation in `middleware/transport.rs`). The Linux Foundation AI Catalog lists an A2A agent card entry with type `application/a2a-agent-card+json` and says the entry's URL SHOULD be served with that type. A2A does not define that type (it registers only `application/a2a+json`, §14.1.1) and does not forbid serving another JSON type to a client that asks for it. A client asking for it got `application/json`.

## Decision

### 1. Caching headers on both card endpoints

One function, `card_response` in `router.rs`, builds both card responses:

- `ETag`: weak, `W/"<first 16 bytes of SHA-256 of the serialized card, hex>"`. A content hash rather than `version`, so a card edited without a version bump still gets a new tag. Weak because the same card is served under more than one media type.
- `Cache-Control`: `max-age=<agent_card_max_age>` on the well-known card; `private, max-age=<agent_card_max_age>` on the extended card, which depends on the caller and must not be stored by a shared cache.
- `If-None-Match` naming the tag (weak comparison, any entry of a list) or `*` → `304 Not Modified` with no body, carrying the same `ETag`, `Cache-Control` and `Vary`.
- No `Last-Modified` (MAY; the card has no modification time).

`max-age` comes from `RuntimeConfig::agent_card_max_age`, default 5 minutes, set with `A2aServerBuilder::agent_card_max_age` (on Lambda, through `runtime_config`).

### 2. `application/a2a-agent-card+json` on the well-known card

On `GET /.well-known/agent-card.json` only, a client whose `Accept` names `application/a2a-agent-card+json` (not refused with `q=0`; wildcards do not count) gets that `Content-Type`. It takes precedence over `application/a2a+json` when both are named. Every other client, and every other endpoint including the extended card, keeps the existing negotiation. The body is the same card; an error response on that URL keeps the existing negotiation. `Vary: Accept` is unchanged.

## Consequences

- Clients can cache the card and revalidate cheaply; a changed card changes the tag.
- AI Catalog readers that ask for the catalog's card type get it; nothing changes for clients that do not.
- The framework serves one media type that A2A does not define, and only on request.

## Required tests

All in `crates/turul-a2a/tests/agent_card_http_tests.rs`; 1–5 fail against 0.2.13.

1. The well-known card carries `Cache-Control: max-age=300` and a weak `ETag`; the tag is stable, and a different card has a different tag — `well_known_card_carries_cache_control_and_etag`.
2. `If-None-Match` with the tag (weak or strong form, in a list) or `*` → 304, empty body, same `ETag`, `Cache-Control`, `Vary`; a stale tag → 200 — `matching_if_none_match_is_not_modified`.
3. `agent_card_max_age` sets `max-age` — `max_age_is_configurable`.
4. Media type by `Accept`: none, `application/json` and `*/*` → `application/json`; `application/a2a+json` → itself; the card type, alone or with `application/a2a+json` → the card type; the card type with `q=0` → `application/json` — `well_known_card_media_type_follows_accept`.
5. The extended card is `private, max-age=300`, revalidates to 304 (with `Vary`), and does not take the card type — `extended_card_is_private_and_keeps_the_a2a_types`.
6. An error on the card URL (a POST with an unsupported Content-Type and `Accept` naming the card type) is `application/json` — `an_error_on_the_card_url_never_takes_the_card_type`.

## Revision log

- 2026-11-17 — Accepted.
