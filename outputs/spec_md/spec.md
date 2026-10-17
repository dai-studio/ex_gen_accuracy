# Moxy Specification

Version 0.1.0. Target language Rust, built on the Pingora proxy framework for the server and SvelteKit for the web user interface.

## Overview

Moxy is a gateway server that runs locally on the machine of the person or team who uses it. It is written in Rust and sits between the people and programs that want to reach AI models and the AI model endpoints themselves. Those callers fall into three groups: developers, administrators and AI agents. Rather than every application having to know where each model is hosted and how to authenticate against it, callers send their traffic to Moxy, and Moxy forwards it to the right place.

Moxy learns which routes exist from a single configuration file called moxy.xml, which is written in an XML vocabulary named Moxy Markup Language. Each route declared in that file can carry its own middleware, which inspects and changes requests on the way out and responses on the way back, and each route names one or more upstream URLs to which matching traffic is forwarded. In addition to these configurable proxy routes, Moxy serves a fixed set of built-in routes of its own. The root path offers endpoint discovery, the chat path offers a browser chat interface, the admin path offers administration, the auth path handles sign-in, and the api path offers a JSON API that the user interface and scripts use.

All persistent state that Moxy keeps is held in one embedded key-value store that is organised according to the Single Table Design pattern. Version 0.1.0 of this specification contained several ambiguities. The decisions taken to remove them, and the interpretations of the original author's intent on which this version relies, are recorded in the decisions and assumptions sections at the end of this document.

## Who Uses Moxy

The first kind of caller is the developer. A developer wants to call models locally, which means reaching every configured model from their own code through one local address, without each of their applications needing to know where each model is hosted. A developer typically keeps using the software development kits they already know and simply points them at a Moxy route, authenticating with a Moxy API key instead of the model provider's key. Developers also browse the root page to discover which routes are available and use the chat page to try models out interactively.

The second kind of caller is the administrator. An administrator wants to configure models locally, which means deciding which routes exist, which upstreams each route uses, which middleware applies to each route, and what the server's system settings are. An administrator signs in through OpenID Connect using an email address that is listed as an administrator in the auth section of moxy.xml. Administrators can make their changes either through the admin user interface or by editing moxy.xml directly with a text editor. Administrators are also the people who issue API keys to developers and to agents.

The third kind of caller is the agent, meaning an automated client such as an AI agent. An agent wants to call configured models programmatically through Moxy, while staying within the rate limits and token budgets that have been set for its API key. An agent has no browser and never uses OpenID Connect. It authenticates with a Moxy API key that an administrator has issued to it, and it calls proxy routes and the endpoint discovery API.

## Dependencies

Moxy is implemented with the Rust language and toolchain, using the stable toolchain and the 2024 edition. Rust is a development dependency available from rust-lang.org and is licensed under MIT or Apache 2.0 at the user's choice.

The HTTP proxy layer is provided by Pingora, the open source proxy framework published by Cloudflare. Pingora is a runtime dependency licensed under Apache 2.0, and Moxy uses its latest stable release with the exact version pinned in the Cargo lock file. Pingora supplies the network listener, routing of requests to upstreams, load balancing and streaming.

The web user interface for the root page, the chat page and the admin page is built with SvelteKit, a runtime dependency licensed under MIT, using its latest stable release pinned in the npm package lock file. SvelteKit is used with its static adapter so that the interface is produced as a single-page application. The output of that build is embedded directly into the Rust binary and served by Moxy under the path prefix that begins with an underscore followed by app, so that no Node.js runtime is required when Moxy runs. The user interface obtains all of its data from Moxy's JSON API.

The key-value store is backed by redb, an embedded key-value engine written in Rust. It is a runtime dependency licensed under MIT or Apache 2.0, used at its latest stable release and pinned in the Cargo lock file.

## The Server Process

Moxy runs as a single server process, known in the specification as the serve service, at version 0.2.0. By default it is reachable at the base address of http 127.0.0.1 on port 8080, and its default authentication mode for proxy traffic is the API key. This one process owns the route table, the middleware pipeline, sessions, users, authentication and the cache.

