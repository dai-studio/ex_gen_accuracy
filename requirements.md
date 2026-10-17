# Moxy requirements (from `inputs/spec.dai`)

These are the 68 requirements used to score the three builds. Each one is taken from `inputs/spec.dai`, the BPMN + `dai:` model from which `inputs/spec.md` was written. The **Source** column names the node (its `name` and BPMN `id`) and the attribute that holds the text.

Where `spec.dai` gives an exact identifier, such as a field name, a header or a path, the requirement uses that identifier. The prose in `spec.md` sometimes describes the same thing only in words. Those requirements are marked **†** and listed at the end of this file.

Scoring: **PASS** means the code does what the requirement says. **PARTIAL** means it is implemented but deviates; the score note names the deviation. **FAIL** means it is missing or contradicts the requirement.

## A. Stack and platform

| ID | Requirement | Source in spec.dai |
|---|---|---|
| A1 | Rust, stable toolchain, edition 2024. | `rust` (Activity_0n9lc1t) `dai:version` |
| A2 | Pingora is the HTTP proxy layer (listener, routing to upstreams, load balancing, streaming), latest stable, pinned in `Cargo.lock`. | `pingora` (Activity_1nafjvj) `dai:version`, `dai:purpose` |
| A3 | SvelteKit with adapter-static, built as a single-page app, latest stable, pinned in `package-lock.json`. | `sveltekit` (Activity_09jmudl) `dai:notes`, `dai:version` |
| A4 | The UI build is embedded in the Rust binary and served under `/_app/*`. No Node.js at runtime. The UI gets all data from `/api`. | `sveltekit` (Activity_09jmudl) `dai:notes`; decision `SvelteKit served by Moxy` (Decision_ui) |
| A5 | redb is the embedded key-value engine behind the KV store, latest stable, pinned in `Cargo.lock`. | `redb` (Activity_redb001); decision `Embedded KV engine` (Decision_kv) |

## B. Process and environment

| ID | Requirement | Source in spec.dai |
|---|---|---|
| B1 | Only `MOXY_CONFIG` (default `./moxy.xml`), `MOXY_DATA_DIR` (default `./data`, created if missing) and `MOXY_LOG` (`error`, `warn`, `info`, `debug` or `trace`; default `info`) are read directly, and only at startup. | `environment variables` (Activity_0m863ow) `dai:notes`; decision `Env vars vs config` (Decision_env) |
| B2 | Startup runs in this order: 1. read the environment variables; 2. load and validate `moxy.xml`, and if it is missing or invalid, log the error and exit non-zero; 3. open the KV store; 4. bind to `<server host port>` (default `127.0.0.1:8080`); 5. start watching `moxy.xml`. | `serve` (Activity_0k3fev3) `dai:notes` "Startup sequence", `dai:baseUrl` |
| B3 | On SIGINT or SIGTERM: stop accepting connections, let in-flight requests finish (at most 30 s), then close the KV store. | `serve` (Activity_0k3fev3) `dai:notes` "Shutdown" |
| B4 | Every error Moxy itself returns uses `{"error": {"type": "<snake_case_type>", "message": "<human readable>"}}`. | `serve` (Activity_0k3fev3) `dai:notes` |

## C. Configuration (`moxy.xml`, MML v1)

