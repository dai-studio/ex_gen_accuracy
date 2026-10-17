//! The Pingora proxy: built-in routes are answered in `request_filter`; everything else
//! is matched against the proxy routes from moxy.xml and forwarded to an upstream.

use crate::ai::{self, UsageTracker};
use crate::auth::{self, Identity};
use crate::builtin;
use crate::config::{AuthMode, HeaderOpR, RouteSpec};
use crate::error::MoxyError;
use crate::http::{Body, Req, Resp};
use crate::pipeline::{self, Caller, Plan, RequestInfo};
use crate::state::{AppState, InflightGuard};
use crate::util::strip_cookie;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use pingora::prelude::*;
use pingora::proxy::FailToProxy;
use pingora::{ErrorSource, ErrorType};
use pingora::http::ResponseHeader;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

/// Bodies up to this size can be read before connecting (Pingora replays them from its
/// retry buffer, which is 64 KiB). Needed for cache keys of POST requests.
const PREREAD_LIMIT: u64 = 64 * 1024;
/// Largest response body kept for the cache.
const MAX_CACHE_BODY: usize = 8 * 1024 * 1024;

pub struct MoxyProxy {
    pub state: Arc<AppState>,
}

#[derive(Default)]
pub struct Ctx {
    guard: Option<InflightGuard>,
    route: Option<Arc<RouteSpec>>,
    plan: Option<Plan>,
    query: Option<String>,
    tried: Vec<usize>,
    current: Option<usize>,
    /// Full replacement body (pre-read and rewritten).
    replacement_body: Option<Bytes>,
    /// Buffer the request body and rewrite it at the end of the stream.
    buffer_body: bool,
    body_buf: BytesMut,
    body_seen: u64,
    request_streamed: bool,
    usage: Option<UsageTracker>,
    cache_key: Option<String>,
    cache_buf: Option<Vec<u8>>,
    cache_status: u16,
    cache_headers: Vec<(String, String)>,
    /// Moxy's own error to report if the request fails.
    reject: Option<MoxyError>,
}

fn client_ip(session: &Session) -> Option<IpAddr> {
    session
        .client_addr()
        .and_then(|a| a.as_inet())
        .map(|a| a.ip())
}

fn header<'a>(session: &'a Session, name: &str) -> Option<&'a str> {
    session
        .req_header()
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
}

/// Write a complete Moxy-generated response to the client.
pub async fn write_resp(session: &mut Session, resp: Resp) -> Result<()> {
    let head = session.req_header().method == http::Method::HEAD;
    let mut h = ResponseHeader::build(resp.status, None)?;
    for (k, v) in &resp.headers {
        h.append_header(k.clone(), v.as_str())?;
    }
    match resp.body {
        Body::Full(b) => {
            h.insert_header("content-length", b.len().to_string())?;
            let end = b.is_empty() || head;
            session.write_response_header(Box::new(h), end).await?;
            if !end {
                session.write_response_body(Some(b), true).await?;
            }
        }
        Body::Stream(mut rx) => {
            h.insert_header("transfer-encoding", "chunked")?;
            session.write_response_header(Box::new(h), false).await?;
            while let Some(chunk) = rx.recv().await {
                if session.write_response_body(Some(chunk), false).await.is_err() {
                    // Client went away; dropping rx stops the producer.
                    return Ok(());
                }
            }
            session.write_response_body(None, true).await?;
        }
    }
    Ok(())
}

async fn write_error(session: &mut Session, e: &MoxyError) -> Result<()> {
    write_resp(session, Resp::error(e)).await
}

/// Read the whole request body, up to `limit` bytes.
async fn read_body(session: &mut Session, limit: usize) -> std::result::Result<Bytes, MoxyError> {
    let mut buf = BytesMut::new();
    loop {
        match session.read_request_body().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > limit {
                    return Err(MoxyError::payload_too_large(limit as u64));
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(buf.freeze()),
            Err(e) => return Err(MoxyError::bad_request(format!("failed to read body: {e}"))),
        }
    }
}

