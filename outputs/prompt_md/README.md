# Moxy

Moxy is a gateway server that sits between callers (developers, admins and AI agents) and one or more AI model endpoints. It:

- loads its **routes from an XML configuration file**,
- runs **per-route middleware** on requests and responses (auth, rate limits, token quotas, CORS, header/path/body rewriting, model filtering, system prompts, PII redaction, caching, …),
- **forwards traffic to configured upstream URLs**, with failover, round-robin and passive health checks, and streams responses (SSE) straight through,
- ships **built-in routes** for administration, authentication, a browser chat UI, an OpenAI-compatible API, and endpoint discovery.

Moxy is written in TypeScript and needs Node.js ≥ 22.18. It has **no runtime dependencies**.

## Quick start

```sh
npm install          # dev dependencies only (typescript, @types/node)
npm run build        # compiles to dist/
npm test             # 155 unit + integration tests

# try it with the bundled mock model, no API keys needed
node examples/mock-llm.ts 9001 &
node dist/cli.js serve -c examples/moxy.xml
open http://127.0.0.1:8080/chat     # sign in as admin / admin
```

During development you can run the sources directly: `node src/cli.ts serve -c examples/moxy.xml`.

### CLI

```
moxy [serve] [-c moxy.xml] [--host HOST] [--port PORT]   start the gateway
moxy validate [-c moxy.xml]                             check a configuration file (incl. all middleware)
moxy hash-password [PASSWORD]                           print a password-hash for <user> (reads stdin if omitted)
moxy gen-key                                            generate an API key and its key-hash
moxy hash-key KEY                                       print the key-hash for an existing key
moxy version
```

The config path defaults to `$MOXY_CONFIG`, then `./moxy.xml`. `SIGHUP` or `POST /admin/api/reload` reloads it. `SIGINT`/`SIGTERM` shut down gracefully.

## Talking to Moxy

| Caller | Typical use |
| --- | --- |
| Developers | Point an SDK at a pass-through route (`/openai/*`) or at the unified API (`/api/v1`) with an API key or token. |
| AI agents | Read `/.well-known/moxy.json` to find endpoints and models, then call `/api/v1/chat/completions` with an API key. They can swap the key for a short-lived token via `/auth/token`. |
| Admins | Open `/admin` for routes, upstream health and per-caller token usage, and reload the config. |
| Browsers | `/chat` is a streaming chat UI over `/api/v1`, using a session cookie from `/auth/login`. |

Credentials, checked in this order:

1. `Authorization: Bearer <token-or-api-key>`, where the token comes from `/auth/login` or `/auth/token`
2. `X-API-Key: <api-key>`
3. the `moxy_session` cookie set by `/auth/login`. It is `HttpOnly` and `SameSite=Strict`, and is ignored on cross-origin state-changing requests.

```sh
# OpenAI SDKs work unchanged against the unified API:
export OPENAI_BASE_URL=http://127.0.0.1:8080/api/v1
export OPENAI_API_KEY=moxy_agent_dev_key_change_me
```

## Built-in routes

| Path | Description |
| --- | --- |
| `GET /healthz` | Liveness: `{status, version, uptime_s}`. Always on. |
| `GET /.well-known/moxy.json` | Discovery: endpoints, auth schemes, visible routes with their models, auth requirements and whether *you* can access them. |
| `GET /auth/login` | Login page (`?next=/path`). |
| `POST /auth/login` | JSON `{username, password}` returns `{token, expires_at, user}` and sets the session cookie. A form post redirects instead. Failed attempts are rate-limited per IP (10 per 5 minutes). |
| `POST /auth/logout` | Revokes the current token and clears the cookie. |
| `GET /auth/me` | The authenticated principal. |
| `POST /auth/token` | `{ttl?: "15m"}`: mints a token for the current principal (e.g. from an API key). It never outlives the configured TTL or the token it was minted from. |
| `GET /chat` | Chat UI. Requires login when users exist (`chat-auth`). |
| `GET /admin` | Admin dashboard (admin role). |
| `GET /admin/api/status` · `/routes` · `/stats` | Gateway status, routes with upstream health (header values redacted), and request/token counters by route, caller and model. |
| `POST /admin/api/reload` · `/stats/reset` | Reload the config file (the previous config stays live if the new one is invalid), or reset the counters. |
| `GET /api/v1/models` | OpenAI-format model list, filtered to models the caller may use. |
| `GET /api/v1/models/{id}` | One model. |
| `POST /api/v1/chat/completions` | Dispatched by `model` to the route that declares it, through **that route's middleware**. |
| `POST /api/v1/embeddings` | Same as above, using the route's embeddings path. |
| `GET /` | Landing page, unless a route claims `/`. |

Built-in prefixes take precedence over configured routes. A route that collides with an enabled built-in is a configuration error. Disable the built-in in `<builtins>` to reuse its path.

