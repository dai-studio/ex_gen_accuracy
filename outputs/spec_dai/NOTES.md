# Moxy: implementation notes

This implements Moxy as described in `spec.dai`. Moxy is a locally run Rust gateway built on Pingora. It loads routes from `moxy.xml` (MML v1), runs each route's middleware, and forwards traffic to upstream URLs. It also serves `/`, `/chat`, `/admin`, `/auth/*`, `/api/*` and `/_app/*` itself. State is kept in a single redb file that uses Single Table Design.

## Build and run

```sh
(cd ui && npm ci && npm run build)   # SvelteKit SPA -> ui/build (already built in this directory)
cargo build --release                 # embeds ui/build into the binary
cp moxy.example.xml moxy.xml          # edit; secrets via ${ENV_VAR}
MOXY_OIDC_CLIENT_SECRET=... OPENAI_API_KEY=... target/release/moxy
cargo test                            # 27 unit + 20 end-to-end tests
```

Other CLI commands: `moxy check` validates `moxy.xml`. `moxy keys create --name N [--owner SUB]` and `moxy keys list` work only while the server is stopped, because redb allows one process at a time. `cargo run --example seed_session -- <email>` creates a browser session without an identity provider (for development).

## Layout

| Path | Purpose |
|---|---|
| `src/config/` | MML: `parse.rs` (XML → raw model: unknown elements, unknown attributes and required attributes), `resolve.rs` (`${NAME}` substitution and semantic validation → typed `Resolved`, route matching), `write.rs` (raw model → XML), `model.rs` (raw model; also the JSON shape used by `/api/config`) |
| `src/store.rs` | `KvStore` trait, `RedbStore` (one table, `moxy.redb`), `MemStore`; `expiresAt` filtering and sweep |
| `src/state.rs` | Active config (`ArcSwap`), reload and atomic write-back, in-memory rate limiter, load balancer (30 s skip of failed upstreams), in-flight counter |
| `src/pipeline.rs` | Shared request-phase middleware (doc order, first rejection stops), token-budget accounting, cache keys and policy, upstream URL mapping |
| `src/proxy.rs` | Pingora `ProxyHttp` implementation: built-ins in `request_filter`, proxying, body rewrite, streaming, response middleware, error mapping |
| `src/ai.rs` | `<ai>`: model rewrite, token counting for OpenAI, Anthropic and Ollama (JSON, SSE, NDJSON), chat request building |
| `src/auth.rs` | API keys, sessions (sliding), users and roles, OIDC code flow with PKCE and nonce (`openidconnect` crate) |
| `src/api.rs`, `src/chat.rs` | `/api/*`; `POST /api/chat` streams SSE through the route's pipeline |
| `src/builtin.rs`, `src/ui.rs` | Reserved-route dispatch; embedded SPA (`rust-embed`) |
| `src/server.rs`, `src/watcher.rs`, `src/main.rs` | Startup order, graceful shutdown, `moxy.xml` watcher (500 ms debounce), CLI |
| `ui/` | SvelteKit 3 + Svelte 5 SPA (adapter-static): endpoints list, chat, admin (routes, middleware, settings, API keys, config status) |
| `tests/` | End-to-end tests that spawn the real binary against mock upstreams and a mock OIDC provider (RS256-signed ID tokens) |

## How it was verified

- `cargo test` covers every middleware (headers, rewrite-path, max-body-size with Content-Length and chunked bodies, timeout → 504, round-robin and failover load balancing, model override, rate limit → 429 + Retry-After, token budget for buffered and streamed responses, cache hit/miss/no-cache/no-store/per-caller/streamed). It also covers auth modes, credential stripping, `/api` (roles, CSRF, 422 validation, write-back to `moxy.xml`, keys lifecycle), `/api/chat` SSE, hot reload (valid and invalid files), cache drop when a route changes, the full OIDC login, startup failures and SIGTERM shutdown.
- I drove the UI in headless Chromium (Playwright, installed under `target/uitest`) against a running server and a mock OpenAI upstream:
  - the endpoint list renders;
  - anonymous `/chat` redirects to login;
  - a chat message streams a reply and shows token counts;
  - the admin page created an API key, which then worked against the proxy;
  - editing a route plus adding header middleware was written to `moxy.xml`;
  - an invalid path showed the 422 errors.

## Decisions I made (not settled by the spec)

