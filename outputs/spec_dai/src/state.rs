//! Shared server state: active config, KV store, in-memory counters.

use crate::config::{self, ActiveConfig, Config, RouteSpec, Strategy};
use crate::store::KvStore;
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rand::Rng;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// How long an upstream that failed is skipped by load balancing.
pub const UPSTREAM_DOWN_FOR: Duration = Duration::from_secs(30);

pub type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Settings read from environment variables at startup.
#[derive(Debug, Clone)]
pub struct EnvSettings {
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub log: String,
}

impl EnvSettings {
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Self {
        EnvSettings {
            config_path: env("MOXY_CONFIG")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "./moxy.xml".into())
                .into(),
            data_dir: env("MOXY_DATA_DIR")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "./data".into())
                .into(),
            log: env("MOXY_LOG")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "info".into()),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReloadError {
    pub message: String,
    pub errors: Vec<String>,
    pub at: DateTime<Utc>,
}

pub struct AppState {
    pub env: EnvSettings,
    pub env_lookup: EnvLookup,
    pub config: ArcSwap<ActiveConfig>,
    pub last_error: Mutex<Option<ReloadError>>,
    pub kv: Arc<dyn KvStore>,
    pub limiter: RateLimiter,
    pub lb: LoadBalancer,
    pub oidc: crate::auth::OidcCache,
    /// HTTP client for upstream calls made by Moxy itself (/api/chat, OIDC).
    pub http: reqwest::Client,
    /// Serialises writes of moxy.xml.
    pub write_lock: tokio::sync::Mutex<()>,
    pub inflight: AtomicUsize,
}

impl AppState {
    pub fn new(
        env: EnvSettings,
        env_lookup: EnvLookup,
        active: ActiveConfig,
        kv: Arc<dyn KvStore>,
    ) -> Arc<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("http client");
        Arc::new(AppState {
            env,
            env_lookup,
            config: ArcSwap::from_pointee(active),
            last_error: Mutex::new(None),
            kv,
            limiter: RateLimiter::default(),
            lb: LoadBalancer::default(),
            oidc: Default::default(),
            http,
            write_lock: tokio::sync::Mutex::new(()),
            inflight: AtomicUsize::new(0),
        })
    }

    pub fn active(&self) -> Arc<ActiveConfig> {
        self.config.load_full()
    }

    /// Re-read moxy.xml from disk. Invalid files leave the current config active.
    pub fn reload_from_disk(&self) -> Result<bool, Vec<String>> {
        let text = std::fs::read_to_string(&self.env.config_path).map_err(|e| {
            let errs = vec![format!(
                "cannot read {}: {e}",
                self.env.config_path.display()
            )];
            self.record_error(&errs);
            errs
        })?;
        self.apply_text(&text)
    }

    /// Validate and swap in a new config text. Returns Ok(false) if unchanged.
    pub fn apply_text(&self, text: &str) -> Result<bool, Vec<String>> {
        if config::version_of(text) == self.active().version {
            return Ok(false);
        }
        let env = self.env_lookup.clone();
        match config::load_str(text, &*env) {
            Ok(new) => {
                self.swap(new);
                Ok(true)
            }
            Err(errs) => {
                self.record_error(&errs);
                Err(errs)
            }
        }
    }

    fn record_error(&self, errs: &[String]) {
        log::error!("moxy.xml is invalid; keeping the previous config: {}", errs.join("; "));
        *self.last_error.lock() = Some(ReloadError {
            message: "moxy.xml is invalid; the previous config is still active".into(),
            errors: errs.to_vec(),
            at: Utc::now(),
        });
    }

    fn swap(&self, new: ActiveConfig) {
        let old = self.active();
        if (old.resolved.host.as_str(), old.resolved.port)
            != (new.resolved.host.as_str(), new.resolved.port)
        {
            log::warn!(
                "<server host/port> changed to {}:{}; this takes effect after a restart",
                new.resolved.host,
                new.resolved.port
            );
        }
        // Drop cache entries of routes that changed or were removed.
        let changed: Vec<String> = old
            .resolved
            .routes
            .iter()
            .filter(|o| {
                !new.resolved
                    .routes
                    .iter()
                    .any(|n| n.path == o.path && n.fingerprint == o.fingerprint)
            })
            .map(|o| o.path.clone())
            .collect();
        if !changed.is_empty() {
            match crate::pipeline::drop_cache_for_routes(&*self.kv, &changed) {
                Ok(n) if n > 0 => log::info!("dropped {n} cache entries for changed routes"),
                Ok(_) => {}
                Err(e) => log::warn!("failed to drop cache entries: {e}"),
            }
        }
        log::info!(
            "config version {} loaded ({} routes)",
            new.version,
            new.resolved.routes.len()
        );
        self.config.store(Arc::new(new));
        *self.last_error.lock() = None;
    }

    /// Validate a raw config, write it to moxy.xml atomically and activate it.
    pub async fn write_config(&self, raw: &Config) -> Result<Arc<ActiveConfig>, Vec<String>> {
        let _guard = self.write_lock.lock().await;
        let env = self.env_lookup.clone();
        let (xml, active) = config::validate_raw(raw, &*env)?;
        let path = &self.env.config_path;
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let tmp = dir.join(format!(
            ".{}.tmp-{}",
            path.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "moxy.xml".into()),
            crate::util::random_hex(4)
        ));
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(xml.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            let _ = std::fs::remove_file(&tmp);
            return Err(vec![format!("failed to write {}: {e}", path.display())]);
        }
        // Activate right away; the watcher sees the same version and does nothing.
        if config::version_of(&xml) != self.active().version {
            self.swap(active);
        }
        Ok(self.active())
    }

    pub fn secure_cookies(&self) -> bool {
        let host = self.active().resolved.host.to_ascii_lowercase();
        !matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]")
    }
}

