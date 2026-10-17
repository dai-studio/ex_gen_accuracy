# Moxy: implementation notes

This is Moxy as described in `spec.md`: a local gateway for AI model endpoints, written in Rust on Pingora, with a SvelteKit web UI embedded into the binary.

## Building and running

```sh
make build              # npm ci + vite build in ui/, then cargo build
make test               # cargo test (unit and end-to-end)
cp moxy.example.xml moxy.xml   # edit, then export the variables it references
cargo run -- serve      # or: ./target/debug/moxy
```

The built UI (`ui/build`) is checked in, so a plain `cargo build` works without Node.js. If `ui/build` is missing, `build.rs` embeds a placeholder page and prints a warning.

Pinned versions:

| Dependency | Version | Pinned in |
|---|---|---|
| Pingora | 0.9.0 (rustls) | `Cargo.lock` |
| redb | 4.4.0 | `Cargo.lock` |
| SvelteKit | 3.1.0 | `ui/package-lock.json` |
| adapter-static | 4.0.0 | `ui/package-lock.json` |
| Svelte | 5.57.2 | `ui/package-lock.json` |
| Vite | 8.3.4 | `ui/package-lock.json` |

Rust edition 2024, stable toolchain (built with 1.96).

### Command line

- `moxy` / `moxy serve`: runs the server.
- `moxy check`: validates `moxy.xml` and exits.
- `moxy keys list | create <name> [--owner <subject>] | revoke <id>`: offline API key management. These open the store directly, so the server must be stopped (redb holds an exclusive file lock).
- `moxy version`

## What was built

### Server (`src/server.rs`, `src/proxy.rs`)

**Startup** runs in the spec's order:
1. Read `MOXY_CONFIG`, `MOXY_DATA_DIR` and `MOXY_LOG`.
2. Load and validate `moxy.xml`. If it is missing or invalid, log the error and exit non-zero.
3. Open `moxy.redb` in the data directory, creating the directory if needed.
4. Bind to `<server host port>`.
5. Start watching `moxy.xml` once Pingora reports it is running.

**Shutdown:** SIGINT and SIGTERM both trigger a graceful shutdown. Pingora stops accepting connections, Moxy waits up to 30 s for in-flight requests (it counts them per request), then closes the KV store and exits 0.

**Errors:** every error Moxy produces itself is `{"error":{"type":"snake_case","message":"..."}}`. Validation errors add `error.details`, a list of `{location, message}`.

**Built-in routes are answered in `request_filter`.** These are `/`, `/chat*`, `/admin*`, `/auth/*`, `/api/*` and `/_app/*`. Their handlers (`src/api/`) work on small request/response types, independent of Pingora.

**Everything else uses the catch-all proxy path:**
- Routes are matched by longest literal prefix; ties go to the route declared first. No match gives 404 `route_not_found`.
- The caller is authenticated as the route requires (`none`, `api-key` or `session`); failure gives 401.
- `Authorization` and the `moxy_session` cookie are always stripped before forwarding (other cookies are kept).
- Middleware runs, then the asterisk suffix is appended to the upstream URL. The query string is preserved, every HTTP method is proxied, and responses (including SSE and chunked) stream through unbuffered.

### Configuration (`src/config/`)

- **Document model.** `doc.rs` keeps attribute values exactly as written, including `${VAR}` references. The JSON API returns this model and the admin writes it back, so resolved secrets are never shown or persisted.
- **Validation.** `xml.rs` parses structurally and rejects unknown elements. `mod.rs` compiles the document into a typed runtime `Config` and checks:
  - duplicate paths, the root path, and the reserved prefixes;
  - a missing destination;
  - unresolved `${VAR}` references;
  - every attribute value.
- **Reload.** A watcher (`notify`, falling back to polling) waits 500 ms after the last change, then reloads.
  - A valid file is swapped in atomically (`ArcSwap`); each request keeps the `Arc` it started with.
  - An invalid file is logged, recorded and shown in the admin UI, and the previous configuration stays active.
  - When a route changes or disappears, its cache entries are purged.