When the process starts it performs its work in a fixed order. It first reads its environment variables. It then loads moxy.xml from the path given by the MOXY_CONFIG environment variable and validates it, and if the file is missing or invalid it logs the error and exits with a non-zero status rather than starting in a broken state. Next it opens the key-value store located in the directory given by MOXY_DATA_DIR. It then binds to the host and port given by the server element in moxy.xml, which default to 127.0.0.1 and 8080. Finally it begins watching moxy.xml for changes so that it can reload the configuration while running.

When the process receives an interrupt or terminate signal, it stops accepting new connections, allows requests that are already in flight to finish for at most thirty seconds, and then closes the key-value store.

Every error that Moxy produces itself, as opposed to an error response that comes back from an upstream, is returned as a JSON body containing a single object named error. That object has a field named type, whose value is a short machine-readable identifier written in snake case, and a field named message, whose value is a human-readable explanation. This gives callers a consistent way to recognise and handle Moxy's own failures programmatically.

## Environment Variables

Moxy reads exactly three environment variables directly, and only at startup, so changing any of them requires a restart. The MOXY_CONFIG variable gives the path to moxy.xml and defaults to a file named moxy.xml in the current directory. The MOXY_DATA_DIR variable gives the directory that holds the key-value store file; it defaults to a directory named data in the current directory, which is created if it does not already exist. The MOXY_LOG variable sets the logging level, which may be error, warn, info, debug or trace, and defaults to info.

Any other environment variable is used only when moxy.xml explicitly refers to it by name using the dollar-and-braces substitution syntax described below. This is the intended way to supply secrets such as upstream API keys and the OpenID Connect client secret. Environment variables never silently override anything in moxy.xml, and secrets must never be written literally into moxy.xml.

## The Configuration File

The file moxy.xml is the single source of truth for the configuration of the server. Its location is taken from MOXY_CONFIG and defaults to moxy.xml in the current directory. It is written in Moxy Markup Language, an XML vocabulary that lets an administrator compose a complete server configuration out of XML elements.

The file follows a defined lifecycle. It is loaded when the server starts, and the server refuses to start if the file is invalid. While the server runs, the file is watched for changes. When a change is detected, Moxy waits for a debounce period of five hundred milliseconds and then parses and validates the new contents. If the new configuration is valid, it is swapped in atomically, so that new requests are handled using the new configuration while requests already in flight finish using the old one. If the new configuration is invalid, the previous configuration remains active, and the error is written to the log and displayed in the admin interface. Changes made through the admin interface are written back into this same file by first validating them, then writing a temporary file, and then atomically renaming the temporary file over the original. That rename triggers the normal reload process, so edits made in the user interface and edits made by hand follow exactly the same path. Comments and formatting in the file are not guaranteed to survive an edit made through the admin interface.

Any attribute value anywhere in the file may contain a reference to an environment variable, written as a dollar sign followed by the variable name in curly braces. At load time each such reference is replaced with the value of the named environment variable. If the referenced variable is not set, the file fails validation.

Version one of the schema is structured as follows. The root element is named moxy and carries a version attribute whose value is one. Inside it there is a server element with a host attribute and a port attribute, for example a host of 127.0.0.1 and a port of 8080. There is also an auth element, which carries an issuer attribute giving the address of the OpenID Connect identity provider, a client-id attribute such as moxy, a client-secret attribute that should be supplied through a reference to an environment variable such as MOXY_OIDC_CLIENT_SECRET, and a session-ttl attribute giving the session lifetime, for example twenty-four hours. The auth element contains one or more admin elements, each carrying an email attribute that names a person who is to be treated as an administrator. After these, the root element contains any number of route elements. Each route element contains its middleware elements, written in the order in which they are to be executed, followed by exactly one destination, which is either a single upstream element with a url attribute or a load-balance element. A typical route might use the path prefix for OpenAI, the API key authentication mode, a description saying that it serves OpenAI models, and an upstream pointing at the version one OpenAI API address.

A route element accepts three attributes. The path attribute is required and is either a literal prefix that ends in a slash followed by an asterisk or an exact path. The auth attribute takes one of the values none, api-key or session, and defaults to api-key. The description attribute is free text that is displayed on the endpoint discovery page.