/// In-memory fixed-window rate-limit counters (reset on restart).
#[derive(Default)]
pub struct RateLimiter {
    windows: Mutex<HashMap<String, (u64, u64)>>,
}

impl RateLimiter {
    /// Count one request. Err(retry_after_secs) when the limit is exceeded.
    pub fn check(&self, key: &str, limit: u64, per: Duration, now: u64) -> Result<(), u64> {
        let per = per.as_secs().max(1);
        let start = now - now % per;
        let mut m = self.windows.lock();
        if m.len() > 100_000 {
            m.retain(|_, (s, _)| *s + per > now);
        }
        let entry = m.entry(key.to_string()).or_insert((start, 0));
        if entry.0 != start {
            *entry = (start, 0);
        }
        if entry.1 >= limit {
            return Err((start + per - now).max(1));
        }
        entry.1 += 1;
        Ok(())
    }
}

/// Upstream selection with 30 s skipping of failed upstreams.
#[derive(Default)]
pub struct LoadBalancer {
    down: Mutex<HashMap<String, Instant>>,
    rr: Mutex<HashMap<String, usize>>,
}

impl LoadBalancer {
    pub fn mark_down(&self, url: &str) {
        log::warn!(
            "upstream {url} failed; skipping it for {}s",
            UPSTREAM_DOWN_FOR.as_secs()
        );
        self.down
            .lock()
            .insert(url.to_string(), Instant::now() + UPSTREAM_DOWN_FOR);
    }

    pub fn is_down(&self, url: &str) -> bool {
        let mut d = self.down.lock();
        match d.get(url) {
            Some(until) if *until > Instant::now() => true,
            Some(_) => {
                d.remove(url);
                false
            }
            None => false,
        }
    }

    /// Pick the index of the next upstream to try, skipping `tried` ones.
    /// Healthy upstreams are preferred; if all untried ones are down, the first untried
    /// one is used anyway rather than failing without trying.
    pub fn select(&self, route: &RouteSpec, tried: &[usize]) -> Option<usize> {
        let urls = &route.upstream.urls;
        let untried: Vec<usize> = (0..urls.len()).filter(|i| !tried.contains(i)).collect();
        if untried.is_empty() {
            return None;
        }
        let healthy: Vec<usize> = untried
            .iter()
            .copied()
            .filter(|i| !self.is_down(urls[*i].as_str()))
            .collect();
        let pool = if healthy.is_empty() { &untried } else { &healthy };
        Some(match route.upstream.strategy {
            Strategy::Single | Strategy::Failover => pool[0],
            Strategy::Random => pool[rand::rng().random_range(0..pool.len())],
            Strategy::RoundRobin => {
                if !tried.is_empty() {
                    pool[0]
                } else {
                    let mut rr = self.rr.lock();
                    let n = rr.entry(route.path.clone()).or_insert(0);
                    // Advance over all upstreams so the rotation is stable even when some are down.
                    let mut pick = pool[0];
                    for step in 0..urls.len() {
                        let cand = (*n + step) % urls.len();
                        if pool.contains(&cand) {
                            pick = cand;
                            break;
                        }
                    }
                    *n = (pick + 1) % urls.len();
                    pick
                }
            }
        })
    }
}

/// RAII guard counting in-flight requests (used for graceful shutdown).
pub struct InflightGuard(Arc<AppState>);

impl InflightGuard {
    pub fn new(state: &Arc<AppState>) -> Self {
        state.inflight.fetch_add(1, Ordering::SeqCst);
        InflightGuard(state.clone())
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_windows() {
        let l = RateLimiter::default();
        let per = Duration::from_secs(60);
        assert!(l.check("k", 2, per, 120).is_ok());
        assert!(l.check("k", 2, per, 130).is_ok());
        assert_eq!(l.check("k", 2, per, 150), Err(30));
        assert!(l.check("other", 2, per, 150).is_ok());
        assert!(l.check("k", 2, per, 180).is_ok(), "new window");
    }

    fn route(xml_lb: &str) -> Arc<RouteSpec> {
        let a = config::load_str(
            &format!(r#"<moxy version="1"><route path="/x/*">{xml_lb}</route></moxy>"#),
            &|_| None,
        )
        .unwrap();
        a.resolved.routes[0].clone()
    }

    #[test]
    fn load_balancing() {
        let lb = LoadBalancer::default();
        let r = route(
            r#"<load-balance strategy="round-robin"><upstream url="http://a" /><upstream url="http://b" /><upstream url="http://c" /></load-balance>"#,
        );
        let picks: Vec<usize> = (0..4).map(|_| lb.select(&r, &[]).unwrap()).collect();
        assert_eq!(picks, vec![0, 1, 2, 0]);
        lb.mark_down("http://b/");
        let picks: Vec<usize> = (0..3).map(|_| lb.select(&r, &[]).unwrap()).collect();
        assert!(!picks.contains(&1), "{picks:?}");

        let f = route(
            r#"<load-balance strategy="failover"><upstream url="http://p" /><upstream url="http://q" /></load-balance>"#,
        );
        assert_eq!(lb.select(&f, &[]), Some(0));
        assert_eq!(lb.select(&f, &[0]), Some(1));
        assert_eq!(lb.select(&f, &[0, 1]), None);
        lb.mark_down("http://p/");
        assert_eq!(lb.select(&f, &[]), Some(1));
    }
}