- **Admin writes.** The admin UI and API validate the new config, write a temp file, `fsync`, rename it over `moxy.xml`, and run the same reload code.

### Middleware (`src/middleware/`)

**Request phase** runs in document order, and the first rejection stops the pipeline before any upstream call. **Response phase** runs in reverse order.

- **headers** – `set`/`add`/`remove` on the request; inside `<response>` they apply to the response.
- **request** – `rewrite-path`, `max-body-size`, `timeout`:
  - `max-body-size` gives 413 from `Content-Length`, or mid-stream for chunked bodies.
  - `timeout` defaults to 60 s; exceeding it gives 504 `upstream_timeout`.
- **response** – header ops, plus `<cache ttl>`.
- **load-balance** – `round-robin`, `random` or `failover`.
  - An upstream that fails to connect, times out or returns 5xx is skipped for 30 s.
  - If it fails to connect, the request is retried on another upstream.
- **ai** – `openai`, `anthropic` or `ollama`:
  - overrides the model in the request body;
  - counts token usage from JSON responses and from SSE/NDJSON streams.
- **rate-limit** – in-memory fixed windows (1s/1m/1h). Rejections are 429 with `Retry-After` and type `rate_limited`.
- **token-budget** – 1h/1d/30d windows aligned to the epoch, stored as `usage#<id>#<route>#<window start>`. Rejections are 429 with `Retry-After` and type `token_budget_exceeded`.

**Upstream failures:** connect failure gives 502 `upstream_unavailable`, a timeout gives 504 `upstream_timeout`, and upstream HTTP errors pass through unchanged.

### Auth (`src/auth/`)

- **OIDC.** Authorization code flow with PKCE (S256), using discovery and JWKS.
  - The ID token is verified (RS/ES/PS via JWKS, or HS* with the client secret), along with issuer, audience, expiry and nonce.
  - Client authentication uses `client_secret_basic` or `client_secret_post`, whichever the provider advertises.
  - If the ID token has no email, Moxy asks the userinfo endpoint.
- **Login state** is stored as `auth#state#<state>` with a 10-minute expiry and is consumed by the callback. An invalid or expired state gives 400.
- **Users** are stored as `user#<sub>`. The role is computed at sign-in and recomputed against the current `<admin>` list on every request.
- **Sessions** are stored as `session#<id>`, where the id is 32 random bytes in base64url.
  - The record holds a CSRF token.
  - Expiry slides: it is extended when a request arrives more than 1 h after the last refresh.
  - The `moxy_session` cookie is HttpOnly, SameSite=Lax and Path=/. It is Secure unless the server host is `127.0.0.1` or `localhost`.
- **API keys** are `moxy_` plus 32 base64url characters. Only the SHA-256 is stored (`apikey#<hash>`), and the plaintext is returned once.

### JSON API (`src/api/handlers.rs`)

| Endpoint | Access |
|---|---|
| `GET /api/endpoints` | anonymous |
| `GET /api/me` | user |
| `POST /api/chat` | user; SSE |
| `GET /api/config` | admin |
| `PUT /api/config/routes` | admin |
| `PUT /api/config/system` | admin |
| `PUT /api/config/middleware?path=<route path>` | admin |
| `GET/POST /api/keys`, `DELETE /api/keys/{id}` | admin |

- Invalid config input gives 422 `validation_failed`.
- Missing credentials give 401; an insufficient role gives 403.
- Session-authenticated state changes must send `X-CSRF-Token`; `/api/me` returns the token.

### Storage (`src/kv.rs`)

- `KvStore` trait, implemented by `RedbStore` (one table, `moxy.redb`) and an in-memory store used by unit tests.
- Every value is a JSON envelope with an optional expiry. Expired entries are ignored on read and removed by a sweep every 10 minutes, which also clears old rate-limit windows.

### Cache (`src/middleware/cache.rs`)

