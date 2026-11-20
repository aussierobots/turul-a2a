# TODO

Options to review. Nothing here is committed to or scheduled.

## Built-in CORS for servers

**Option:** turul-a2a offers built-in CORS, configured on `A2aServerBuilder`, so browser callers on another origin can reach an agent without a gateway.

- **Why:** `turul-a2a-client` runs in the browser (ADR-033). The server sends no `Access-Control-*` headers and answers a preflight `OPTIONS` with 400, so a cross-origin browser call is blocked today.
- **Today's workaround:** serve the agent on the caller's origin, or put a CORS layer (for example `tower_http::cors::CorsLayer` on `into_router()`) or an API gateway in front of it.
- **To decide:**
  - Which origins, methods and headers to allow. The A2A headers include `A2A-Version`, `A2A-Extensions`, `Authorization` and `X-API-Key`.
  - Whether credentials are allowed.
  - How it applies on Lambda, where API Gateway can do CORS itself.
  - Whether a browser e2e test (headless Firefox) joins the release gate.