fn map_error(e: &Error) -> MoxyError {
    match e.etype() {
        ErrorType::HTTPStatus(413) => MoxyError::new(413, "payload_too_large", "request body too large"),
        ErrorType::HTTPStatus(code) => MoxyError::new(*code, "error", "request failed"),
        ErrorType::ConnectTimedout | ErrorType::ReadTimedout | ErrorType::WriteTimedout => {
            MoxyError::upstream_timeout("upstream timed out")
        }
        _ => match e.esource() {
            ErrorSource::Upstream => MoxyError::upstream_unavailable("upstream connection failed"),
            ErrorSource::Downstream => match e.etype() {
                ErrorType::ReadError | ErrorType::WriteError | ErrorType::ConnectionClosed => {
                    MoxyError::new(0, "client_closed", "client closed the connection")
                }
                _ => MoxyError::bad_request("bad request"),
            },
            _ => MoxyError::internal("internal error"),
        },
    }
}

fn is_timeout(e: &Error) -> bool {
    matches!(
        e.etype(),
        ErrorType::ConnectTimedout | ErrorType::ReadTimedout | ErrorType::WriteTimedout
    )
}

fn reject_err(ctx: &mut Ctx, e: MoxyError) -> Box<Error> {
    let status = e.status;
    ctx.reject = Some(e);
    Error::explain(ErrorType::HTTPStatus(status), "rejected by moxy")
}

impl MoxyProxy {
    async fn authenticate(
        &self,
        session: &Session,
        route: &RouteSpec,
    ) -> std::result::Result<Option<Identity>, MoxyError> {
        match route.auth {
            AuthMode::None => Ok(None),
            AuthMode::ApiKey => auth::authenticate_api_key(&self.state, header(session, "authorization"))?
                .map(Some)
                .ok_or_else(|| MoxyError::unauthorized("missing API key (Authorization: Bearer <moxy key>)")),
            AuthMode::Session => auth::authenticate_session(&self.state, header(session, "cookie"))?
                .map(Some)
                .ok_or_else(|| MoxyError::unauthorized("sign in required")),
        }
    }

    /// Returns Ok(true) if a response has been sent.
    async fn prepare_proxy(&self, session: &mut Session, ctx: &mut Ctx) -> std::result::Result<bool, MoxyError> {
        let path = session.req_header().uri.path().to_string();
        let method = session.req_header().method.as_str().to_string();
        let active = self.state.active();
        let (route, suffix) = active
            .resolved
            .match_route(&path)
            .ok_or_else(|| MoxyError::route_not_found(&path))?;
        let identity = self.authenticate(session, &route).await?;
        let ip = client_ip(session);
        let caller = Caller {
            identity: identity.as_ref(),
            ip,
        };
        let content_length = header(session, "content-length").and_then(|v| v.trim().parse::<u64>().ok());
        let chunked = header(session, "transfer-encoding").is_some();
        let plan = pipeline::request_phase(
            &self.state,
            &route,
            RequestInfo {
                suffix,
                content_length,
                caller,
            },
        )?;

        let mut cache_ok = pipeline::request_cacheable(&method, &route, header(session, "cache-control"));
        let has_body = content_length.is_some_and(|l| l > 0) || chunked;
        let mut key_body = Bytes::new();
        if let Some(ai_spec) = &route.ai {
            if has_body {
                let rewrite = |b: &[u8]| {
                    ai::rewrite_request(ai_spec.provider, b, plan.model_override.as_deref(), route.has_token_budget)
                };
                if content_length.is_some_and(|l| l <= PREREAD_LIMIT) && !chunked {
                    // Small body: read it now so it can be rewritten and used in the cache key.
                    session.enable_retry_buffering();
                    let body = read_body(session, PREREAD_LIMIT as usize).await?;
                    ctx.request_streamed = ai::wants_stream(&body);
                    key_body = match rewrite(&body) {
                        Some(new) => {
                            let new = Bytes::from(new);
                            ctx.replacement_body = Some(new.clone());
                            new
                        }
                        None => body,
                    };
                } else {
                    // Large or chunked body: rewrite while streaming; not cacheable.
                    ctx.buffer_body = plan.model_override.is_some() || route.has_token_budget;
                    cache_ok = false;
                }
            }
        } else if has_body {
            cache_ok = false;
        }

        ctx.query = session.req_header().uri.query().map(str::to_string);
        if cache_ok && !ctx.request_streamed {
            let url = pipeline::target_url(&route.upstream.urls[0], &plan.suffix, ctx.query.as_deref());
            let key = pipeline::cache_key(&route.path, &method, &url, &key_body, &caller.cache_key());
            if let Some(hit) = pipeline::cache_get(&*self.state.kv, &key) {
                let mut resp = Resp::bytes(hit.status, "application/octet-stream", hit.body_bytes());
                resp.headers = hit.headers.clone();
                resp.headers.push(("x-moxy-cache".into(), "hit".into()));
                write_resp(session, resp)
                    .await
                    .map_err(|e| MoxyError::internal(e.to_string()))?;
                return Ok(true);
            }
            ctx.cache_key = Some(key);
        }
        ctx.route = Some(route);
        ctx.plan = Some(plan);
        Ok(false)
    }
}

