//! The per-route middleware pipeline.
//!
//! The request phase runs in document order and the first rejection stops the
//! pipeline before the upstream is called. Response-phase work (response
//! headers, caching, token accounting) runs in reverse document order and is
//! driven by the proxy and chat handlers using the helpers in this module.

pub mod ai;
pub mod budget;
pub mod cache;
pub mod lb;
pub mod ratelimit;

use crate::auth::{Caller, SESSION_COOKIE};
use crate::config::{AiConfig, BudgetWindow, Middleware, RequestOp, Route};
use crate::error::MoxyError;
use crate::kv::KvStore;
use crate::util::strip_cookie;
use http::HeaderMap;
use ratelimit::RateLimiter;
use std::net::IpAddr;
use std::time::Duration;

/// The outcome of a successful request phase.
#[derive(Debug, Clone)]
pub struct RequestPlan {
    /// Headers to send upstream.
    pub headers: HeaderMap,
    /// Path suffix to append to the upstream URL (after rewrites).
    pub suffix: String,
    pub timeout: Duration,
    pub max_body: Option<u64>,
    pub ai: Option<AiConfig>,
    pub cache_ttl: Option<Duration>,
    /// Token budget counters to charge once usage is known.
    pub budget_counters: Vec<(String, BudgetWindow)>,
}

/// Inputs to the request phase.
pub struct RequestInput<'a> {
    pub route: &'a Route,
    pub caller: &'a Caller,
    pub client_ip: Option<IpAddr>,
    pub headers: &'a HeaderMap,
    pub suffix: &'a str,
    pub content_length: Option<u64>,
}

/// Remove Moxy's own credentials so they never reach an upstream.
pub fn strip_credentials(headers: &mut HeaderMap) {
    headers.remove(http::header::AUTHORIZATION);
    let cookies: Vec<String> = headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_string))
        .collect();
    headers.remove(http::header::COOKIE);
    for c in cookies {
        if let Some(rest) = strip_cookie(&c, SESSION_COOKIE)
            && let Ok(v) = rest.parse()
        {
            headers.append(http::header::COOKIE, v);
        }
    }
}

/// Run the request phase of `route`'s middleware in document order.
pub fn run_request_phase(
    input: RequestInput,
    limiter: &RateLimiter,
    kv: &dyn KvStore,
) -> Result<RequestPlan, MoxyError> {
    let route = input.route;
    let mut headers = input.headers.clone();
    strip_credentials(&mut headers);
    let mut plan = RequestPlan {
        headers,
        suffix: input.suffix.to_string(),
        timeout: crate::config::DEFAULT_TIMEOUT,
        max_body: None,
        ai: route.ai().cloned(),
        cache_ttl: route.cache_ttl(),
        budget_counters: Vec::new(),
    };
    for (index, mw) in route.middleware.iter().enumerate() {
        match mw {
            Middleware::Headers(ops) => {
                for op in ops {
                    op.apply(&mut plan.headers);
                }
            }
            Middleware::Request(r) => {
                for op in &r.ops {
                    match op {
                        RequestOp::RewritePath { from, to } => {
                            if let Some(rest) = plan.suffix.strip_prefix(from.as_str()) {
                                plan.suffix = format!("{to}{rest}");
                            }
                        }
                        RequestOp::MaxBodySize(limit) => {
                            if input.content_length.is_some_and(|len| len > *limit) {
                                return Err(MoxyError::payload_too_large(*limit));
                            }
                            plan.max_body = Some(plan.max_body.map_or(*limit, |m| m.min(*limit)));
                        }
                        RequestOp::Timeout(d) => plan.timeout = *d,
                    }
                }
            }
            Middleware::Response(_) | Middleware::Ai(_) => {}
            Middleware::RateLimit(rl) => {
                let identity = input.caller.count_identity(rl.key, input.client_ip);
                let key = format!("{}#{index}#{identity}", route.path());
                limiter.check(&key, rl)?;
            }
            Middleware::TokenBudget(tb) => {
                let identity = input.caller.count_identity(tb.key, input.client_ip);
                budget::check(kv, route.path(), tb, &identity)?;
                plan.budget_counters.push((identity, tb.window));
            }
        }
    }
    Ok(plan)
}

