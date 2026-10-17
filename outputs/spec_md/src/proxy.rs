//! The Pingora integration: built-in routes are answered directly, everything
//! else goes through the catch-all proxy path and the route's middleware.

use crate::api::{self, ApiRequest, ApiResponse, Body, MAX_API_BODY};
use crate::auth::{self, Caller};
use crate::config::{AuthMode, Config, HeaderOp, Route};
use crate::error::MoxyError;
use crate::middleware::ai::{AiRequestInfo, BodyObserver, transform_request};
use crate::middleware::{self, RequestInput, RequestPlan, budget, cache, lb};
use crate::routing;
use crate::state::{AppState, InflightGuard};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use http::{HeaderMap, HeaderName, Method};
use pingora::ErrorType;
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::prelude::*;
use pingora::proxy::{FailToProxy, ProxyHttp, Session};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

/// Bodies up to this size can be read before connecting upstream and replayed by Pingora.
const PREBUFFER_LIMIT: u64 = 64 * 1024;
/// Upper bound for bodies the ai middleware buffers in order to rewrite them.
const MAX_REWRITE_BODY: u64 = 32 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Headers the proxy manages itself and never copies from the middleware plan.
const MANAGED: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

pub struct MoxyProxy {
    pub state: Arc<AppState>,
}

enum BodyMode {
    /// Forward the body as it arrives.
    Passthrough,
    /// The body was read in the request filter and rewritten; send this instead.
    Prebuffered(Bytes),
    /// Buffer the whole body as it arrives and rewrite it at the end.
    BufferAll(BytesMut),
}

struct CacheState {
    key: String,
    ttl: Duration,
    capture: Option<Capture>,
}