Validation rejects the file in several situations. It is invalid if two routes declare the same path. It is invalid if any route path is equal to the root path or falls under one of the reserved prefixes, which are chat, admin, auth, api and the underscore app prefix. It is invalid if a route has neither an upstream element nor a load-balance element. It is invalid if it contains an element the schema does not recognise. It is also invalid if any environment variable reference cannot be resolved. The middleware elements themselves are described in the middleware section of this document.

## Route Table and Matching

Every incoming request is matched against the route table in a fixed order of precedence. Built-in routes are considered first, and they are reserved. They consist of the root path matched exactly, the chat path and everything beneath it, the admin path and everything beneath it, everything beneath the auth path, everything beneath the api path, and everything beneath the underscore app path, which carries the static user interface assets. If no built-in route matches, the proxy routes declared in moxy.xml are considered, and these are handled by the catch-all proxy path. Among proxy routes, the one with the longest matching path prefix wins, and when two routes tie on length, the one declared first in moxy.xml wins. If nothing matches at all, Moxy responds with status 404 and a JSON error whose type is route_not_found.

Moxy recognises two roles. A user is anyone who has signed in through OpenID Connect. An administrator is a user whose email address is listed in an admin element within the auth element of moxy.xml. Administrators can do everything that ordinary users can do, in addition to the administrative functions.

## The Endpoint Discovery Page

The root path, requested with the GET method, serves an HTML page built with SvelteKit that anyone may view without signing in. It returns status 200 and lists the proxy routes contained in the current moxy.xml, showing for each one its path, its description and its authentication mode. It never reveals upstream URLs, headers or credentials. Exactly the same information is available in JSON form from the endpoints resource of the JSON API.

## The Chat Interface

The chat path, requested with the GET method, serves a browser chat interface built with SvelteKit for talking to configured models. It requires a signed-in session belonging to someone with the user or administrator role. A visitor without a session is redirected with status 302 to the login path of the auth routes, with a return_to parameter pointing back at the chat page, and a signed-in visitor receives status 200.

The model picker in the chat interface lists every proxy route that contains an ai middleware element. When the user sends a message, the interface submits it to the chat resource of the JSON API using the POST method. That request is passed through the full middleware pipeline of the chosen route, which means that rate limits and token budgets apply to the signed-in user exactly as they would to any other caller, and the model's reply is streamed back to the browser as server-sent events. Chat history is kept only in the browser and is never stored on the server.

## The Admin Interface

The admin path, requested with the GET method, serves a browser administration interface built with SvelteKit for configuring the server. It requires the administrator role. A signed-in person who is not an administrator receives status 403, while an anonymous visitor is redirected to the login path of the auth routes with a return_to parameter pointing back at the admin page. An administrator receives status 200.

Through this interface an administrator can add, change and delete proxy routes along with their upstreams. They can edit the system settings, which are the contents of the server element and the auth element. They can edit the middleware configuration of each route. They can create, list and revoke API keys. The interface also shows which version of the configuration is currently active and, if the most recent reload failed, the error that caused it to fail.

Every configuration change made through the admin interface is written back to moxy.xml in the manner described in the configuration file section. Nothing is configured solely in the key-value store, with the single exception of API keys.

## The Sign-In Routes

The routes beneath the auth path implement sign-in through OpenID Connect using the authorization code flow with Proof Key for Code Exchange, and they themselves require no prior authentication.

A GET request to the login path, optionally carrying a return_to parameter that names a path, creates a login state record and then redirects with status 302 to the OpenID Connect provider. When the provider sends the browser back, a GET request to the callback path validates the state and the nonce, exchanges the authorization code for tokens, creates or updates the user's record, creates a session, sets the session cookie, and finally redirects to the path given by return_to, or to the root path if none was given. If the state is invalid or has expired, the callback responds with status 400. A POST request to the logout path deletes the session, clears the cookie and redirects to the root path. For safety, only relative paths on the same origin are accepted as values of return_to.

## The JSON API