| ID | Requirement | Source in spec.dai |
|---|---|---|
| C1 | The schema is `<moxy version="1">`, then `<server host port/>`, then `<auth issuer client-id client-secret session-ttl>` with one or more `<admin email/>` children, then repeatable `<route>` elements. | `moxy.xml (config)` (Activity_0q42btx) `dai:notes` "Schema (MML v1)" |
| C2 | `<route>` attributes: `path` is required and is either a literal prefix ending in `/*` or an exact path; `auth` is `none`, `api-key` or `session`, default `api-key`; `description` is shown on `/`. | `moxy.xml (config)` (Activity_0q42btx) `dai:notes` "&lt;route&gt; attributes" |
| C3 | Each route lists its middleware elements in execution order, followed by exactly one destination: `<upstream url/>` or `<load-balance>`. | `moxy.xml (config)` (Activity_0q42btx) `dai:notes` schema |
| C4 | `${NAME}` in any attribute is replaced with environment variable `NAME` at load time. A missing variable is a validation error. | `moxy.xml (config)` (Activity_0q42btx) `dai:notes` |
| C5 | Validation rejects: a duplicate route path; a path equal to `/` or under `/chat`, `/admin`, `/auth`, `/api` or `/_app`; a route with no `<upstream>` or `<load-balance>`; an unknown element; an unresolved `${NAME}`. | `moxy.xml (config)` (Activity_0q42btx) `dai:notes` "Validation errors"; decision `Route matching` (Decision_routing) |
| C6 | The file is watched. On a change (debounced 500 ms) it is parsed and validated. If it is valid, the new config is swapped in atomically and in-flight requests finish on the old one. If it is invalid, the previous config stays active and the error is logged and shown in `/admin`. | `moxy.xml (config)` (Activity_0q42btx) `dai:notes` "Lifecycle" |
| C7 | `/admin` edits are written back to the file: validate, write a temp file, atomically rename it over the original. The rename triggers the normal reload. | `moxy.xml (config)` (Activity_0q42btx) `dai:notes`; decision `/admin persists to moxy.xml` (Decision_adminwrite) |

## D. Routing

| ID | Requirement | Source in spec.dai |
|---|---|---|
| D1 | Built-in routes are reserved and matched first: `/` (exact), `/chat` and `/chat/*`, `/admin` and `/admin/*`, `/auth/*`, `/api/*`, `/_app/*`. | `Routes` (Activity_00vb7qp) `dai:notes` step 1 |
| D2 | Proxy routes from `moxy.xml` are handled by `/*`. The longest matching path prefix wins; on a tie, the route declared first wins. | `Routes` (Activity_00vb7qp) `dai:notes` step 2; decision `Route matching` (Decision_routing) |
| D3 | No match returns 404 `{"error": {"type": "route_not_found", ...}}`. | `Routes` (Activity_00vb7qp) `dai:notes` step 3 |

## E. Built-in pages

| ID | Requirement | Source in spec.dai |
|---|---|---|
| E1 | `GET /` is an anonymous SvelteKit page (200) listing each proxy route's path, description and auth mode. It never shows upstream URLs, headers or credentials. | `/ (anon)` (Activity_1ceywrr) `dai:notes`, `dai:authType="none"`, `dai:statusCode="200"` |
| E2 | `GET /chat` requires a signed-in session (user or admin). Without one it returns 302 to `/auth/login?return_to=/chat`. With one it returns 200. | `/chat` (Activity_1kjuls9) `dai:notes`, `dai:statusCode="200"` |
| E3 | The chat model picker lists every proxy route that has `<ai>`. Messages go to `POST /api/chat`, which runs the route's full middleware pipeline (so rate limits and token budgets apply to the user) and streams the reply as SSE. History is kept in the browser only. | `/chat` (Activity_1kjuls9) `dai:notes` |
| E4 | `GET /admin` requires the admin role. A signed-in non-admin gets 403. An anonymous caller is redirected to `/auth/login?return_to=/admin`. An admin gets 200. | `/admin` (Activity_06nppal) `dai:notes`, `dai:statusCode="200"` |
| E5 | The admin UI can edit routes and their upstreams, edit system settings (`<server>` and `<auth>`), edit each route's middleware, create, list and revoke API keys, and show the active config version and the last reload error. | `/admin` (Activity_06nppal) `dai:notes` "Functions" |

## F. Sign-in routes (`/auth/*`, OIDC)