- Stores GET 200 responses, and non-streamed 200 responses to POST on `ai` routes.
- `Cache-Control: no-cache` on the request bypasses the cache; `no-store` or `private` on the response prevents storing.
- The key is SHA-256 over route path, method, upstream URL(s), canonicalised JSON body and caller identity.
- Responses carry `X-Moxy-Cache: HIT|MISS`.

### Web UI (`ui/`)

SvelteKit with `adapter-static` in SPA mode. The fallback page is `200.html`, and assets live under `/_app`, embedded via `build.rs`. All data comes from `/api`.

- **`/`** – endpoint discovery, with a curl example per route.
- **`/chat`** – route/model picker and streaming replies. History stays in browser memory and is never stored on the server. Shows token usage.
- **`/admin`** – shows the active config (generation, hash, load time) and the last reload error, with tabs for:
  - **Routes** – add, edit and delete routes; a structured middleware editor with reordering.
  - **System settings** – server and auth, including the admin list.
  - **API keys** – create (plaintext shown once), list and revoke.

## Tests

**68 unit tests**, covering:
- duration and `${VAR}` parsing;
- XML parse and write round trip, and every validation rule;
- route matching and precedence;
- both KV stores;
- API keys, sessions, sliding expiry and roles;
- rate limiting, budget windows and recording;
- usage extraction for all three providers (whole JSON and split SSE);
- model override, cache keys and rules;
- load-balancer strategies and health;
- reload and write-back.

**15 end-to-end tests** (`tests/e2e.rs`) run the real binary against an axum mock upstream and a mock OIDC provider that signs RS256 ID tokens. They cover:
- discovery and 404s;
- credential stripping and header, path and query handling;
- rate limits, 413, 504, 502 and upstream error passthrough;
- failover and round-robin;
- unbuffered SSE;
- per-caller cache and POST caching on `ai` routes;
- model override and token budgets, including streamed usage;
- large and chunked request bodies;
- live reload, and keeping the last good config;
- the full OIDC sign-in, with role changes taking effect on the next request;
- the admin API with CSRF, key lifecycle, chat SSE, config writes and logout;
- refusing to start with an invalid config;
- graceful shutdown.

I also drove the UI once with headless Chromium (Playwright) against a mock provider: discovery, sign-in redirect, editing and saving a route, validation errors, key creation and revocation, streamed chat, and sign-out all worked. That script was a one-off and is not part of the repository.

## Decisions I made

1. **API keys act with the `user` role on the JSON API**, whoever owns them. The spec says every key belongs to an administrator but doesn't say what a key may do. Inheriting admin rights would let any agent rewrite the configuration, so keys can call proxy routes, `/api/endpoints`, `/api/me` and `/api/chat`, but not the admin endpoints.
2. **`DELETE /api/keys/{id}` revokes rather than erases.** It sets `revoked: true`, so the admin list keeps an audit trail. Revoked keys are rejected everywhere.
3. **CSRF is enforced on `/api/*` and on `POST /auth/logout`.** Logout accepts the token as a header or as a `csrf_token` form field. It is not enforced on `auth="session"` proxy routes, which are forwarded unchanged; SameSite=Lax still blocks cross-site POSTs.
4. **The `<auth>` element is optional.** Without it, sign-in returns 503 `oidc_not_configured`, but `none` and `api-key` routes still work (keys come from `moxy keys create`). When present, it needs `issuer`, `client-id` and at least one `<admin>`.
5. **`client-secret` must contain a `${VAR}` reference.** The spec says secrets must never appear literally in `moxy.xml`, so a literal secret fails validation.
6. **Unknown attributes are rejected** as well as unknown elements, and the destination must be the last child of `<route>` (the spec says "followed by"). XML comments are lost when the admin UI rewrites the file, as the spec allows.
7. **Route paths and suffixes:**
   - `/x/*` matches `/x/...` and also `/x` itself.
   - The suffix keeps its leading slash and is appended to the upstream path, so `/openai/*` → `https://api.openai.com/v1` maps `/openai/chat/completions` to `/v1/chat/completions`.
   - `rewrite-path from/to` is a prefix replacement on that suffix.
   - `/` and `/*` are both rejected as "the root path".