The routes beneath the api path form Moxy's own JSON API, which is used by the SvelteKit interface and by scripts. It is not the model proxy, because model traffic always travels through the catch-all proxy path instead.

Callers authenticate to this API either with the session cookie, which is how the browser interface does it, or with an Authorization header that carries a Moxy API key as a bearer token. The endpoints resource, read with GET, is available anonymously and returns the same data as the root page. The me resource, read with GET, requires the user role and returns the current caller's identity and role. The chat resource, called with POST, requires the user role and accepts a body naming the route path to use and the list of messages; it streams the reply back as server-sent events. The config resource, read with GET, requires the administrator role and returns the current configuration as JSON. Three further configuration resources, all written with PUT and all requiring the administrator role, replace the routes, replace the system settings, and replace the middleware of a particular route identified by its path. Each of these validates its input and then writes moxy.xml, and if the input is invalid it responds with status 422 together with the validation errors. The keys resource requires the administrator role and supports listing keys with GET, creating a key with POST, and deleting a key identified by its id with DELETE. The plaintext form of a newly created key is returned only once, in the response to the POST that created it, and can never be retrieved again.

When credentials are missing, the API responds with status 401, and when the caller's role is insufficient it responds with status 403. Any request that changes state and is authenticated by a session cookie must also send an X-CSRF-Token header containing the cross-site request forgery token stored in that session.

## The Proxy Path

The catch-all proxy path handles every request that does not match a built-in route and does match the path of a route element in moxy.xml, following the matching rules described earlier. Its default authentication mode is the API key.

For each request, Moxy first authenticates the caller according to the route's auth attribute. With the value none, no authentication is performed. With the value api-key, which is the default, the caller must send an Authorization header carrying a Moxy API key as a bearer token. With the value session, the caller must present a valid session cookie. If authentication fails, Moxy responds with status 401. Regardless of the mode, the caller's Authorization header and Moxy session cookie are always stripped from the request before it is forwarded, so that Moxy's own credentials never leak to an upstream. Any credentials the upstream needs are added instead by header middleware.

Moxy then runs the route's middleware pipeline. After that it forwards the request to the upstream. The portion of the request path that was matched by the asterisk is appended to the upstream URL, and the query string is preserved. For example, if a route has the path prefix for OpenAI and an upstream of the OpenAI version one API address, then a request to the chat completions path beneath the OpenAI prefix is forwarded to the chat completions path beneath the OpenAI version one address. Finally, the upstream's response is streamed back to the caller through the response phase of the middleware. Every HTTP method is proxied, and streaming responses, whether server-sent events or chunked transfers, are passed through without being buffered.

## Middleware

Middleware is configured per route, as child elements of the route element in moxy.xml. Middleware falls into seven categories that manipulate requests and responses: headers, request, response including caching, load balancing, AI-related processing, rate limiting, and token spend.

Ordering is precise. During the request phase, middleware runs in the order in which it appears in the document. During the response phase, it runs in the reverse of that order. The first middleware that rejects a request halts the pipeline, and in that case the upstream is never called.

The headers category is expressed with a headers element that contains set, add and remove children. A set child carries a name and a value and replaces the header, an add child carries a name and a value and adds the header, and a remove child carries a name and removes that header. By default these apply to the request sent to the upstream. To change the headers of the response instead, the same children are placed inside a response element.

The request category is expressed with a request element containing any of three children. A rewrite-path child with from and to attributes rewrites the request path. A max-body-size child with a bytes attribute limits the size of the request body, and a request that exceeds it is rejected with status 413. A timeout child with a duration attribute limits how long Moxy waits for the upstream, defaulting to sixty seconds, and a request that exceeds it is answered with status 504.

The response category is expressed with a response element, which may contain the same set, add and remove header children described above, as well as a cache child with a ttl attribute that enables the response cache described in the cache section.

The load balancing category is expressed with a load-balance element that carries a strategy attribute whose value is round-robin, random or failover, and that contains two or more upstream elements, each with a url. A load-balance element is used in place of a single upstream element. Whenever one of its upstreams returns an error or times out, that upstream is skipped for the following thirty seconds.

