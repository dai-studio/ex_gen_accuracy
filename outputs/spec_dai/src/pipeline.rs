//! The per-route middleware pipeline, shared by the proxy path (`/*`) and `/api/chat`.
//!
//! The request phase runs the route's middleware in document order; the first one that
//! rejects stops the pipeline. Its result is a [`Plan`] that the caller applies while
//! forwarding. Response-phase header operations are returned in reverse document order.

use crate::auth::Identity;
use crate::config::{HeaderOpR, LimitKey, Mw, ReqOpR, RespOpR, RouteSpec, DEFAULT_TIMEOUT};
use crate::error::MoxyError;
use crate::state::AppState;
use crate::store::KvStore;
use crate::util::{now_secs, sha256_hex};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::time::Duration;
use url::Url;

/// Who is calling, for rate limits, budgets and cache separation.
#[derive(Debug, Clone, Copy)]
pub struct Caller<'a> {
    pub identity: Option<&'a Identity>,
    pub ip: Option<IpAddr>,
}

impl Caller<'_> {
    fn ip_key(&self) -> String {
        format!(
            "ip:{}",
            self.ip.map(|i| i.to_string()).unwrap_or_else(|| "unknown".into())
        )
    }

    /// The identity a limit is keyed on. API-key limits fall back to the user for
    /// session callers; anything without an identity falls back to the client IP.
    pub fn limit_key(&self, key: LimitKey) -> String {
        match (key, self.identity) {
            (LimitKey::Ip, _) => self.ip_key(),
            (LimitKey::ApiKey, Some(id)) => match id.api_key_id() {
                Some(k) => k.to_string(),
                None => id.sub.clone(),
            },
            (LimitKey::User, Some(id)) => id.sub.clone(),
            (_, None) => self.ip_key(),
        }
    }

    pub fn cache_key(&self) -> String {
        match self.identity {
            Some(id) => id.caller_key(),
            None => self.ip_key(),
        }
    }
}

/// A token-budget counter this request is accounted against.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetSlot {
    pub kv_key: String,
    pub window_end: u64,
}

#[derive(Debug, Clone)]
pub struct Plan {
    /// Upstream-relative path (after rewrite-path).
    pub suffix: String,
    pub header_ops: Vec<HeaderOpR>,
    /// Response header operations in execution (reverse document) order.
    pub response_ops: Vec<HeaderOpR>,
    pub body_limit: Option<u64>,
    pub timeout: Duration,
    pub model_override: Option<String>,
    pub budgets: Vec<BudgetSlot>,
    pub cache_ttl: Option<Duration>,
}

pub struct RequestInfo<'a> {
    pub suffix: String,
    pub content_length: Option<u64>,
    pub caller: Caller<'a>,
}

fn rewrite(suffix: &str, from: &str, to: &str) -> Option<String> {
    let from_t = from.trim_end_matches('/');
    let rest = if from_t.is_empty() {
        suffix
    } else if suffix == from_t {
        ""
    } else if suffix.starts_with(&format!("{from_t}/")) {
        &suffix[from_t.len()..]
    } else {
        return None;
    };
    let mut out = format!("{}{}", to.trim_end_matches('/'), rest);
    if out.is_empty() {
        out.push('/');
    }
    Some(out)
}