/// Apply the response-phase header operations (reverse document order).
pub fn apply_response_headers(route: &Route, headers: &mut HeaderMap) {
    for op in route.response_header_ops() {
        op.apply(headers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_str;
    use crate::kv::MemoryStore;

    fn route(inner: &str) -> std::sync::Arc<Route> {
        let cfg = load_str(
            &format!(r#"<moxy version="1"><route path="/a/*" auth="none">{inner}<upstream url="http://u/base"/></route></moxy>"#),
            &|n| (n == "TOKEN").then(|| "t0k".to_string()),
        )
        .unwrap();
        cfg.routes[0].clone()
    }

    fn input<'a>(route: &'a Route, headers: &'a HeaderMap, len: Option<u64>) -> RequestInput<'a> {
        RequestInput {
            route,
            caller: &Caller::Anonymous,
            client_ip: Some("127.0.0.1".parse().unwrap()),
            headers,
            suffix: "/v1/x",
            content_length: len,
        }
    }

    #[test]
    fn strips_credentials_and_applies_headers_in_order() {
        let r = route(
            r#"<headers><set name="Authorization" value="Bearer ${TOKEN}"/><add name="X-A" value="1"/></headers>
               <headers><add name="X-A" value="2"/><remove name="X-Remove"/></headers>"#,
        );
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer moxy_secret".parse().unwrap());
        h.insert("cookie", "moxy_session=abc; theme=dark".parse().unwrap());
        h.insert("x-remove", "x".parse().unwrap());
        let plan = run_request_phase(
            input(&r, &h, None),
            &RateLimiter::new(),
            &MemoryStore::new(),
        )
        .unwrap();
        assert_eq!(plan.headers["authorization"], "Bearer t0k");
        assert_eq!(plan.headers["cookie"], "theme=dark");
        let xa: Vec<_> = plan.headers.get_all("x-a").iter().collect();
        assert_eq!(xa.len(), 2);
        assert!(plan.headers.get("x-remove").is_none());
    }

    #[test]
    fn rewrites_limits_and_timeouts() {
        let r = route(
            r#"<request><rewrite-path from="/v1/" to="/api/"/><max-body-size bytes="10"/><timeout duration="2s"/></request>"#,
        );
        let h = HeaderMap::new();
        let plan = run_request_phase(
            input(&r, &h, Some(5)),
            &RateLimiter::new(),
            &MemoryStore::new(),
        )
        .unwrap();
        assert_eq!(plan.suffix, "/api/x");
        assert_eq!(plan.timeout, Duration::from_secs(2));
        assert_eq!(plan.max_body, Some(10));
        let err = run_request_phase(
            input(&r, &h, Some(11)),
            &RateLimiter::new(),
            &MemoryStore::new(),
        )
        .unwrap_err();
        assert_eq!(err.status.as_u16(), 413);
    }

    #[test]
    fn first_rejection_wins() {
        let r = route(r#"<rate-limit requests="1" per="1m"/><rate-limit requests="5" per="1h"/>"#);
        let h = HeaderMap::new();
        let limiter = RateLimiter::new();
        let kv = MemoryStore::new();
        assert!(run_request_phase(input(&r, &h, None), &limiter, &kv).is_ok());
        let err = run_request_phase(input(&r, &h, None), &limiter, &kv).unwrap_err();
        assert_eq!(err.kind, "rate_limited");
    }

    #[test]
    fn response_headers_run_in_reverse() {
        let r = route(
            r#"<response><set name="X-Order" value="first-in-doc"/></response>
               <response><set name="X-Order" value="second-in-doc"/></response>"#,
        );
        let mut h = HeaderMap::new();
        apply_response_headers(&r, &mut h);
        // reverse order: the first element in the document runs last
        assert_eq!(h["x-order"], "first-in-doc");
    }
}