| ID | Requirement | Source in spec.dai |
|---|---|---|
| F1 | Sign-in uses the OpenID Connect authorization code flow with PKCE. `/auth/*` needs no prior authentication. | `/auth` (Activity_19uonmy) `dai:notes`, `dai:authType="none"` |
| F2 | `GET /auth/login?return_to=<path>` creates a login-state record and redirects (302) to the OIDC provider. | `/auth` (Activity_19uonmy) `dai:notes` |
| F3 | `GET /auth/callback` validates state and nonce, exchanges the code, creates or updates the user record, creates a session, sets the cookie, and redirects to `return_to` (default `/`). | `/auth` (Activity_19uonmy) `dai:notes` |
| F4 | An invalid or expired state at the callback returns 400. | `/auth` (Activity_19uonmy) `dai:notes` |
| F5 | `POST /auth/logout` deletes the session, clears the cookie and redirects to `/`. | `/auth` (Activity_19uonmy) `dai:notes` |
| F6 | Only same-origin relative paths are accepted for `return_to`. | `/auth` (Activity_19uonmy) `dai:notes` |

## G. JSON API (`/api/*`)

| ID | Requirement | Source in spec.dai |
|---|---|---|
| G1 | Callers authenticate with the session cookie or with `Authorization: Bearer <moxy API key>`. | `/api` (Activity_107p1xs) `dai:notes`, `dai:authType="bearer"` |
| G2 | `GET /api/endpoints` is anonymous and returns the same data as `/`. | `/api` (Activity_107p1xs) `dai:notes` |
| G3 | `GET /api/me` requires a user and returns the caller's identity and role. | `/api` (Activity_107p1xs) `dai:notes` |
| G4 | `POST /api/chat` requires a user. It takes the body `{"route": "<route path>", "messages": [...]}` and streams the reply as SSE. | `/api` (Activity_107p1xs) `dai:notes` |
| G5 | `GET /api/config` requires an admin and returns the current config as JSON. | `/api` (Activity_107p1xs) `dai:notes` |
| G6 † | `PUT /api/config/routes`, `PUT /api/config/settings` and `PUT /api/config/routes/{path}/middleware` require an admin. Each validates its input, then writes `moxy.xml`. Invalid input returns 422 with the validation errors. | `/api` (Activity_107p1xs) `dai:notes` |
| G7 | `GET /api/keys`, `POST /api/keys` and `DELETE /api/keys/{id}` require an admin. The plaintext key is returned only once, by POST. | `/api` (Activity_107p1xs) `dai:notes` |
| G8 | Missing credentials return 401. An insufficient role returns 403. | `/api` (Activity_107p1xs) `dai:notes` |
| G9 | State-changing calls made with a session cookie must send `X-CSRF-Token` matching the session's `csrfToken`. | `/api` (Activity_107p1xs) `dai:notes`; `Session` (Activity_0h0h53q) property `csrfToken` |

## H. Proxy path (`/*`)

| ID | Requirement | Source in spec.dai |
|---|---|---|
| H1 | Each request is authenticated by the route's `auth`: `none`; `api-key` (`Authorization: Bearer <moxy API key>`, the default); or `session` (cookie). Failure returns 401. | `/*` (Activity_1xahufv) `dai:notes` step 1, `dai:authType="api-key"` |
| H2 | The caller's `Authorization` header and Moxy session cookie are always removed before forwarding. | `/*` (Activity_1xahufv) `dai:notes` step 1 |
| H3 | After middleware, the part of the path matched by `*` is appended to the upstream URL and the query string is kept (`/openai/chat/completions` → `https://api.openai.com/v1/chat/completions`). | `/*` (Activity_1xahufv) `dai:notes` steps 2–3 |
| H4 | All HTTP methods are proxied. Streaming responses (SSE, chunked) pass through without buffering, through the response phase. | `/*` (Activity_1xahufv) `dai:notes` step 4 |

## I. Middleware