The AI-related category is expressed with an ai element that carries a provider attribute whose value is openai, anthropic or ollama, and an optional model attribute that overrides the requested model. This middleware parses the provider-specific request and response bodies, which allows the model name to be rewritten and token usage to be counted. It is required by the token budget middleware, and a route must contain it in order to appear in the chat interface.

The rate limit category is expressed with a rate-limit element whose requests attribute gives a number of permitted requests, whose per attribute gives the window as one second, one minute or one hour, and whose key attribute says whether requests are counted per API key, per user or per IP address. The counters are kept in memory and are reset whenever the server restarts. A request that exceeds the limit is rejected with status 429, a Retry-After header, and a JSON error of type rate_limited.

The token spend category is expressed with a token-budget element whose tokens attribute gives a number of permitted tokens, whose per attribute gives the window as one hour, one day or thirty days, and whose key attribute says whether usage is counted per API key or per user. Token usage is counted from the responses returned by the upstream and stored in the key-value store. Once the budget for the current window has been used up, further requests are rejected with status 429, a Retry-After header, and a JSON error of type token_budget_exceeded.

Failures in reaching the upstream are reported in a consistent way. If Moxy cannot connect to the upstream, it responds with status 502 and an error of type upstream_unavailable. If the upstream does not respond in time, Moxy responds with status 504 and an error of type upstream_timeout. If the upstream itself returns an HTTP error response, that response is passed through to the caller unchanged.

## Upstreams

Upstreams are not fixed in code, and any number of them may be configured. The specification illustrates this with two example model endpoints. The first is an AI model endpoint, such as an OpenAI-compatible API, declared with an upstream element in moxy.xml. The second shows that a single route can spread its traffic across several model endpoints by means of a load-balance element, and that different routes can point at different models.

Upstreams are not limited to AI models. Any HTTP or HTTPS address may be used, which means Moxy also functions as a general-purpose reverse proxy. On routes of that kind, the AI-specific middleware, namely the ai element and the token-budget element, is simply left out of the configuration.

## Authentication and API Keys

Authentication in Moxy combines an OpenID Connect integration with Moxy's own API keys, and it is configured through the auth element of moxy.xml. Two kinds of record are written to the key-value store in support of it.

The first is the login state record. Its key consists of the prefix auth, the word state and the state value, separated by hash characters. It holds the nonce, the Proof Key for Code Exchange verifier, the path to return to after sign-in, and an expiry time. It is created by the login route, deleted when the callback is handled, and expires after ten minutes in any case.

The second is the API key record. Its key consists of the prefix apikey followed by a hash character and the SHA-256 hash of the key. It holds the key's id, a human-readable name, the subject identifier of its owner, the time it was created, the time it was last used, and a flag saying whether it has been revoked. API keys take the form of the prefix moxy followed by an underscore and thirty-two random characters drawn from the URL-safe base64 alphabet. Only the hash of each key is ever stored, never the key itself.

API keys are intended for programmatic callers, such as developers' own code and AI agents. Every key belongs to the user who created it, who is always an administrator, and rate limits and token budgets configured per API key use the key as the counting key.

## Data Storage

The key-value store is the single place where Moxy keeps all of its persistent state. It is an embedded store powered by redb, held in one file named moxy.redb inside the directory given by MOXY_DATA_DIR. The application accesses it through a KvStore trait, so that the underlying engine can be replaced in future without changing the rest of the code.

The store follows Single Table Design, meaning that every kind of entity lives in one table and is distinguished by the prefix of its key. User records use the prefix user followed by the OpenID Connect subject. Session records use the prefix session followed by the session id. Login state records use the prefix auth, then state, then the state value. API key records use the prefix apikey followed by a SHA-256 hash. Cache entries use the prefix cache followed by a SHA-256 hash. Token budget counters use the prefix usage, followed by either the API key id or the user's subject, then the route path, then the start of the counting window, each part separated by a hash character. Every value is stored as JSON and may carry an optional expiry time. Entries that have expired are ignored when read and are deleted by a sweep that runs every ten minutes. Rate limit counters are deliberately not stored here, because they are kept in memory.