## Configuration reference

See [`examples/moxy.xml`](examples/moxy.xml) for a complete, commented example.

General rules:

- **Environment variables**: `${NAME}` expands to an environment variable, and startup fails if it is unset. `${NAME:-default}` supplies a fallback; `$${` is a literal `${`. Expansion applies to every attribute value and text node.
- **Durations**: `500ms`, `30s`, `5m`, `1h`, `1d`. A bare number means seconds.
- **Sizes**: `512kb`, `10mb`, `1g`. A bare number means bytes.
- **Lists**: separate items with commas and/or whitespace.
- **Strict checking**: unknown elements and attributes are errors, and every error message includes its line number.

```xml
<moxy>
  <server host="127.0.0.1" port="8080" trust-proxy="false" max-body="10mb"
          tls-cert="cert.pem" tls-key="key.pem"/>          <!-- TLS optional -->
  <logging level="info|debug|warn|error|silent" access="true" format="json|text"/>

  <auth secret="${MOXY_SECRET}" token-ttl="12h" cookie="moxy_session">
    <user name="alice" password-hash="scrypt$..." roles="admin"/>
    <user name="bob" password="${BOB_PASSWORD}" roles="developer"/>   <!-- plain text allowed, discouraged -->
    <api-key name="agent-1" key="${AGENT_KEY}" roles="agent"/>
    <api-key name="agent-2" key-hash="sha256:<hex>" roles="agent"/>
  </auth>

  <builtins admin="true" auth="true" chat="true" api="true" discovery="true"
            chat-auth="true" title="Moxy"/>

  <upstreams>                                   <!-- named, reusable upstreams -->
    <upstream name="openai" url="https://api.openai.com/v1" timeout="120s" weight="1">
      <header name="Authorization" value="Bearer ${OPENAI_API_KEY}"/>
    </upstream>
  </upstreams>

  <chains>                                      <!-- reusable middleware lists -->
    <chain name="members"><auth roles="developer,agent"/><rate-limit requests="60"/></chain>
  </chains>

  <middleware>...</middleware>                  <!-- global: runs first on every route -->

  <routes>
    <route id="openai" path="/openai/*" methods="GET,POST"
           strategy="failover|round-robin|random" retries="1" retry-on="error,502,503,504"
           preserve-host="false" hidden="false">
      <description>Shown in discovery</description>
      <upstream ref="openai"/>
      <upstream url="https://backup.example.com/v1" weight="2"><header name="..." value="..."/></upstream>
      <models chat-path="/chat/completions" embeddings-path="/embeddings">
        <model id="gpt-4o-mini" description="..."/>
        <model id="fast" upstream-model="gpt-4o-mini"/>   <!-- alias; rewritten in the body -->
      </models>
      <middleware>
        <use chain="members"/>
        <max-tokens limit="4096"/>
      </middleware>
    </route>
  </routes>
</moxy>
```

### Routes and forwarding

- **Path patterns**: `/exact` matches only that path. `/prefix/*` matches `/prefix` and everything below it. `/*` matches everything. Exact routes win over prefix routes, longer prefixes win over shorter ones, and remaining ties go to the route declared first. A route whose `methods` don't match falls through to the next candidate; if none matches, Moxy answers `405` with an `Allow` header. `OPTIONS` always matches so that CORS preflights work.
- **Target URL**: for a prefix route, Moxy appends the path below the prefix to the upstream URL (`/openai/chat/completions` → `https://api.openai.com/v1/chat/completions`). An exact route forwards to the upstream URL as written. The query string is always preserved.
- **Headers**:
  - Hop-by-hop headers are removed.
  - `Host` is set to the upstream's host unless `preserve-host` is set.
  - `X-Forwarded-For`, `-Proto` and `-Host` and `X-Request-Id` are added.
  - The Moxy session cookie is stripped.
  - The upstream's `<header>` values are applied last, so they override what the caller sent.
  - `Accept-Encoding: identity` is requested so Moxy can read token usage.
- **Failover**:
  - Moxy tries upstreams in strategy order; healthy ones go first.
  - `retries` defaults to the number of upstreams − 1.
  - `retry-on` takes `error` (connection error or timeout), `5xx`, `4xx` or explicit status codes.
  - After 2 consecutive failures an upstream is considered down for 15 s.
- **Streaming**: responses are streamed to the client as they arrive. If the client disconnects, the upstream request is aborted.
- **Usage tracking**: Moxy reads token usage from OpenAI, Anthropic and Ollama responses, both JSON and SSE/NDJSON streams. The counts feed `/admin` statistics and `<quota>`. For OpenAI streams, send `stream_options: {include_usage: true}` to get usage; the chat UI does this.
- **Unified API**: `/api/v1/chat/completions` picks the route that declares the requested model in `<models>` and forwards to that route's upstream URL plus `chat-path`. It runs the global middleware and the route's middleware exactly like a direct request.