**Proxying and middleware**
- **Route paths.** `/x/*` matches `/x` and `/x/...`, and the suffix is everything after `/x`. Reserved prefixes are checked per path segment, so `/chatgpt/*` is allowed but `/chat/*` is not. `/*` is allowed as a catch-all. Unknown *attributes* are rejected as well as unknown elements, so typos are caught. `version="1"` is required.
- **`rewrite-path from to`** rewrites the start of the forwarded suffix (the part matched by `*`), at segment boundaries.
- **`timeout duration`** is applied as Pingora's connect, read and write timeouts. It is an idle timeout, not a limit on total request time, so long streams are not cut off.
- **Middleware defaults and limits.** Rate-limit and token-budget `key` default to `api-key`. Load-balance `strategy` defaults to `round-robin`. At most one `<ai>` and one `<cache>` per route. `<token-budget>` requires `<ai>`, which the spec says is needed to count usage.
- **Who a limit applies to.** For session callers, `key="api-key"` falls back to the user. Callers with no identity (routes with `auth="none"`) fall back to the client IP. API-key callers' `user` is the key's owner.
- **Windows.** Rate limits and token budgets use fixed windows aligned to the Unix epoch. Usage keys are `usage#<key id | sub | ip:addr>#<route path>#<window start (unix secs)>`.
- **Token counting.** Tokens = input + output from provider responses. Usage is only counted (and `Accept-Encoding: identity` forced) on routes that have `<token-budget>`. For streamed OpenAI requests on such routes, Moxy adds `stream_options.include_usage=true`, because otherwise OpenAI does not report usage in streams. The budget check happens before a request, so a single request can overshoot the budget.
- **"Errors" for load balancing** means transport failures: connect errors, DNS failures and timeouts. HTTP error responses pass through and do not mark an upstream down. If every upstream is marked down, the first untried one is still attempted.
- **Request body handling.** On `<ai>` routes, bodies up to 64 KiB with a Content-Length are read before connecting, so they can be rewritten and used in cache keys. This uses Pingora's retry buffer, which is limited to 64 KiB. Larger or chunked bodies are rewritten while streaming, and such requests are not cached.
- **Cache.**
  - The lookup runs after the whole request phase, so rate limits and budgets always apply.
  - "Streamed" means the request asked for `stream: true`, or the response is `text/event-stream` or `application/x-ndjson`.
  - The upstream URL in the key is built from the route's first upstream, so all members of a load-balanced route share entries.
  - Headers are stored after response middleware has run. Hits get an `x-moxy-cache: hit` header.

**`/api/chat`**
- Request body: `{"route": "/openai/*", "messages": [...], "model"?: "...", "maxTokens"?: N}`. If the route's `<ai model>` is set, it wins.
- Provider endpoints are fixed suffixes on the upstream URL: OpenAI `/chat/completions`, Anthropic `/messages` (with `anthropic-version: 2023-06-01`, `max_tokens` defaults to 1024, system messages are lifted out), Ollama `/api/chat`.
- The SSE output uses one normalised format for every provider: `{"type":"delta"|"error"|"done", ...}`. It is sent with reqwest (not Pingora), but runs the same pipeline code and load balancer.

**Auth and users**
- **Login redirect URI** is `http://<Host header>/auth/callback`; it must be registered at the identity provider. There is no TLS listener.
- **Users without auth configured.** `<auth>` is optional. Without it, `/auth/login` returns 503 `auth_not_configured`, and API keys can only come from the CLI.
- **Roles.** An API key acts with its owner's role, re-checked against the current `<admin>` list. So an admin's key can call admin endpoints, which scripts need. Admin is never granted when the identity provider reports `email_verified=false`; to support this I added an `emailVerified` field to the user record.
- **CSRF.** `/api/me` returns `csrfToken` for session callers. `POST /auth/logout` does not require the token, because the SameSite=Lax cookie already protects it.

**`/api` shapes**
- `GET /api/endpoints` returns `{"endpoints":[{path, description, auth, chat}]}`.
- `GET /api/config` returns `{version, loadedAt, path, lastError, config}`. `config` holds the raw values, so secrets appear only as `${NAME}`.
- `PUT /api/config/routes` accepts an array or `{"routes": [...]}`.
- `PUT /api/config/routes/{url-encoded path}/middleware` accepts an array or `{"middleware": [...]}`.
- `PUT /api/config/settings` takes `{server?, auth?}`: a missing key keeps the current value and `null` removes `auth`.
- `DELETE /api/keys/{id}` marks the key revoked; revoked keys stay listed.
- **Config version** is the first 12 hex characters of the SHA-256 of the file. After a successful `/api` write, the new config is activated immediately rather than waiting for the watcher. The watcher then sees the same version and does nothing.

**Shutdown and storage**
- **Shutdown.** SIGINT and SIGTERM are both graceful: listeners close, and the process exits as soon as in-flight requests reach zero, or after 30 s. The KV store is not closed explicitly: the process exits directly, and redb commits are durable.
- **Sweep.** The KV sweep runs every 10 minutes. Login-state records expire after 10 minutes. Sessions slide when the last refresh (expiresAt − ttl) is more than 1 h old.

**Tooling**
- **SvelteKit 3 / Vite 8** (latest stable) are used. Kit 3 has no `svelte.config.js` (config is passed to the `sveltekit()` Vite plugin) and uses `#lib` instead of `$lib`.

## Not implemented / limitations

- **TLS and HTTP/2.** No TLS listener (Moxy serves plain HTTP locally) and no HTTP/2 to upstreams (HTTP/1.1, with TLS via rustls and system roots).
- **Host/port changes** in `moxy.xml` are logged but need a restart; the other settings reload live.
- **OIDC.** No RP-initiated logout at the identity provider and no userinfo endpoint call; claims come from the ID token only. Provider discovery is cached for 1 h.
- **Body size limits.** Built-in routes accept bodies up to 4 MiB. Cached response bodies are limited to 8 MiB, and buffered (non-streamed) usage parsing to 16 MiB.
- **UI tests.** The UI has no automated test in `cargo test`; I verified it manually with the Playwright walkthrough above.
- **Multiple instances.** Several Moxy instances sharing one store are not supported, as the spec says.