## Sessions

A session is the server-side record of a signed-in browser user. Its key is the prefix session followed by its id, where the id is thirty-two random bytes encoded in URL-safe base64. A session record holds its id as a string, the subject of the user it belongs to as a string, a cross-site request forgery token as a string, the date and time it was created, and the date and time it expires.

The session is identified to the browser by a cookie named moxy_session whose value is the session id. The cookie is marked HttpOnly, uses the Lax same-site policy, applies to the whole site from the root path, and is marked Secure unless the server is bound to 127.0.0.1 or localhost. A session lasts for the duration configured by the session-ttl attribute of the auth element, which defaults to twenty-four hours, and the lifetime is sliding. Whenever an authenticated request arrives more than one hour after the session was last refreshed, its expiry time is extended.

## Users

Moxy keeps one user record for each person who has ever signed in through OpenID Connect. The record is created on the person's first sign-in and updated on every subsequent sign-in. Its key is the prefix user followed by the OpenID Connect subject claim. The record holds the subject as a string, the email address as a string, the display name as a string, the role as a string, the date and time it was created, and the date and time of the most recent sign-in.

The role is computed at sign-in from the list of admin elements in the auth section of moxy.xml and is stored in the record. However, because the role is also checked again against the current configuration on every request, a change to the list of administrators takes effect on the user's very next request, without the user needing to sign in again.

## Cache

Moxy has an optional response cache that is off by default and is enabled per route by placing a cache element with a ttl attribute, for example five minutes, inside the route's response element.

The cache stores responses with status 200 to GET requests. It also stores responses with status 200 to POST requests on routes that contain an ai element, provided the response is not streamed. It never stores streamed responses, it ignores requests that carry a Cache-Control header of no-cache, and it never stores responses that carry a Cache-Control header of no-store or private.

Each cache entry is stored under the prefix cache followed by a SHA-256 hash computed over the route path, the method, the upstream URL, the normalised request body and the caller's key. Because the caller, whether identified by API key or by user, is part of the hash, one caller can never receive a response that was cached for a different caller. Entries expire once their ttl has passed, and all of the entries belonging to a route are discarded whenever that route changes in moxy.xml.

## Design Decisions

The following decisions were taken to resolve ambiguities in version 0.1.0 of the specification. A decision with the status accepted has been confirmed. A decision with the status proposed was taken while clarifying the specification and is awaiting confirmation from the owner.

The decision on Single Table Design has been accepted. Version 0.1.0 listed Single Table Design as though it were a dependency, but it is actually a data modelling pattern rather than a software package. The decision is that all entities share one key-value table and are distinguished by typed key prefixes, namely user, session, auth, apikey, cache and usage. As a consequence there is one storage abstraction and one file to back up, but the key formats become part of the contract and would require a migration if they were ever changed.

The decision that the configuration file is moxy.xml is proposed. Version 0.1.0 referred to the configuration file variously as moxy.xml, config.yaml and config.yml. The decision is that there is exactly one configuration file, moxy.xml, written in Moxy Markup Language, and that the port, routes, middleware, authentication and system settings all live in it. As a consequence YAML is not supported, and any reference to config.yaml or config.yml is to be read as meaning moxy.xml.

The decision to use an embedded key-value engine is proposed. Version 0.1.0 named a key-value store but no particular product, and Moxy runs locally as a single process. The decision is to use redb, an embedded Rust key-value store, held in one file under the data directory and accessed through a KvStore trait. As a consequence there is no external database to run, but Moxy is single-node only, and running several Moxy instances against the same store is not supported.

The decision that the admin interface persists its changes to moxy.xml is proposed. Version 0.1.0 did not say whether changes made in the admin interface were written back to moxy.xml or stored somewhere else. The decision is that the admin interface and the JSON API write moxy.xml atomically, after which the file watcher reloads it, so that moxy.xml remains the single source of truth, and that API keys are the only administrator-managed data kept in the key-value store. As a consequence edits made by hand and edits made in the interface can never drift apart, although comments and formatting in moxy.xml may be lost after an edit made in the interface.