pub fn request_phase(
    state: &AppState,
    route: &RouteSpec,
    info: RequestInfo,
) -> Result<Plan, MoxyError> {
    let mut plan = Plan {
        suffix: info.suffix,
        header_ops: Vec::new(),
        response_ops: Vec::new(),
        body_limit: None,
        timeout: DEFAULT_TIMEOUT,
        model_override: None,
        budgets: Vec::new(),
        cache_ttl: None,
    };
    let now = now_secs();
    for mw in &route.middleware {
        match mw {
            Mw::Headers(ops) => plan.header_ops.extend(ops.iter().cloned()),
            Mw::Request(ops) => {
                for op in ops {
                    match op {
                        ReqOpR::RewritePath { from, to } => {
                            if let Some(s) = rewrite(&plan.suffix, from, to) {
                                plan.suffix = s;
                            }
                        }
                        ReqOpR::MaxBodySize(limit) => {
                            if info.content_length.is_some_and(|l| l > *limit) {
                                return Err(MoxyError::payload_too_large(*limit));
                            }
                            plan.body_limit =
                                Some(plan.body_limit.map_or(*limit, |l| l.min(*limit)));
                        }
                        ReqOpR::Timeout(d) => plan.timeout = *d,
                    }
                }
            }
            Mw::Response(ops) => {
                for op in ops {
                    if let RespOpR::Cache(ttl) = op {
                        plan.cache_ttl = Some(*ttl);
                    }
                }
            }
            Mw::Ai(ai) => plan.model_override = ai.model.clone(),
            Mw::RateLimit(rl) => {
                let key = format!(
                    "{}#{}#{}",
                    route.path,
                    rl.id,
                    info.caller.limit_key(rl.key)
                );
                if let Err(retry) = state.limiter.check(&key, rl.requests, rl.per, now) {
                    return Err(MoxyError::new(
                        429,
                        "rate_limited",
                        format!(
                            "rate limit of {} requests per {}s exceeded",
                            rl.requests,
                            rl.per.as_secs()
                        ),
                    )
                    .with_header("Retry-After", retry.to_string()));
                }
            }
            Mw::TokenBudget(tb) => {
                let per = tb.per.as_secs().max(1);
                let start = now - now % per;
                let slot = BudgetSlot {
                    kv_key: format!(
                        "usage#{}#{}#{}",
                        info.caller.limit_key(tb.key),
                        route.path,
                        start
                    ),
                    window_end: start + per,
                };
                let used = state
                    .kv
                    .get_raw(&slot.kv_key)
                    .ok()
                    .flatten()
                    .and_then(|v| v.get("tokens").and_then(Value::as_u64))
                    .unwrap_or(0);
                if used >= tb.tokens {
                    return Err(MoxyError::new(
                        429,
                        "token_budget_exceeded",
                        format!(
                            "token budget of {} tokens per {}s is used up",
                            tb.tokens, per
                        ),
                    )
                    .with_header("Retry-After", (slot.window_end - now).max(1).to_string()));
                }
                plan.budgets.push(slot);
            }
        }
    }
    // Response phase runs in reverse document order.
    for mw in route.middleware.iter().rev() {
        if let Mw::Response(ops) = mw {
            for op in ops {
                if let RespOpR::Header(h) = op {
                    plan.response_ops.push(h.clone());
                }
            }
        }
    }
    Ok(plan)
}

/// Add used tokens to every budget window this request was accounted against.
pub fn record_usage(kv: &dyn KvStore, budgets: &[BudgetSlot], tokens: u64) {
    if tokens == 0 {
        return;
    }
    for b in budgets {
        let expires = Utc
            .timestamp_opt(b.window_end as i64, 0)
            .single()
            .unwrap_or_else(Utc::now);
        let res = kv.update(&b.kv_key, &mut |cur| {
            let used = cur
                .as_ref()
                .and_then(|v| v.get("tokens").and_then(Value::as_u64))
                .unwrap_or(0);
            Some(json!({"tokens": used + tokens, "expiresAt": expires}))
        });
        if let Err(e) = res {
            log::error!("failed to record token usage for {}: {e}", b.kv_key);
        }
    }
}

/// Upstream URL for a request: the upstream base URL + the suffix + query string.
pub fn target_url(base: &Url, suffix: &str, query: Option<&str>) -> String {
    let mut path = format!("{}{}", base.path().trim_end_matches('/'), suffix);
    if path.is_empty() {
        path.push('/');
    }
    let mut out = format!(
        "{}://{}{}",
        base.scheme(),
        base.host_str().unwrap_or_default(),
        base.port().map(|p| format!(":{p}")).unwrap_or_default()
    );
    out.push_str(&path);
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        out.push('?');
        out.push_str(q);
    }
    out
}

