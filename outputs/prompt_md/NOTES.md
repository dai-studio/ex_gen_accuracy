# Moxy: implementation notes

The specification was one paragraph (`prompt.md`). It asked for a gateway that sits between developers, admins and AI agents and one or more AI model endpoints. The gateway loads routes from an XML file, applies per-route middleware to requests and responses, and forwards to upstream URLs. It also has built-in routes for administration, authentication, a chat UI, an API, and endpoint discovery. Everything else below is a design decision I made; the most consequential ones are listed in "Decisions".

## What was built

A complete, dependency-free gateway in TypeScript for Node.js ≥ 22.18. `README.md` is the user guide and full configuration reference.

- **XML configuration** (`src/xml.ts`, `src/config.ts`)
  - A small XML parser of my own: elements, attributes, text, CDATA, comments and entities, with line/column errors. DOCTYPE is rejected, which rules out XXE and entity bombs.
  - A strict loader: unknown elements or attributes are errors that include the line number.
  - `${ENV}` / `${ENV:-default}` interpolation.
  - Named upstreams, reusable middleware `<chains>`, global middleware, and `<models>` per route with model aliasing.
- **Routing and forwarding** (`src/router.ts`, `src/proxy.ts`, `src/pipeline.ts`)
  - Exact and prefix (`/x/*`) routes with specificity ordering, method filtering, and 404/405 responses.
  - Correct header handling: hop-by-hop removal, `X-Forwarded-*`, request ids, injected provider credentials, and stripping the Moxy cookie.
  - Upstream selection: failover, weighted round-robin and random strategies, configurable retries and `retry-on`, and passive health tracking.
  - Inactivity timeouts (504), connection failures (502), and streaming pass-through (SSE). An aborted client aborts the upstream request.
- **Middleware**: 16 built-ins.
  - Access control: `auth`, `rate-limit`, `quota`, `ip-filter`, `body-limit`, `cors`.
  - Request rewriting: `set-header`, `remove-header`, `rewrite-path`, `timeout`, `json-set`.
  - AI-specific: `max-tokens`, `model-filter`, `system-prompt`, `redact` (PII/secret scrubbing or blocking).
  - Response caching: `cache`.
  - All of them share one onion-model contract (`src/middleware/types.ts`). Adding another means writing one factory and registering it.
- **Token usage tracking** (`src/usage.ts`): reads OpenAI, Anthropic and Ollama usage from JSON bodies and from SSE/NDJSON streams as they pass through. This feeds the admin statistics and the `quota` middleware.
- **Built-in routes**:
  - Authentication (`/auth/*`): login page and JSON login, logout with token revocation, `me`, and token minting.
  - Administration (`/admin` dashboard and `/admin/api/*`): status, routes with upstream health, stats by route/caller/model, live reload, stats reset.
  - Chat UI (`/chat`): streaming, model picker, system prompt, stop button.
  - OpenAI-compatible API (`/api/v1/models`, `/chat/completions`, `/embeddings`): dispatches by model through the owning route's middleware.
  - Discovery (`/.well-known/moxy.json`), plus `/healthz` and a landing page.
- **CLI** (`src/cli.ts`): `serve`, `validate`, `hash-password`, `gen-key`, `hash-key`. SIGHUP reloads the config; SIGINT/SIGTERM shut down gracefully.
- **Examples**: `examples/moxy.xml` (commented) and `examples/mock-llm.ts`, an OpenAI-compatible mock so the whole system can be tried without API keys.
- **Tests**: 155 `node:test` tests.
  - Unit tests: parser, config validation, router, auth/tokens, usage parsing, limiter, upstream ordering.
  - HTTP integration tests that run real traffic through the gateway to mock upstreams: proxying, failover, timeouts, streaming and client aborts, every middleware, and every built-in route including reload.

Verified: `npm run typecheck` (sources and tests), `npm run build` and `npm test` all pass. I also ran the compiled `dist/cli.js` by hand against the mock model and checked form login, cookie-authenticated chat, streaming, caching, SIGHUP reload and shutdown.

## Decisions

**Platform**
- **TypeScript on Node, zero runtime dependencies.** Go wasn't installed and Node was. The gateway only uses `node:http`/`https`/`crypto`/`net`. The tests run the `.ts` sources directly (Node type stripping), and `tsc` produces `dist/`.