The decision on route matching is proposed. Version 0.1.0 did not say how a catch-all request was mapped to an upstream, or how built-in routes and proxy routes interact. The decision is that built-in prefixes are reserved and checked first, that proxy routes are matched by longest prefix and then by declaration order, and that the suffix matched by the asterisk is appended to the upstream URL. As a consequence proxy routes cannot use the root path or the chat, admin, auth, api or underscore app prefixes, and this restriction is enforced when the configuration is validated.

The decision on middleware order and rejection is proposed. Version 0.1.0 named seven middleware categories but did not define their elements, their order, or what a rejection looks like. The decision is that the request phase runs in document order and the response phase in reverse order, and that rejections use fixed status codes, namely 401, 413, 429 together with a Retry-After header, 502 and 504, along with a JSON error body. As a consequence callers can handle Moxy's errors programmatically, and the set of elements is designated version one and can be extended later.

The decision on roles and API keys is proposed. Version 0.1.0 did not say which routes require authentication, what roles exist, how someone becomes an administrator, or how callers without a browser authenticate. The decision is that there are two roles, user for anyone who signs in through OpenID Connect and administrator for anyone whose email is listed in an admin element of the auth section, that the root page is anonymous, the chat page requires the user role, the admin page requires the administrator role, the JSON API decides per endpoint, and proxy routes decide per route with the API key as the default, and that programmatic callers use API keys issued by an administrator and sent as bearer tokens. As a consequence administrator rights are controlled in the configuration rather than in the user interface, and existing OpenAI-style software development kits work unchanged once their API key is set to a Moxy key.

The decision that SvelteKit is served by Moxy is proposed. Version 0.1.0 listed SvelteKit without saying which pages it renders or how it runs alongside a Rust server. The decision is that SvelteKit, using its static adapter, builds the root page, the chat page and the admin page as a single-page application whose assets are embedded in the binary and served under the underscore app prefix, with all data coming from the JSON API. As a consequence Moxy ships as one binary with no Node.js needed at runtime, and there is no server-side rendering.

The decision on environment variables versus configuration is proposed. Version 0.1.0 did not list any environment variables or explain how they relate to moxy.xml. The decision is that only MOXY_CONFIG, MOXY_DATA_DIR and MOXY_LOG are read directly, and that everything else comes from explicit environment variable references inside moxy.xml. As a consequence secrets stay out of the configuration file and there are no hidden overrides.

The decision on cache policy is proposed. Version 0.1.0 described a cache without saying what it held, how it was keyed, or when entries expired. The decision is that the cache is an opt-in, per-route response cache enabled with a ttl, keyed by both the request and the caller, and limited to non-streamed responses with status 200. As a consequence model output is never cached unexpectedly, and cache entries are never shared between callers.

## Assumptions

The following assumptions are interpretations of the original author's intent on which this specification depends. Each of them remains unvalidated until the owner confirms it.

It is assumed that the agent persona is an automated client. The basis for this is that version 0.1.0 named the persona and connected it to the server but gave it no role or goals. If this assumption is wrong and agents were meant to be AI agents hosted inside Moxy, then an agent runtime would need to be specified.

It is assumed that the notes for the auth routes in version 0.1.0 were a copy error. The basis for this is that in version 0.1.0 the notes for the auth routes were identical to those for the admin routes, while the authentication component uses OpenID Connect. If this assumption is wrong and the auth routes were meant to configure authentication providers, then that function belongs within the system settings of the admin interface.

It is assumed that the two model URLs shown in the original design are examples. The basis for this is that version 0.1.0 showed two hard-coded model URL nodes while also forwarding traffic to any URL. If this assumption is wrong and exactly two fixed model upstreams were intended, then the configuration schema could be made simpler.

It is assumed that the api routes form Moxy's own management API. The basis for this is that version 0.1.0 defined the api path without any description, that model traffic already has the catch-all proxy path, and that the SvelteKit interface needs an API from which to obtain its data. If this assumption is wrong and the api path was meant to be a unified model API, for example one compatible with OpenAI, then it would overlap with the proxy routes and would need a separate design.
