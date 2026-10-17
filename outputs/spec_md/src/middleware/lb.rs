//! Upstream selection for `<upstream>` and `<load-balance>` destinations.
//!
//! An upstream that errors or times out is skipped for the following thirty seconds.

use crate::config::{Destination, Route, Strategy};
use rand::Rng;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use url::Url;

pub const UNHEALTHY_FOR: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Balancer {
    counters: Mutex<HashMap<String, usize>>,
    unhealthy: Mutex<HashMap<String, Instant>>,
}

impl Balancer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark_failed(&self, url: &Url) {
        self.unhealthy
            .lock()
            .unwrap()
            .insert(url.as_str().to_string(), Instant::now() + UNHEALTHY_FOR);
    }

    pub fn is_healthy(&self, url: &Url) -> bool {
        let mut map = self.unhealthy.lock().unwrap();
        match map.get(url.as_str()) {
            Some(until) if *until > Instant::now() => false,
            Some(_) => {
                map.remove(url.as_str());
                true
            }
            None => true,
        }
    }

    /// Choose an upstream index for `route`, never one listed in `tried`.
    /// Healthy upstreams are preferred; if none is healthy the others are still tried.
    pub fn select(&self, route: &Route, tried: &[usize]) -> Option<usize> {
        let upstreams = route.destination.upstreams();
        let candidates: Vec<usize> = (0..upstreams.len())
            .filter(|i| !tried.contains(i))
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let healthy: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|i| self.is_healthy(&upstreams[*i]))
            .collect();
        let pool = if healthy.is_empty() {
            candidates
        } else {
            healthy
        };
        let strategy = match &route.destination {
            Destination::Single(_) => Strategy::Failover,
            Destination::LoadBalance { strategy, .. } => *strategy,
        };
        Some(match strategy {
            Strategy::Failover => pool[0],
            Strategy::Random => pool[rand::rng().random_range(0..pool.len())],
            Strategy::RoundRobin => {
                let mut counters = self.counters.lock().unwrap();
                let c = counters.entry(route.path().to_string()).or_insert(0);
                let pick = pool[*c % pool.len()];
                *c = c.wrapping_add(1);
                pick
            }
        })
    }

    /// Whether another untried upstream remains for a retry.
    pub fn has_alternative(&self, route: &Route, tried: &[usize]) -> bool {
        (0..route.destination.upstreams().len()).any(|i| !tried.contains(&i))
    }
}

/// Join an upstream base URL, a forwarded path suffix and a query string.
pub fn upstream_url(base: &Url, suffix: &str, query: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str(base.scheme());
    s.push_str("://");
    s.push_str(&authority(base));
    s.push_str(&upstream_path(base, suffix, query));
    s
}

/// Path and query to send to the upstream.
pub fn upstream_path(base: &Url, suffix: &str, query: Option<&str>) -> String {
    let mut path = base.path().trim_end_matches('/').to_string();
    path.push_str(suffix);
    if path.is_empty() {
        path.push('/');
    }
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        path.push('?');
        path.push_str(q);
    }
    path
}

/// `host[:port]` as it should appear in the Host header.
pub fn authority(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_str;

    fn route(strategy: &str) -> std::sync::Arc<Route> {
        let cfg = load_str(
            &format!(
                r#"<moxy version="1"><route path="/a/*"><load-balance strategy="{strategy}">
                <upstream url="http://one"/><upstream url="http://two"/><upstream url="http://three"/>
                </load-balance></route></moxy>"#
            ),
            &|_| None,
        )
        .unwrap();
        cfg.routes[0].clone()
    }

    #[test]
    fn round_robin_cycles_and_skips_unhealthy() {
        let b = Balancer::new();
        let r = route("round-robin");
        let picks: Vec<usize> = (0..3).map(|_| b.select(&r, &[]).unwrap()).collect();
        assert_eq!(picks, vec![0, 1, 2]);
        b.mark_failed(&r.destination.upstreams()[1]);
        for _ in 0..4 {
            assert_ne!(b.select(&r, &[]).unwrap(), 1);
        }
    }

    #[test]
    fn failover_prefers_first_healthy() {
        let b = Balancer::new();
        let r = route("failover");
        assert_eq!(b.select(&r, &[]), Some(0));
        b.mark_failed(&r.destination.upstreams()[0]);
        assert_eq!(b.select(&r, &[]), Some(1));
        assert_eq!(b.select(&r, &[1]), Some(2));
        assert_eq!(b.select(&r, &[0, 1, 2]), None);
        // all unhealthy: still try something
        b.mark_failed(&r.destination.upstreams()[1]);
        b.mark_failed(&r.destination.upstreams()[2]);
        assert_eq!(b.select(&r, &[]), Some(0));
    }

    #[test]
    fn random_stays_in_range() {
        let b = Balancer::new();
        let r = route("random");
        for _ in 0..20 {
            assert!(b.select(&r, &[]).unwrap() < 3);
        }
    }

    #[test]
    fn url_joining() {
        let base = Url::parse("https://api.openai.com/v1").unwrap();
        assert_eq!(
            upstream_url(&base, "/chat/completions", Some("a=1")),
            "https://api.openai.com/v1/chat/completions?a=1"
        );
        let root = Url::parse("http://127.0.0.1:11434/").unwrap();
        assert_eq!(upstream_url(&root, "", None), "http://127.0.0.1:11434/");
        assert_eq!(
            upstream_url(&root, "/api/chat", None),
            "http://127.0.0.1:11434/api/chat"
        );
        assert_eq!(authority(&root), "127.0.0.1:11434");
        assert_eq!(authority(&base), "api.openai.com");
    }
}