| ID | Requirement | Source in spec.dai |
|---|---|---|
| I1 | The request phase runs in document order and the response phase in reverse. The first rejection stops the pipeline and the upstream is not called. | `Middleware` (Activity_0naucbp) `dai:notes` "Order"; decision `Middleware order and rejection` (Decision_mw) |
| I2 | `<headers>` with `<set name value>`, `<add name value>` and `<remove name>` applies to the upstream request. The same children inside `<response>` change response headers. | `Middleware` (Activity_0naucbp) `dai:notes` "headers", "response" |
| I3 | `<request><rewrite-path from to/>` rewrites the path. | `Middleware` (Activity_0naucbp) `dai:notes` "request" |
| I4 | `<request><max-body-size bytes/>` returns 413 when exceeded. | `Middleware` (Activity_0naucbp) `dai:notes` "request" |
| I5 | `<request><timeout duration/>` defaults to 60s and returns 504 when exceeded. | `Middleware` (Activity_0naucbp) `dai:notes` "request" |
| I6 | `<response><cache ttl/>` enables the cache. | `Middleware` (Activity_0naucbp) `dai:notes` "response"; `Cache` (Activity_0z98u12) |
| I7 | `<load-balance strategy="round-robin \| random \| failover">` contains two or more `<upstream url>` and replaces a single `<upstream>`. An upstream that errors or times out is skipped for 30 s. | `Middleware` (Activity_0naucbp) `dai:notes` "load balancing"; `model2 url` (Activity_0hb5etj) |
| I8 | `<ai provider="openai \| anthropic \| ollama" model>` parses provider bodies to rewrite the model and count token usage. It is required by `<token-budget>` and for a route to appear in `/chat`. | `Middleware` (Activity_0naucbp) `dai:notes` "ai related" |
| I9 | `<rate-limit requests per="1s \| 1m \| 1h" key="api-key \| user \| ip">` keeps counters in memory (reset on restart). Rejection is 429 with `Retry-After` and type `rate_limited`. | `Middleware` (Activity_0naucbp) `dai:notes` "rate limit" |
| I10 | `<token-budget tokens per="1h \| 1d \| 30d" key="api-key \| user">` counts usage from upstream responses and stores it in the KV store. Rejection is 429 with `Retry-After` and type `token_budget_exceeded`. | `Middleware` (Activity_0naucbp) `dai:notes` "token spend" |
| I11 | A connection failure returns 502 `upstream_unavailable`. A timeout returns 504 `upstream_timeout`. Upstream HTTP error responses pass through unchanged. | `Middleware` (Activity_0naucbp) `dai:notes` "Upstream errors" |

## J. Storage, auth records, sessions and users

| ID | Requirement | Source in spec.dai |
|---|---|---|
| J1 | One embedded redb store in a single file, `$MOXY_DATA_DIR/moxy.redb`, with one table, accessed through a `KvStore` trait. | `KV Store` (DataStoreReference_0h9ku2u) `dai:notes`; decision `Single Table Design` (Decision_std) |
| J2 | Key formats: `user#<sub>`, `session#<id>`, `auth#state#<state>`, `apikey#<sha256>`, `cache#<sha256>`, `usage#<api-key id \| user sub>#<route path>#<window start>`. | `KV Store` (DataStoreReference_0h9ku2u) `dai:notes` |
| J3 † | Every value is JSON and carries an optional `expiresAt`. Expired entries are ignored on read and deleted by a sweep every 10 min. Rate-limit counters are not stored. | `KV Store` (DataStoreReference_0h9ku2u) `dai:notes` |
| J4 † | The login-state record `auth#state#<state>` is `{nonce, pkceVerifier, returnTo, expiresAt}`. It is created by `/auth/login`, deleted on callback, and expires after 10 min. | `Auth` (Activity_12436hi) `dai:notes` |
| J5 † | API keys look like `moxy_<32 random base64url chars>`, and only the SHA-256 hash is stored. The record is `{id, name, ownerSub, createdAt, lastUsedAt, revoked}`. | `Auth` (Activity_12436hi) `dai:notes` |
| J6 † | Session model: the id is 32 random bytes, base64url-encoded. Properties are `id: str`, `userSub: str`, `csrfToken: str`, `createdAt: datetime`, `expiresAt: datetime`. | `Session` (Activity_0h0h53q) `dai:notes`, `dai:properties` |
| J7 | Cookie `moxy_session=<id>; HttpOnly; SameSite=Lax; Path=/`, plus `Secure` unless bound to 127.0.0.1 or localhost. | `Session` (Activity_0h0h53q) `dai:notes` |
| J8 | The session lifetime is `<auth session-ttl>` (default 24h) and slides: any authenticated request more than 1 h after the last refresh extends `expiresAt`. | `Session` (Activity_0h0h53q) `dai:notes` |
| J9 † | User model: properties are `sub: str`, `email: str`, `name: str`, `role: str`, `createdAt: datetime`, `lastLoginAt: datetime`. The record is created on first login and updated on every login. | `Users` (Activity_06ixee5) `dai:notes`, `dai:properties` |
| J10 | The role is computed at login from `<auth><admin email>` and stored. It is re-checked against the current config on every request. | `Users` (Activity_06ixee5) `dai:notes`; decision `Roles and API keys` (Decision_authz) |