**Configuration**
- **Config shape.** The prompt gave no schema, so I designed `<moxy>` with `server`, `logging`, `auth`, `builtins`, `upstreams`, `chains`, `middleware` and `routes` sections. Middleware are child elements named after their type, listed in execution order.
- **Missing `${VAR}` is a startup error.** `${VAR:-}` makes a variable optional. Failing loudly seemed better than silently sending empty credentials.

**Routing**
- **Path semantics.** Prefix routes append the remainder of the path to the upstream URL; exact routes forward to the upstream URL as written. Built-in paths win over configured routes, and an overlapping route is rejected at load time unless that built-in is disabled.
- **Dot segments are rejected (400) on the raw request target.** WHATWG URL parsing would silently resolve `%2e%2e`, and encoded slashes (`..%2F`) could be decoded by an upstream, so the check runs before any parsing.

**Forwarding and responses**
- **Request bodies are buffered** (up to `max-body`, default 10 MB) so middleware can inspect and rewrite JSON, and so failover can resend the body.
- **Responses stream.** They are buffered only when a middleware asks for the body (currently only `cache`), and never for `text/event-stream`.
- **`Accept-Encoding: identity` is sent upstream** so usage can be read from responses. This gives up upstream compression, which matters little for AI APIs.
- **Moxy's `X-Request-Id` is kept** on responses, because it matches the access log. A provider's own id is relayed as `X-Upstream-Request-Id`.
- **Errors use the OpenAI error envelope** so existing SDKs display Moxy's messages.

**Authentication**
- **Auth model.**
  - Users with scrypt hashes (plain-text passwords are allowed but discouraged).
  - API keys stored as SHA-256 hashes.
  - HS256 JWT sessions.
  - Roles are arbitrary strings; `admin` passes every role check and is required for `/admin`.
  - Each token's roles are re-read from the current config on every request, so removing a user or key invalidates its tokens immediately.
- **Auth secret.** If no secret is configured, a random one is generated per process; tokens then don't survive a restart, but they do survive a reload.
- **CSRF.** Session cookies are `HttpOnly; SameSite=Strict`, and a cookie is ignored on unsafe methods whose `Origin` doesn't match `Host`.
- **Login brute-force protection.** Only failed attempts are counted: 10 per IP per 5 minutes, after which logins return 429. The numbers are fixed in code.
- **Access to built-ins.** `/admin` always needs an admin principal; with no admin configured it is unreachable (a startup warning says so). `/chat` needs a login only if users exist and `chat-auth` is on.

**API, discovery and failover**
- **Unified API dispatch.** A model is served by the first route that declares it; duplicates are logged and ignored. `/api/v1/models` lists only models the caller passes the route's `auth` checks for, and hides hidden routes.
- **Discovery content.** It never shows upstream URLs or header values. The admin route view shows upstream URLs but only the *names* of injected headers.
- **Passive health tracking.** After 2 consecutive failures an upstream is down for 15 s; down upstreams are tried last rather than skipped. These numbers are fixed in code.
- **Durations.** A bare number means seconds.

## Left unimplemented / known limitations

**Protocol support**
- **No WebSocket or HTTP `Upgrade` proxying**, so realtime and voice APIs that need WebSockets are not supported. HTTP/2 to clients is not offered either.
- **Request bodies are always buffered**, so very large uploads (e.g. audio files) are capped by `max-body`.
- **The unified API speaks the OpenAI format only.** It forwards the body unchanged and does not translate between OpenAI and Anthropic/Gemini formats. Pass-through routes work with any provider's native API.

**State and scaling**
- **All state is in-memory and per-process**: rate limits, quotas, cache, stats, upstream health and token revocations. Nothing is shared between replicas or kept across restarts. Running several instances would need a shared store such as Redis.
- **No metrics export** (Prometheus/OpenTelemetry); only the JSON admin endpoints and structured access logs.

**Configuration**
- **No watching of the config file**; reloads are manual (SIGHUP or the admin API). Changes to `server` (host/port/TLS) need a restart, and the reload response says so.
- **Upstream health is passive only**: there are no active health-check probes.
- **Middleware cannot run on built-in routes.** It applies only to configured routes, including requests the unified API dispatches into them.

**Usage, quotas and cost**
- **Token usage needs the provider to report it.** OpenAI streams report usage only with `stream_options.include_usage`; Moxy does not inject that option. Usage is not tracked for requests answered from the cache.
- **Quotas count tokens after the fact**, so a request that starts under budget can overshoot it. There is no cost/price accounting.

**Security**
- **No OAuth/OIDC/SSO.** Users and keys live in the config file, and there is no API for managing them.
- **Admin actions are only logged**, not audited separately.