8. **Attribute value formats:**
   - Durations are `500ms`, `60s`, `5m`, `24h`, `30d`, compounds like `1h30m`, or bare seconds.
   - `rate-limit per` is `1s|1m|1h` (or `second|minute|hour`).
   - `token-budget per` is `1h|1d|30d` (or `hour|day|month`).
9. **Default counting keys.** The spec gives none. `rate-limit` uses `ip` on `auth="none"` routes, `user` on session routes and `api-key` otherwise. `token-budget` uses `user` on session routes and `api-key` otherwise. If the requested identity is missing (for example `key="api-key"` on a chat request from a signed-in user), it falls back to the user's subject, then to the client IP.
10. **Usage keys.** With several `token-budget` elements in the same window, usage is charged once per window, because the usage key has no per-element part. Usage is recorded after the response, so the request that crosses the budget is allowed and later ones are rejected.
11. **`/api/chat` contract:**
    - Request body: `{route, model?, messages:[{role,content}], max_tokens?, temperature?}`.
    - Moxy translates the request into the provider's native API: OpenAI `/chat/completions`, Anthropic `/messages`, Ollama `/api/chat`.
    - The reply is normalised into SSE events `{"type":"delta"|"error"|"done"}`.
    - The route's full request phase runs (headers, rewrites, limits, budgets, load balancing), charged to the signed-in user.
    - For OpenAI streams, Moxy asks for usage (`stream_options.include_usage`). It does this on the proxy path too when a token budget needs counting.
12. **`/api/endpoints` adds `chat`, `provider` and `model`** to path, description and auth, because the chat model picker needs them. The discovery page shows the same fields. Upstreams, headers and credentials are never exposed.
13. **Middleware replacement is `PUT /api/config/middleware?path=<route path>`.** A query parameter avoids encoding the `/*` of a route path in the URL path. The routes endpoint accepts a bare array or `{"routes": [...]}`.
14. **Config version** is shown as a generation counter plus the SHA-256 of the file.
15. **Server address changes take effect only after a restart.** Changing `<server host/port>` at runtime is accepted and logged with a warning, because Pingora's listener is fixed once bound.
16. **The OIDC `redirect_uri` comes from the request:** `{X-Forwarded-Proto or http}://{Host}/auth/callback`. Register that URL with your identity provider.
17. **Invalid `MOXY_LOG` is a startup error.** Any value other than `error`, `warn`, `info`, `debug` or `trace` makes Moxy exit non-zero rather than silently using `info`.
18. **Offline key tool.** `moxy keys create` is an extension I added so a first key can be made without an identity provider. Keys made this way are owned by `--owner` (default `cli`) rather than a signed-in admin.

## Limitations and things left unimplemented

- **Large AI request bodies are not cached.** POST bodies on `ai` routes are read before connecting upstream (so they can be part of the cache key) only when `Content-Length` is 64 KiB or less, because that is the size Pingora can replay from its retry buffer. Larger or chunked bodies are still rewritten (model override, usage request) by buffering them as they stream, but they bypass the POST cache.
- **The timeout is per read.** Pingora's `read_timeout` limits each wait for upstream data, including the response headers, rather than the total response time, so a slow but steady stream is not cut off. Connection timeouts are capped at 10 s and map to 504.
- **The response phase is partly order-independent.** Response header operations and caching follow reverse document order, but token accounting runs once the response completes, whatever its position in the pipeline.
- **Rate-limit counters are per process** and reset on restart, as specified. Moxy is single-node, so there is no shared state.
- **No WebSocket-specific handling.** Pingora's default policy (WebSocket upgrades allowed) applies.
- **Moxy does not terminate TLS.** The spec defines no TLS settings. If you put it behind TLS, set `X-Forwarded-Proto: https` so OIDC redirects use https.
- **CLI key commands need the server stopped**, because redb uses an exclusive file lock.
- **No automated UI test suite.** The SvelteKit pages were checked manually with headless Chromium as described above; there are no committed UI tests.