### Middleware

Middleware runs in the order listed: global middleware first, then the route's. Request hooks run top to bottom. If one replies (a 401, 429, cache hit, …), the rest of the chain is skipped. Response hooks run bottom to top, and only for middleware that comes before the one that replied.

| Element | Attributes | Effect |
| --- | --- | --- |
| `<auth>` | `roles`, `optional=false`, `strip-credentials=true` | Requires a Moxy principal with one of `roles` (empty = any; `admin` passes everything). Returns 401 or 403. Moxy credentials are stripped before forwarding. |
| `<rate-limit>` | `requests`*, `window=1m`, `key=principal\|ip\|route\|global\|header:<name>` | Sliding-window limit. Adds `X-RateLimit-*` headers; returns 429 with `Retry-After`. Anonymous callers fall back to their IP. |
| `<quota>` | `tokens`*, `window=1d`, `key` | Token budget based on observed usage. Returns 429 `quota_exceeded`. Adds `X-Moxy-Quota-*` headers. |
| `<ip-filter>` | `allow`, `deny` (IPs/CIDRs, v4 and v6) | Returns 403 for denied addresses, or for addresses outside `allow`. |
| `<body-limit>` | `max`* | Returns 413 above the size. |
| `<cors>` | `origins=*` (globs), `methods`, `headers=*`, `expose`, `credentials=false`, `max-age=10m` | Answers preflights and adds CORS headers. |
| `<set-header>` | `name`*, `value` (or text), `phase=request\|response`, `override=true` | Sets a header. Values may use `{principal.id}`, `{principal.name}`, `{principal.roles}`, `{request.id}`, `{client.ip}`, `{route.id}`, `{model}`. |
| `<remove-header>` | `name`* (list), `phase` | Removes headers. |
| `<rewrite-path>` | `from`* (regex), `to` | Rewrites the path appended to the upstream URL. |
| `<timeout>` | `value`* | Upstream inactivity timeout for the route. The default is the upstream's `timeout`, else 120 s. |
| `<json-set>` | `path`* (dot path), `value`, `type=string\|number\|boolean\|json\|null`, `mode=set\|default` | Sets a field in JSON request bodies. |
| `<max-tokens>` | `limit`*, `default`, `fields`, `default-field=max_tokens` | Caps `max_tokens` / `max_completion_tokens` / `max_output_tokens`, and optionally sets a default. |
| `<model-filter>` | `allow`, `deny` (globs), `require=false` | Returns 403 `model_not_allowed`, or 400 if the model is required but missing. |
| `<system-prompt>` | text or `text`, `mode=prepend\|append\|replace`, `format=auto\|openai\|anthropic` | Injects a system instruction (OpenAI `messages` or Anthropic `system`). |
| `<redact>` | `patterns`, `action=replace\|block`, `replacement=[REDACTED:{name}]`; children `<pattern name flags>regex</pattern>` | Scrubs or blocks sensitive data in request bodies. Built-in patterns: `email`, `credit-card` (Luhn-checked), `ssn`, `phone`, `ipv4`, `api-key`, `aws-access-key`, `jwt`. Default set: email, credit-card, ssn, api-key, aws-access-key. |
| `<cache>` | `ttl=5m`, `max-entries=500`, `per-principal=true`, `methods=GET,POST` | LRU cache for successful non-streaming responses, keyed by route, path, query, body and caller. Adds `X-Moxy-Cache: HIT\|MISS`. |
| `<use>` | `chain`* | Inlines a `<chain>`. |

\* = required.

Errors produced by Moxy use the OpenAI error shape: `{"error": {"message", "type": "moxy_error", "code"}}`.

## Development

```
src/
  cli.ts            command-line entry point
  gateway.ts        HTTP server, request dispatch, reload, access log
  config.ts         XML → typed config (validation, ${ENV} interpolation)
  xml.ts            dependency-free XML parser
  router.ts         path/method matching
  pipeline.ts       middleware chain + response relay
  proxy.ts          upstream selection, failover, health, header rewriting
  auth.ts           scrypt passwords, HS256 tokens, API keys, revocation
  usage.ts          token-usage extraction from JSON / SSE / NDJSON
  stats.ts          in-memory counters
  middleware/       built-in middleware (access.ts, transform.ts, cache.ts)
  builtins/         admin, auth, api, chat/discovery pages
ui/                 HTML/CSS for login, chat, admin and index pages
examples/           example config + mock OpenAI-compatible model server
test/               node:test suites (unit + HTTP integration)
```

`npm run check` runs the type check (sources and tests), the build and the test suite.

To add a middleware, write a `MiddlewareFactory` (see `src/middleware/types.ts`) and register it in `src/middleware/index.ts`.