/// The path-and-query part of [`target_url`].
pub fn target_path(base: &Url, suffix: &str, query: Option<&str>) -> String {
    let mut path = format!("{}{}", base.path().trim_end_matches('/'), suffix);
    if path.is_empty() {
        path.push('/');
    }
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        path.push('?');
        path.push_str(q);
    }
    path
}

/// `Host` header value for an upstream.
pub fn host_header(base: &Url) -> String {
    match base.port() {
        Some(p) => format!("{}:{p}", base.host_str().unwrap_or_default()),
        None => base.host_str().unwrap_or_default().to_string(),
    }
}

// ---------------------------------------------------------------- cache

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedResponse {
    pub route: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub expires_at: chrono::DateTime<Utc>,
}

impl CachedResponse {
    pub fn body_bytes(&self) -> Vec<u8> {
        B64.decode(&self.body).unwrap_or_default()
    }
}

/// JSON bodies are normalised (key order, whitespace); others are used as-is.
pub fn normalise_body(body: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<Value>(body) {
        Ok(v) => {
            let mut out = Vec::new();
            write_canonical(&v, &mut out);
            out
        }
        Err(_) => body.to_vec(),
    }
}

fn write_canonical(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend(serde_json::to_vec(k).unwrap_or_default());
                out.push(b':');
                write_canonical(&m[*k], out);
            }
            out.push(b'}');
        }
        Value::Array(a) => {
            out.push(b'[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(x, out);
            }
            out.push(b']');
        }
        other => out.extend(serde_json::to_vec(other).unwrap_or_default()),
    }
}

pub fn cache_key(
    route_path: &str,
    method: &str,
    upstream_url: &str,
    body: &[u8],
    caller: &str,
) -> String {
    let mut material = Vec::new();
    for part in [route_path.as_bytes(), method.as_bytes(), upstream_url.as_bytes()] {
        material.extend_from_slice(part);
        material.push(0);
    }
    material.extend(normalise_body(body));
    material.push(0);
    material.extend_from_slice(caller.as_bytes());
    format!("cache#{}", sha256_hex(&material))
}

pub fn cache_get(kv: &dyn KvStore, key: &str) -> Option<CachedResponse> {
    kv.get_raw(key)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value(v).ok())
}

pub fn cache_put(
    kv: &dyn KvStore,
    key: &str,
    route: &str,
    status: u16,
    headers: Vec<(String, String)>,
    body: &[u8],
    ttl: Duration,
) {
    let rec = CachedResponse {
        route: route.to_string(),
        status,
        headers,
        body: B64.encode(body),
        expires_at: Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_default(),
    };
    if let Ok(v) = serde_json::to_value(&rec) {
        if let Err(e) = kv.put_raw(key, &v) {
            log::warn!("failed to store cache entry: {e}");
        }
    }
}

/// Is a request allowed to use the cache at all?
pub fn request_cacheable(method: &str, route: &RouteSpec, cache_control: Option<&str>) -> bool {
    let method_ok = method == "GET" || (method == "POST" && route.ai.is_some());
    let no_cache = cache_control.is_some_and(|c| {
        c.split(',')
            .any(|d| d.trim().eq_ignore_ascii_case("no-cache") || d.trim().eq_ignore_ascii_case("no-store"))
    });
    route.cache_ttl.is_some() && method_ok && !no_cache
}

/// May this response be stored?
pub fn response_cacheable(status: u16, cache_control: Option<&str>, streamed: bool) -> bool {
    let forbidden = cache_control.is_some_and(|c| {
        c.split(',').any(|d| {
            let d = d.trim();
            d.eq_ignore_ascii_case("no-store") || d.to_ascii_lowercase().starts_with("private")
        })
    });
    status == 200 && !streamed && !forbidden
}