fn apply_ops_req(h: &mut RequestHeader, ops: &[HeaderOpR]) -> Result<()> {
    for op in ops {
        match op {
            HeaderOpR::Set(n, v) => h.insert_header(n.clone(), v.as_str())?,
            HeaderOpR::Add(n, v) => {
                h.append_header(n.clone(), v.as_str())?;
            }
            HeaderOpR::Remove(n) => {
                h.remove_header(n.as_str());
            }
        }
    }
    Ok(())
}

fn apply_ops_resp(h: &mut ResponseHeader, ops: &[HeaderOpR]) -> Result<()> {
    for op in ops {
        match op {
            HeaderOpR::Set(n, v) => h.insert_header(n.clone(), v.as_str())?,
            HeaderOpR::Add(n, v) => {
                h.append_header(n.clone(), v.as_str())?;
            }
            HeaderOpR::Remove(n) => {
                h.remove_header(n.as_str());
            }
        }
    }
    Ok(())
}

#[async_trait]
impl ProxyHttp for MoxyProxy {
    type CTX = Ctx;

    fn new_ctx(&self) -> Ctx {
        Ctx {
            guard: Some(InflightGuard::new(&self.state)),
            ..Default::default()
        }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Ctx) -> Result<bool> {
        let path = session.req_header().uri.path().to_string();
        if builtin::is_builtin(&path) {
            let body = match read_body(session, builtin::MAX_BUILTIN_BODY).await {
                Ok(b) => b,
                Err(e) => {
                    write_error(session, &e).await?;
                    return Ok(true);
                }
            };
            let rh = session.req_header();
            let req = Req {
                method: rh.method.as_str().to_string(),
                path,
                query: rh.uri.query().unwrap_or("").to_string(),
                headers: rh.headers.clone(),
                body,
                client_ip: client_ip(session),
            };
            let resp = builtin::handle(&self.state, req).await;
            write_resp(session, resp).await?;
            return Ok(true);
        }
        match self.prepare_proxy(session, ctx).await {
            Ok(done) => Ok(done),
            Err(e) => {
                write_error(session, &e).await?;
                Ok(true)
            }
        }
    }