struct Capture {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

pub struct ProxyCtx {
    _inflight: InflightGuard,
    route: Option<Arc<Route>>,
    caller: Caller,
    plan: Option<RequestPlan>,
    query: Option<String>,
    tried: Vec<usize>,
    selected: Option<usize>,
    body: BodyMode,
    body_seen: u64,
    prebuffer_sent_for: usize,
    ai_info: AiRequestInfo,
    cache: Option<CacheState>,
    observer: Option<BodyObserver>,
    response_done: bool,
    /// An error decided by a filter, reported by `fail_to_proxy`.
    reject: Option<MoxyError>,
}

fn client_ip(session: &Session) -> Option<IpAddr> {
    session
        .client_addr()
        .and_then(|a| a.as_inet())
        .map(|a| a.ip())
}

fn perr(context: &'static str) -> Box<pingora::Error> {
    pingora::Error::explain(ErrorType::InternalError, context)
}

/// Write an [`ApiResponse`] to the downstream.
pub async fn write_response(session: &mut Session, resp: ApiResponse, head: bool) -> Result<()> {
    let mut header = ResponseHeader::build(resp.status, Some(resp.headers.len() + 2))?;
    for (n, v) in resp.headers.iter() {
        header.append_header(n.clone(), v.clone())?;
    }
    match resp.body {
        Body::Full(bytes) => {
            header.insert_header("content-length", bytes.len().to_string())?;
            let end = bytes.is_empty() || head;
            session.write_response_header(Box::new(header), end).await?;
            if !end {
                session.write_response_body(Some(bytes), true).await?;
            }
        }
        Body::Stream(mut rx) => {
            header.insert_header("transfer-encoding", "chunked")?;
            session
                .write_response_header(Box::new(header), false)
                .await?;
            while let Some(chunk) = rx.recv().await {
                session.write_response_body(Some(chunk), false).await?;
            }
            session.write_response_body(None, true).await?;
        }
    }
    Ok(())
}

async fn write_error(session: &mut Session, err: &MoxyError) -> Result<()> {
    let head = session.req_header().method == Method::HEAD;
    write_response(session, ApiResponse::error(err), head).await
}

enum ReadError {
    TooLarge,
    Io(Box<pingora::Error>),
}

async fn read_body(session: &mut Session, limit: u64) -> std::result::Result<Bytes, ReadError> {
    let mut buf = BytesMut::new();
    loop {
        match session.read_request_body().await {
            Ok(Some(chunk)) => {
                if (buf.len() + chunk.len()) as u64 > limit {
                    return Err(ReadError::TooLarge);
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(buf.freeze()),
            Err(e) => return Err(ReadError::Io(e)),
        }
    }
}

fn apply_to_response(op: &HeaderOp, resp: &mut ResponseHeader) -> Result<()> {
    match op {
        HeaderOp::Set(n, v) => resp.insert_header(n.clone(), v.clone()).map(|_| ()),
        HeaderOp::Add(n, v) => resp.append_header(n.clone(), v.clone()).map(|_| ()),
        HeaderOp::Remove(n) => {
            resp.remove_header(n);
            Ok(())
        }
    }
}

/// Make `upstream`'s end-to-end headers equal to `want`, leaving managed headers alone.
fn sync_headers(upstream: &mut RequestHeader, want: &HeaderMap) -> Result<()> {
    let managed = |n: &HeaderName| MANAGED.contains(&n.as_str());
    let existing: Vec<HeaderName> = upstream.headers.keys().cloned().collect();
    for name in existing {
        if !managed(&name) && !want.contains_key(&name) {
            upstream.remove_header(&name);
        }
    }
    for name in want.keys() {
        if managed(name) {
            continue;
        }
        let current: Vec<_> = upstream.headers.get_all(name).iter().cloned().collect();
        let desired: Vec<_> = want.get_all(name).iter().cloned().collect();
        if current != desired {
            upstream.remove_header(name);
            for v in desired {
                upstream.append_header(name.clone(), v)?;
            }
        }
    }
    Ok(())
}

/// Map a Pingora failure to Moxy's error vocabulary.
fn map_error(e: &pingora::Error) -> Option<MoxyError> {
    use pingora::ErrorSource;
    use pingora::ErrorType::*;
    match e.etype() {
        HTTPStatus(413) => return Some(MoxyError::payload_too_large(0)),
        HTTPStatus(code) => {
            return Some(MoxyError::new(
                http::StatusCode::from_u16(*code).unwrap_or(http::StatusCode::BAD_GATEWAY),
                "proxy_error",
                e.to_string(),
            ));
        }
        _ => {}
    }
    match e.esource() {
        ErrorSource::Downstream => match e.etype() {
            ReadError | WriteError | ConnectionClosed => None,
            _ => Some(MoxyError::bad_request("malformed request")),
        },
        ErrorSource::Upstream => Some(match e.etype() {
            ConnectTimedout | ReadTimedout | WriteTimedout | TLSHandshakeTimedout => {
                MoxyError::upstream_timeout()
            }
            _ => MoxyError::upstream_unavailable(format!(
                "the upstream could not be reached: {}",
                e.etype().as_str()
            )),
        }),
        _ => Some(match e.etype() {
            ConnectTimedout => MoxyError::upstream_timeout(),
            ConnectRefused | ConnectNoRoute | ConnectError | TLSHandshakeFailure | InvalidCert
            | HandshakeError | SocketError => MoxyError::upstream_unavailable(format!(
                "the upstream could not be reached: {}",
                e.etype().as_str()
            )),
            _ => MoxyError::internal("internal proxy error"),
        }),
    }
}

impl MoxyProxy {
    async fn handle_builtin(
        &self,
        session: &mut Session,
        path: String,
        query: Option<String>,
    ) -> Result<bool> {
        let method = session.req_header().method.clone();
        let headers = session.req_header().headers.clone();
        let body = match read_body(session, MAX_API_BODY as u64).await {
            Ok(b) => b,
            Err(ReadError::TooLarge) => {
                write_error(session, &MoxyError::payload_too_large(MAX_API_BODY as u64)).await?;
                return Ok(true);
            }
            Err(ReadError::Io(e)) => return Err(e),
        };
        let req = ApiRequest {
            method: method.clone(),
            path,
            query,
            headers,
            body,
            client_ip: client_ip(session),
        };
        let resp = api::handle(&self.state, req).await;
        write_response(session, resp, method == Method::HEAD).await?;
        Ok(true)
    }

    fn authenticate(
        &self,
        config: &Config,
        route: &Route,
        headers: &HeaderMap,
    ) -> std::result::Result<Caller, MoxyError> {
        let kv = self.state.kv.as_ref();
        match route.auth {
            AuthMode::None => Ok(Caller::Anonymous),
            AuthMode::ApiKey => auth::authenticate_api_key(kv, headers)?.ok_or_else(|| {
                MoxyError::unauthorized("a valid Moxy API key is required as a bearer token")
            }),
            AuthMode::Session => auth::authenticate_session(kv, config, headers)?
                .ok_or_else(|| MoxyError::unauthorized("a valid session is required")),
        }
    }

    /// The request phase for a proxy route. Returns `Ok(true)` if a response was already sent.
    async fn handle_proxy(
        &self,
        session: &mut Session,
        ctx: &mut ProxyCtx,
        path: String,
    ) -> Result<bool> {
        let config = self.state.config();
        let Some((route, suffix)) = config.match_route(&path) else {
            write_error(session, &MoxyError::route_not_found()).await?;
            return Ok(true);
        };
        let suffix = suffix.to_string();
        let headers = session.req_header().headers.clone();
        let method = session.req_header().method.clone();

        let caller = match self.authenticate(&config, &route, &headers) {
            Ok(c) => c,
            Err(e) => {
                let e = if route.auth == AuthMode::ApiKey {
                    e.with_header("www-authenticate", "Bearer")
                } else {
                    e
                };
                write_error(session, &e).await?;
                return Ok(true);
            }
        };

        let content_length = headers
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let chunked = headers
            .get(http::header::TRANSFER_ENCODING)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));

        let plan = match middleware::run_request_phase(
            RequestInput {
                route: &route,
                caller: &caller,
                client_ip: client_ip(session),
                headers: &headers,
                suffix: &suffix,
                content_length,
            },
            &self.state.limiter,
            self.state.kv.as_ref(),
        ) {
            Ok(p) => p,
            Err(e) => {
                write_error(session, &e).await?;
                return Ok(true);
            }
        };

        // The ai middleware needs the whole body to rewrite the model, request usage or key the cache.
        let has_body = content_length.is_some_and(|l| l > 0) || chunked;
        if let Some(ai) = plan.ai.as_ref().filter(|_| has_body) {
            let ensure_usage = !plan.budget_counters.is_empty();
            if ai.model.is_some() || ensure_usage || plan.cache_ttl.is_some() {
                let limit = plan
                    .max_body
                    .unwrap_or(MAX_REWRITE_BODY)
                    .min(MAX_REWRITE_BODY);
                match content_length {
                    Some(len) if len <= PREBUFFER_LIMIT => {
                        // Pingora replays bodies of this size from its retry buffer.
                        session.enable_retry_buffering();
                        let body = match read_body(session, limit).await {
                            Ok(b) => b,
                            Err(ReadError::TooLarge) => {
                                write_error(session, &MoxyError::payload_too_large(limit)).await?;
                                return Ok(true);
                            }
                            Err(ReadError::Io(e)) => return Err(e),
                        };
                        let (new_body, info) = transform_request(ai, &body, ensure_usage);
                        ctx.ai_info = info;
                        ctx.body = BodyMode::Prebuffered(new_body);
                    }
                    _ => ctx.body = BodyMode::BufferAll(BytesMut::new()),
                }
            }
        }

        // Response cache lookup.
        if let Some(ttl) = plan.cache_ttl {
            let body_key = match &ctx.body {
                BodyMode::Prebuffered(b) => Some(b.clone()),
                _ if !has_body => Some(Bytes::new()),
                _ => None,
            };
            let cacheable = !cache::request_bypasses(&headers)
                && (method == Method::GET
                    || (method == Method::POST && route.ai().is_some() && !ctx.ai_info.stream));
            if let (true, Some(body_key)) = (cacheable, body_key) {
                let query = session.req_header().uri.query().map(str::to_string);
                let upstream: Vec<String> = route
                    .destination
                    .upstreams()
                    .iter()
                    .map(|u| lb::upstream_url(u, &plan.suffix, query.as_deref()))
                    .collect();
                let key = cache::cache_key(
                    route.path(),
                    method.as_str(),
                    &upstream.join(" "),
                    &body_key,
                    &caller.cache_identity(),
                );
                match cache::lookup(self.state.kv.as_ref(), &key) {
                    Ok(Some(entry)) => {
                        self.serve_cached(session, &route, entry, method == Method::HEAD)
                            .await?;
                        return Ok(true);
                    }
                    Ok(None) => {}
                    Err(e) => log::warn!("cache lookup failed: {e:#}"),
                }
                ctx.cache = Some(CacheState {
                    key,
                    ttl,
                    capture: None,
                });
            }
        }

        ctx.route = Some(route);
        ctx.caller = caller;
        ctx.plan = Some(plan);
        Ok(false)
    }

    async fn serve_cached(
        &self,
        session: &mut Session,
        route: &Route,
        entry: cache::CacheEntry,
        head: bool,
    ) -> Result<()> {
        let body = entry.body_bytes();
        let mut header = ResponseHeader::build(entry.status, Some(entry.headers.len() + 3))?;
        for (n, v) in &entry.headers {
            header.append_header(n.clone(), v.as_str())?;
        }
        for op in route.response_header_ops() {
            apply_to_response(op, &mut header)?;
        }
        header.insert_header("x-moxy-cache", "HIT")?;
        header.insert_header("content-length", body.len().to_string())?;
        let end = body.is_empty() || head;
        session.write_response_header(Box::new(header), end).await?;
        if !end {
            session.write_response_body(Some(body), true).await?;
        }
        Ok(())
    }

    fn selected_upstream(&self, ctx: &ProxyCtx) -> Option<url::Url> {
        let route = ctx.route.as_ref()?;
        route.destination.upstreams().get(ctx.selected?).cloned()
    }
}

#[async_trait]
impl ProxyHttp for MoxyProxy {
    type CTX = ProxyCtx;