## K. Cache

| ID | Requirement | Source in spec.dai |
|---|---|---|
| K1 | The cache is off by default and enabled per route with `<cache ttl>` inside `<response>`. | `Cache` (Activity_0z98u12) `dai:notes`; decision `Cache policy` (Decision_cache) |
| K2 | Cached: 200 responses to GET, and 200 non-streamed responses to POST on routes with `<ai>`. Not cached: streamed responses, requests with `Cache-Control: no-cache`, responses with `Cache-Control: no-store` or `private`. | `Cache` (Activity_0z98u12) `dai:notes` |
| K3 | The key is `cache#<sha256 of route path + method + upstream URL + normalised body + caller key>`. | `Cache` (Activity_0z98u12) `dai:notes` |
| K4 | Entries expire after ttl. All of a route's entries are dropped when that route changes in `moxy.xml`. | `Cache` (Activity_0z98u12) `dai:notes` |

## † Where spec.dai is more exact than spec.md

When `spec.dai` was turned into prose for `spec.md`, six requirements lost their exact identifiers. Both versions describe the same behaviour, but only `spec.dai` names the identifiers.

| ID | spec.dai says | spec.md says |
|---|---|---|
| G6 | `PUT /api/config/routes`, `PUT /api/config/settings`, `PUT /api/config/routes/{path}/middleware` | "three further configuration resources … replace the routes, replace the system settings, and replace the middleware of a particular route identified by its path" |
| J3 | "carries an optional `expiresAt`" | "may carry an optional expiry time" |
| J4 | `{nonce, pkceVerifier, returnTo, expiresAt}` | "holds the nonce, the PKCE verifier, the path to return to … and an expiry time" |
| J5 | `{id, name, ownerSub, createdAt, lastUsedAt, revoked}` | "the key's id, a human-readable name, the subject identifier of its owner, the time it was created, the time it was last used, and a flag" |
| J6 | properties `id`, `userSub`, `csrfToken`, `createdAt`, `expiresAt` | "its id … the subject of the user … a CSRF token … the date and time it was created … expires" |
| J9 | properties `sub`, `email`, `name`, `role`, `createdAt`, `lastLoginAt` | "the subject … the email address … the display name … the role … created … most recent sign-in" |

Scored against these exact identifiers, spec_md drops from PASS to PARTIAL on all six. Its code uses `/api/config/system` and `/api/config/middleware?path=`, snake_case JSON fields, `subject` for `sub`/`userSub`, `owner` for `ownerSub`, and a `{v, exp}` value envelope. spec_dai uses the `spec.dai` names exactly. prompt_md's scores on these items don't change.