    async fn upstream_peer(&self, _session: &mut Session, ctx: &mut Ctx) -> Result<Box<HttpPeer>> {
        let route = ctx.route.clone().expect("route set in request_filter");
        let timeout = ctx.plan.as_ref().map(|p| p.timeout).unwrap_or(Duration::from_secs(60));
        loop {
            let Some(idx) = self.state.lb.select(&route, &ctx.tried) else {
                return Err(reject_err(
                    ctx,
                    MoxyError::upstream_unavailable("no upstream could be reached"),
                ));
            };
            ctx.tried.push(idx);
            let base = &route.upstream.urls[idx];
            let host = base.host_str().unwrap_or_default().to_string();
            let port = base.port_or_known_default().unwrap_or(80);
            let addr = tokio::net::lookup_host((host.trim_matches(|c| c == '[' || c == ']'), port))
                .await
                .ok()
                .and_then(|mut a| a.next());
            let Some(addr) = addr else {
                log::warn!("cannot resolve upstream host {host}");
                self.state.lb.mark_down(base.as_str());
                continue;
            };
            ctx.current = Some(idx);
            let tls = base.scheme() == "https";
            let mut peer = HttpPeer::new(addr, tls, host);
            peer.options.connection_timeout = Some(timeout);
            peer.options.total_connection_timeout = Some(timeout);
            peer.options.read_timeout = Some(timeout);
            peer.options.write_timeout = Some(timeout);
            return Ok(Box::new(peer));
        }
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream: &mut RequestHeader,
        ctx: &mut Ctx,
    ) -> Result<()> {
        let route = ctx.route.clone().expect("route");
        let plan = ctx.plan.as_ref().expect("plan");
        let base = &route.upstream.urls[ctx.current.unwrap_or(0)];
        let pq = pipeline::target_path(base, &plan.suffix, ctx.query.as_deref());
        let uri: http::Uri = pq
            .parse()
            .map_err(|e| Error::because(ErrorType::InternalError, "bad upstream uri", e))?;
        upstream.set_uri(uri);
        upstream.insert_header("host", pipeline::host_header(base))?;
        // Never forward the caller's credentials.
        upstream.remove_header("authorization");
        if let Some(c) = header(session, "cookie").map(str::to_string) {
            match strip_cookie(&c, auth::SESSION_COOKIE) {
                Some(rest) => upstream.insert_header("cookie", rest)?,
                None => {
                    upstream.remove_header("cookie");
                }
            }
        }
        if ctx.cache_key.is_some() || route.has_token_budget {
            // Bodies must be readable for usage counting / cache storage.
            upstream.insert_header("accept-encoding", "identity")?;
        }
        if let Some(b) = &ctx.replacement_body {
            upstream.remove_header("transfer-encoding");
            upstream.insert_header("content-length", b.len().to_string())?;
        } else if ctx.buffer_body {
            upstream.remove_header("content-length");
        }
        apply_ops_req(upstream, &plan.header_ops)?;
        Ok(())
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Ctx,
    ) -> Result<()> {
        if let Some(b) = &ctx.replacement_body {
            *body = end_of_stream.then(|| b.clone());
            return Ok(());
        }
        if let Some(chunk) = body.as_ref() {
            ctx.body_seen += chunk.len() as u64;
            if let Some(limit) = ctx.plan.as_ref().and_then(|p| p.body_limit) {
                if ctx.body_seen > limit {
                    return Err(reject_err(ctx, MoxyError::payload_too_large(limit)));
                }
            }
        }
        if ctx.buffer_body {
            if let Some(chunk) = body.take() {
                ctx.body_buf.extend_from_slice(&chunk);
            }
            if end_of_stream {
                let orig = std::mem::take(&mut ctx.body_buf).freeze();
                let route = ctx.route.as_ref().expect("route");
                let provider = route.ai.as_ref().map(|a| a.provider);
                ctx.request_streamed = ai::wants_stream(&orig);
                let new = provider.and_then(|p| {
                    ai::rewrite_request(
                        p,
                        &orig,
                        ctx.plan.as_ref().and_then(|pl| pl.model_override.as_deref()),
                        route.has_token_budget,
                    )
                });
                *body = Some(new.map(Bytes::from).unwrap_or(orig));
            }
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        resp: &mut ResponseHeader,
        ctx: &mut Ctx,
    ) -> Result<()> {
        let Some(route) = ctx.route.clone() else {
            return Ok(());
        };
        if resp.status.is_informational() {
            return Ok(());
        }
        let get = |n: &str| resp.headers.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
        let streamed = ctx.request_streamed || pipeline::is_streamed_content_type(get("content-type").as_deref());
        if let (Some(ai_spec), true) = (&route.ai, route.has_token_budget) {
            ctx.usage = Some(UsageTracker::new(ai_spec.provider, streamed));
        }
        if ctx.cache_key.is_some()
            && pipeline::response_cacheable(resp.status.as_u16(), get("cache-control").as_deref(), streamed)
        {
            ctx.cache_buf = Some(Vec::new());
        } else {
            ctx.cache_key = None;
        }
        if let Some(plan) = &ctx.plan {
            apply_ops_resp(resp, &plan.response_ops)?;
        }
        if ctx.cache_key.is_some() {
            ctx.cache_status = resp.status.as_u16();
            ctx.cache_headers = resp
                .headers
                .iter()
                .filter(|(k, _)| {
                    !matches!(
                        k.as_str(),
                        "content-length" | "transfer-encoding" | "connection" | "keep-alive" | "date"
                    )
                })
                .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
                .collect();
        }
        Ok(())
    }

    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Ctx,
    ) -> Result<Option<Duration>> {
        if let Some(b) = body.as_ref() {
            if let Some(u) = ctx.usage.as_mut() {
                u.feed(b);
            }
            if let Some(buf) = ctx.cache_buf.as_mut() {
                if buf.len() + b.len() > MAX_CACHE_BODY {
                    ctx.cache_buf = None;
                } else {
                    buf.extend_from_slice(b);
                }
            }
        }
        if end_of_stream {
            if let (Some(key), Some(buf), Some(route), Some(plan)) =
                (ctx.cache_key.take(), ctx.cache_buf.take(), &ctx.route, &ctx.plan)
            {
                if let Some(ttl) = plan.cache_ttl {
                    pipeline::cache_put(
                        &*self.state.kv,
                        &key,
                        &route.path,
                        ctx.cache_status,
                        std::mem::take(&mut ctx.cache_headers),
                        &buf,
                        ttl,
                    );
                }
            }
        }
        Ok(None)
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Ctx,
        mut e: Box<Error>,
    ) -> Box<Error> {
        if let (Some(route), Some(i)) = (&ctx.route, ctx.current) {
            self.state.lb.mark_down(route.upstream.urls[i].as_str());
            let more = ctx.tried.len() < route.upstream.urls.len();
            e.set_retry(more);
        }
        e
    }