    fn new_ctx(&self) -> Self::CTX {
        ProxyCtx {
            _inflight: self.state.track_request(),
            route: None,
            caller: Caller::Anonymous,
            plan: None,
            query: None,
            tried: Vec::new(),
            selected: None,
            body: BodyMode::Passthrough,
            body_seen: 0,
            prebuffer_sent_for: 0,
            ai_info: AiRequestInfo::default(),
            cache: None,
            observer: None,
            response_done: false,
            reject: None,
        }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let path = session.req_header().uri.path().to_string();
        let query = session.req_header().uri.query().map(str::to_string);
        if routing::classify(&path).is_some() {
            return self.handle_builtin(session, path, query).await;
        }
        ctx.query = query;
        self.handle_proxy(session, ctx, path).await
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let route = ctx.route.clone().ok_or_else(|| perr("no route selected"))?;
        let timeout = ctx
            .plan
            .as_ref()
            .map(|p| p.timeout)
            .unwrap_or(crate::config::DEFAULT_TIMEOUT);
        let Some(idx) = self.state.balancer.select(&route, &ctx.tried) else {
            ctx.reject = Some(MoxyError::upstream_unavailable("no upstream is available"));
            return Err(perr("no upstream available"));
        };
        ctx.tried.push(idx);
        ctx.selected = Some(idx);
        let url = &route.destination.upstreams()[idx];
        let tls = url.scheme() == "https";
        let port = url
            .port_or_known_default()
            .unwrap_or(if tls { 443 } else { 80 });
        let (addr, sni) = match url.host() {
            Some(url::Host::Ipv4(ip)) => (SocketAddr::new(IpAddr::V4(ip), port), ip.to_string()),
            Some(url::Host::Ipv6(ip)) => (SocketAddr::new(IpAddr::V6(ip), port), ip.to_string()),
            Some(url::Host::Domain(name)) => {
                let resolved = tokio::net::lookup_host((name, port))
                    .await
                    .ok()
                    .and_then(|mut a| a.next());
                match resolved {
                    Some(a) => (a, name.to_string()),
                    None => {
                        self.state.balancer.mark_failed(url);
                        ctx.reject = Some(MoxyError::upstream_unavailable(format!(
                            "cannot resolve upstream host {name}"
                        )));
                        return Err(pingora::Error::explain(
                            ErrorType::ConnectNoRoute,
                            "upstream host not resolvable",
                        ));
                    }
                }
            }
            None => return Err(perr("upstream has no host")),
        };
        let mut peer = HttpPeer::new(addr, tls, sni);
        peer.options.connection_timeout = Some(timeout.min(CONNECT_TIMEOUT));
        peer.options.total_connection_timeout = Some(timeout.min(CONNECT_TIMEOUT * 2));
        peer.options.read_timeout = Some(timeout);
        peer.options.write_timeout = Some(timeout);
        Ok(Box::new(peer))
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut e: Box<pingora::Error>,
    ) -> Box<pingora::Error> {
        if let Some(url) = self.selected_upstream(ctx) {
            log::warn!("cannot connect to upstream {url}: {e}");
            self.state.balancer.mark_failed(&url);
        }
        if let Some(route) = &ctx.route {
            if self.state.balancer.has_alternative(route, &ctx.tried) {
                ctx.reject = None;
                e.set_retry(true);
            } else {
                e.set_retry(false);
            }
        }
        e
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let (Some(plan), Some(base)) = (ctx.plan.as_ref(), self.selected_upstream(ctx)) else {
            return Err(perr("no upstream selected"));
        };
        let path = lb::upstream_path(&base, &plan.suffix, ctx.query.as_deref());
        let uri: http::Uri = path.parse().map_err(|_| perr("invalid upstream path"))?;
        upstream.set_uri(uri);
        sync_headers(upstream, &plan.headers)?;
        upstream.insert_header("host", lb::authority(&base))?;
        match &ctx.body {
            BodyMode::Passthrough => {}
            BodyMode::Prebuffered(b) => {
                upstream.remove_header("transfer-encoding");
                upstream.insert_header("content-length", b.len().to_string())?;
            }
            BodyMode::BufferAll(_) => {
                upstream.remove_header("content-length");
                upstream.insert_header("transfer-encoding", "chunked")?;
            }
        }
        Ok(())
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let max = ctx.plan.as_ref().and_then(|p| p.max_body);
        match &mut ctx.body {
            BodyMode::Prebuffered(b) => {
                // Replace the replayed original body with the rewritten one, once per attempt.
                if ctx.prebuffer_sent_for != ctx.tried.len() {
                    ctx.prebuffer_sent_for = ctx.tried.len();
                    *body = Some(b.clone());
                } else if body.is_some() {
                    *body = Some(Bytes::new());
                }
            }
            BodyMode::Passthrough => {
                if let (Some(chunk), Some(max)) = (body.as_ref(), max) {
                    if ctx.tried.len() <= 1 {
                        ctx.body_seen += chunk.len() as u64;
                    }
                    if ctx.body_seen > max {
                        ctx.reject = Some(MoxyError::payload_too_large(max));
                        return Err(pingora::Error::explain(
                            ErrorType::HTTPStatus(413),
                            "request body too large",
                        ));
                    }
                }
            }
            BodyMode::BufferAll(buf) => {
                if let Some(chunk) = body.take() {
                    let limit = max.unwrap_or(MAX_REWRITE_BODY).min(MAX_REWRITE_BODY);
                    if (buf.len() + chunk.len()) as u64 > limit {
                        ctx.reject = Some(MoxyError::payload_too_large(limit));
                        return Err(pingora::Error::explain(
                            ErrorType::HTTPStatus(413),
                            "request body too large",
                        ));
                    }
                    buf.extend_from_slice(&chunk);
                }
                if end_of_stream {
                    let ensure_usage = ctx
                        .plan
                        .as_ref()
                        .is_some_and(|p| !p.budget_counters.is_empty());
                    let whole = std::mem::take(buf).freeze();
                    match ctx.plan.as_ref().and_then(|p| p.ai.as_ref()) {
                        Some(ai) => {
                            let (new_body, info) = transform_request(ai, &whole, ensure_usage);
                            ctx.ai_info = info;
                            *body = Some(new_body);
                        }
                        None => *body = Some(whole),
                    }
                } else {
                    // An empty chunk keeps the upstream body open without sending anything.
                    *body = Some(Bytes::new());
                }
            }
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        resp: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let Some(route) = ctx.route.clone() else {
            return Ok(());
        };
        let status = resp.status.as_u16();
        if status >= 500
            && let Some(url) = self.selected_upstream(ctx)
        {
            self.state.balancer.mark_failed(&url);
        }
        let content_type = resp
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let streamed = cache::is_streamed_content_type(&content_type) || ctx.ai_info.stream;

        if let Some(c) = ctx.cache.as_mut()
            && cache::response_storable(status, &resp.headers, streamed)
        {
            c.capture = Some(Capture {
                status,
                headers: cache::storable_headers(&resp.headers),
                body: Vec::new(),
            });
        }
        if let Some(ai) = route.ai()
            && (200..300).contains(&status)
        {
            ctx.observer = Some(BodyObserver::new(ai.provider, &content_type));
        }
        for op in route.response_header_ops() {
            apply_to_response(op, resp)?;
        }
        if ctx.cache.is_some() {
            resp.insert_header("x-moxy-cache", "MISS")?;
        }
        Ok(())
    }

    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        if let Some(chunk) = body.as_ref() {
            if let Some(obs) = ctx.observer.as_mut() {
                obs.feed(chunk);
            }
            if let Some(c) = ctx.cache.as_mut() {
                let too_big = c
                    .capture
                    .as_ref()
                    .is_some_and(|cap| cap.body.len() + chunk.len() > cache::MAX_CACHED_BODY);
                if too_big {
                    c.capture = None;
                } else if let Some(cap) = c.capture.as_mut() {
                    cap.body.extend_from_slice(chunk);
                }
            }
        }
        if end_of_stream {
            ctx.response_done = true;
        }
        Ok(None)
    }

    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        e: &pingora::Error,
        ctx: &mut Self::CTX,
    ) -> FailToProxy {
        if matches!(
            e.etype(),
            ErrorType::ReadTimedout | ErrorType::ConnectTimedout
        ) && let Some(url) = self.selected_upstream(ctx)
        {
            self.state.balancer.mark_failed(&url);
        }
        let err = ctx.reject.take().or_else(|| map_error(e));
        // The connection is not reused after a failure, so say so in the response.
        session.set_keepalive(None);
        let code = match err {
            Some(err) => {
                if session.response_written().is_none()
                    && let Err(we) = write_error(session, &err).await
                {
                    log::debug!("could not write error response: {we}");
                }
                err.status.as_u16()
            }
            None => 0,
        };
        FailToProxy {
            error_code: code,
            can_reuse_downstream: false,
        }
    }

    async fn logging(
        &self,
        _session: &mut Session,
        e: Option<&pingora::Error>,
        ctx: &mut Self::CTX,
    ) {
        let Some(route) = ctx.route.clone() else {
            return;
        };
        // Token accounting from whatever usage the response reported.
        if let Some(obs) = ctx.observer.take() {
            let counters = ctx
                .plan
                .as_ref()
                .map(|p| p.budget_counters.clone())
                .unwrap_or_default();
            if let Some(usage) = obs.finish()
                && !counters.is_empty()
                && let Err(err) = budget::record(
                    self.state.kv.as_ref(),
                    route.path(),
                    &counters,
                    usage.total_tokens,
                )
            {
                log::warn!("failed to record token usage: {err:#}");
            }
        }
        // Store a completed, cacheable response.
        if e.is_none()
            && ctx.response_done
            && let Some(CacheState {
                key,
                ttl,
                capture: Some(cap),
            }) = ctx.cache.take()
            && let Err(err) = cache::store(
                self.state.kv.as_ref(),
                &key,
                route.path(),
                cap.status,
                cap.headers,
                &cap.body,
                ttl,
            )
        {
            log::warn!("failed to store cache entry: {err:#}");
        }
    }
}