/// Is the response a stream (SSE or NDJSON)?
pub fn is_streamed_content_type(ct: Option<&str>) -> bool {
    ct.is_some_and(|c| {
        let c = c.to_ascii_lowercase();
        c.starts_with("text/event-stream") || c.starts_with("application/x-ndjson")
    })
}

pub fn drop_cache_for_routes(kv: &dyn KvStore, routes: &[String]) -> Result<usize, String> {
    let mut n = 0;
    for (k, v) in kv.scan_prefix("cache#")? {
        if v
            .get("route")
            .and_then(Value::as_str)
            .is_some_and(|r| routes.iter().any(|x| x == r))
        {
            kv.delete(&k)?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    #[test]
    fn rewrite_paths() {
        assert_eq!(rewrite("/v1/chat", "/v1", "/").as_deref(), Some("/chat"));
        assert_eq!(rewrite("/v1", "/v1", "/v2").as_deref(), Some("/v2"));
        assert_eq!(rewrite("/v10/chat", "/v1", "/v2"), None);
        assert_eq!(rewrite("/a", "/", "/api").as_deref(), Some("/api/a"));
        assert_eq!(rewrite("/old/x", "/old/", "/new/").as_deref(), Some("/new/x"));
    }

    #[test]
    fn target_urls() {
        let u = Url::parse("https://api.openai.com/v1").unwrap();
        assert_eq!(
            target_url(&u, "/chat/completions", Some("a=1")),
            "https://api.openai.com/v1/chat/completions?a=1"
        );
        let u = Url::parse("http://localhost:11434").unwrap();
        assert_eq!(target_url(&u, "", None), "http://localhost:11434/");
        assert_eq!(target_path(&u, "/api/chat", None), "/api/chat");
        assert_eq!(host_header(&u), "localhost:11434");
        let u = Url::parse("http://h/base/").unwrap();
        assert_eq!(target_path(&u, "/x", None), "/base/x");
    }

    #[test]
    fn cache_keys_normalise_json_and_separate_callers() {
        let a = cache_key("/r/*", "POST", "http://u/x", br#"{"b":1,"a":[1,{"d":2,"c":3}]}"#, "apikey:1");
        let b = cache_key("/r/*", "POST", "http://u/x", br#"{ "a":[1,{"c":3,"d":2}], "b":1 }"#, "apikey:1");
        let c = cache_key("/r/*", "POST", "http://u/x", br#"{"b":1,"a":[1,{"d":2,"c":3}]}"#, "apikey:2");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("cache#"));
    }

    #[test]
    fn cache_policy() {
        assert!(response_cacheable(200, None, false));
        assert!(!response_cacheable(200, Some("private, max-age=1"), false));
        assert!(!response_cacheable(200, Some("no-store"), false));
        assert!(!response_cacheable(200, None, true));
        assert!(!response_cacheable(201, None, false));
        assert!(is_streamed_content_type(Some("text/event-stream; charset=utf-8")));
    }

    #[test]
    fn drop_cache_entries() {
        let kv = MemStore::default();
        cache_put(&kv, "cache#1", "/a/*", 200, vec![], b"x", Duration::from_secs(60));
        cache_put(&kv, "cache#2", "/b/*", 200, vec![], b"y", Duration::from_secs(60));
        assert_eq!(drop_cache_for_routes(&kv, &["/a/*".into()]).unwrap(), 1);
        assert!(cache_get(&kv, "cache#1").is_none());
        assert_eq!(cache_get(&kv, "cache#2").unwrap().body_bytes(), b"y");
    }

    #[test]
    fn usage_accumulates() {
        let kv = MemStore::default();
        let slot = BudgetSlot {
            kv_key: "usage#k#/r/*#0".into(),
            window_end: now_secs() + 100,
        };
        record_usage(&kv, std::slice::from_ref(&slot), 5);
        record_usage(&kv, std::slice::from_ref(&slot), 7);
        assert_eq!(kv.get_raw(&slot.kv_key).unwrap().unwrap()["tokens"], 12);
    }
}