    fn error_while_proxy(
        &self,
        peer: &HttpPeer,
        session: &mut Session,
        e: Box<Error>,
        ctx: &mut Ctx,
        client_reused: bool,
    ) -> Box<Error> {
        if is_timeout(&e) {
            if let (Some(route), Some(i)) = (&ctx.route, ctx.current) {
                self.state.lb.mark_down(route.upstream.urls[i].as_str());
            }
        }
        let mut e = e.more_context(format!("Peer: {peer}"));
        if !session.req_header().method.is_idempotent() || session.as_ref().retry_buffer_truncated() {
            e.set_retry(false);
        } else {
            e.retry.decide_reuse(client_reused);
        }
        e
    }

    async fn fail_to_proxy(&self, session: &mut Session, e: &Error, ctx: &mut Ctx) -> FailToProxy {
        let err = ctx.reject.take().unwrap_or_else(|| map_error(e));
        if err.status > 0 {
            if let Err(we) = write_error(session, &err).await {
                log::debug!("could not send error response: {we}");
            }
        }
        FailToProxy {
            error_code: err.status,
            can_reuse_downstream: false,
        }
    }

    async fn logging(&self, session: &mut Session, e: Option<&Error>, ctx: &mut Ctx) {
        // Count token usage even if the client disconnected mid-stream.
        if let (Some(mut usage), Some(plan)) = (ctx.usage.take(), &ctx.plan) {
            let (_, total) = usage.finish();
            pipeline::record_usage(&*self.state.kv, &plan.budgets, total);
        }
        let status = session.response_written().map(|r| r.status.as_u16()).unwrap_or(0);
        let rh = session.req_header();
        match e {
            Some(err) => log::info!("{} {} -> {} ({})", rh.method, rh.uri, status, err),
            None => log::info!("{} {} -> {}", rh.method, rh.uri, status),
        }
        ctx.guard.take();
    }
}
