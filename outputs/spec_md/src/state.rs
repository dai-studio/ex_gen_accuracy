//! Shared server state and the configuration lifecycle (load, reload, write back, watch).

use crate::auth::oidc::OidcClient;
use crate::config::{self, Config, ConfigDoc, ValidationError};
use crate::kv::KvStore;
use crate::middleware::{cache, lb::Balancer, ratelimit::RateLimiter};
use crate::util::sha256_hex;
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type EnvFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub const RELOAD_DEBOUNCE: Duration = Duration::from_millis(500);
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(600);

/// Which configuration is active, and whether the last reload failed.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigStatus {
    /// Incremented every time a new configuration is activated.
    pub generation: u64,
    /// SHA-256 of the active moxy.xml contents.
    pub hash: String,
    pub loaded_at: DateTime<Utc>,
    pub path: String,
    pub last_error: Option<ReloadError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReloadError {
    pub at: DateTime<Utc>,
    pub message: String,
    pub errors: Vec<ValidationError>,
}

#[derive(Debug)]
pub enum ReloadOutcome {
    Unchanged,
    Reloaded,
    Failed(Vec<ValidationError>),
}

pub struct AppState {
    config: ArcSwap<Config>,
    status: Mutex<ConfigStatus>,
    pub config_path: PathBuf,
    pub kv: Arc<dyn KvStore>,
    pub limiter: RateLimiter,
    pub balancer: Balancer,
    pub http: reqwest::Client,
    pub oidc: OidcClient,
    pub env: EnvFn,
    /// Requests currently being handled (used for graceful shutdown).
    pub inflight: AtomicUsize,
    write_lock: Mutex<()>,
}

/// Decrements the in-flight counter when dropped.
pub struct InflightGuard(Arc<AppState>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Read and compile moxy.xml from disk.
pub fn load_file(path: &Path, env: &EnvFn) -> Result<(Config, String), Vec<ValidationError>> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        vec![ValidationError::new(
            path.display().to_string(),
            format!("cannot read configuration file: {e}"),
        )]
    })?;
    let hash = sha256_hex(text.as_bytes());
    let env = env.clone();
    let cfg = config::load_str(&text, &move |n| env(n))?;
    Ok((cfg, hash))
}

impl AppState {
    pub fn new(
        config_path: PathBuf,
        cfg: Config,
        hash: String,
        kv: Arc<dyn KvStore>,
        env: EnvFn,
    ) -> Arc<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("HTTP client builds");
        Arc::new(Self {
            config: ArcSwap::from_pointee(cfg),
            status: Mutex::new(ConfigStatus {
                generation: 1,
                hash,
                loaded_at: Utc::now(),
                path: config_path.display().to_string(),
                last_error: None,
            }),
            config_path,
            kv,
            limiter: RateLimiter::new(),
            balancer: Balancer::new(),
            oidc: OidcClient::new(http.clone()),
            http,
            env,
            inflight: AtomicUsize::new(0),
            write_lock: Mutex::new(()),
        })
    }

    /// The active configuration. Callers keep the `Arc` for the whole request,
    /// so a reload never changes the configuration under an in-flight request.
    pub fn config(&self) -> Arc<Config> {
        self.config.load_full()
    }

    pub fn status(&self) -> ConfigStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn track_request(self: &Arc<Self>) -> InflightGuard {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        InflightGuard(self.clone())
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }

    /// Re-read moxy.xml. A valid file is swapped in atomically; an invalid one
    /// leaves the previous configuration active and records the error.
    pub fn reload_from_disk(&self) -> ReloadOutcome {
        let text_hash = std::fs::read(&self.config_path)
            .map(|b| sha256_hex(&b))
            .ok();
        if text_hash.is_some()
            && text_hash.as_deref() == Some(self.status.lock().unwrap().hash.as_str())
        {
            return ReloadOutcome::Unchanged;
        }
        match load_file(&self.config_path, &self.env) {
            Ok((cfg, hash)) => {
                let old = self.config();
                if cfg.server != old.server {
                    log::warn!(
                        "server address changed to {}:{}; this takes effect after a restart",
                        cfg.server.host,
                        cfg.server.port
                    );
                }
                self.purge_changed_routes(&old, &cfg);
                self.config.store(Arc::new(cfg));
                let mut st = self.status.lock().unwrap();
                st.generation += 1;
                st.hash = hash;
                st.loaded_at = Utc::now();
                st.last_error = None;
                log::info!("configuration reloaded (generation {})", st.generation);
                ReloadOutcome::Reloaded
            }
            Err(errors) => {
                let message = config::format_errors(&errors);
                log::error!(
                    "configuration reload failed, keeping the previous configuration: {message}"
                );
                self.status.lock().unwrap().last_error = Some(ReloadError {
                    at: Utc::now(),
                    message,
                    errors: errors.clone(),
                });
                ReloadOutcome::Failed(errors)
            }
        }
    }

    /// Discard cache entries of routes that changed or disappeared.
    fn purge_changed_routes(&self, old: &Config, new: &Config) {
        let new_routes: HashMap<&str, &config::doc::RouteDoc> = new
            .doc
            .routes
            .iter()
            .map(|r| (r.path.as_str(), r))
            .collect();
        for r in &old.doc.routes {
            if new_routes.get(r.path.as_str()) != Some(&r) {
                match cache::purge_route(self.kv.as_ref(), &r.path) {
                    Ok(n) if n > 0 => log::info!("discarded {n} cache entries of route {}", r.path),
                    Ok(_) => {}
                    Err(e) => log::warn!("failed to purge cache for {}: {e:#}", r.path),
                }
            }
        }
    }

    /// Validate a document, write it to moxy.xml atomically and reload it.
    pub fn write_config(&self, doc: ConfigDoc) -> Result<Result<(), Vec<ValidationError>>> {
        let _guard = self.write_lock.lock().unwrap();
        let env = self.env.clone();
        if let Err(errors) = config::compile(doc.clone(), &move |n| env(n)) {
            return Ok(Err(errors));
        }
        let text = config::xml::write(&doc);
        write_atomically(&self.config_path, text.as_bytes())?;
        match self.reload_from_disk() {
            ReloadOutcome::Failed(errors) => Ok(Err(errors)),
            _ => Ok(Ok(())),
        }
    }

    /// Periodic maintenance: expired KV entries and finished rate limit windows.
    pub fn sweep(&self) {
        match self.kv.sweep_expired() {
            Ok(n) if n > 0 => log::debug!("swept {n} expired entries"),
            Ok(_) => {}
            Err(e) => log::warn!("sweep failed: {e:#}"),
        }
        self.limiter.cleanup();
    }
}

/// Write to a temporary file in the same directory, then rename it over the target.
pub fn write_atomically(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("moxy.xml");
    let tmp = dir.join(format!(".{name}.tmp-{}", crate::util::random_token(6)));
    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("renaming over {}", path.display()))?;
    Ok(())
}

/// Watch moxy.xml and reload it (after a debounce) whenever it changes.
/// Falls back to polling if the platform watcher cannot be created.
pub async fn watch_config(state: Arc<AppState>) {
    use notify::{RecursiveMode, Watcher};
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let file_name = state.config_path.file_name().map(|n| n.to_os_string());
    let dir = state
        .config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let watcher = {
        let tx = tx.clone();
        let file_name = file_name.clone();
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                let relevant = event.paths.is_empty()
                    || event
                        .paths
                        .iter()
                        .any(|p| p.file_name().map(|n| n.to_os_string()) == file_name);
                if relevant {
                    let _ = tx.send(());
                }
            }
        })
        .and_then(|mut w| w.watch(&dir, RecursiveMode::NonRecursive).map(|_| w))
    };
    let _watcher = match watcher {
        Ok(w) => Some(w),
        Err(e) => {
            log::warn!(
                "file watcher unavailable ({e}); polling {} instead",
                state.config_path.display()
            );
            let path = state.config_path.clone();
            tokio::spawn(async move {
                let mut last = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let now = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                    if now != last {
                        last = now;
                        let _ = tx.send(());
                    }
                }
            });
            None
        }
    };

    while rx.recv().await.is_some() {
        // debounce: wait until no further events arrive for the debounce period
        loop {
            match tokio::time::timeout(RELOAD_DEBOUNCE, rx.recv()).await {
                Ok(Some(())) => continue,
                Ok(None) => return,
                Err(_) => break,
            }
        }
        let st = state.clone();
        let _ = tokio::task::spawn_blocking(move || st.reload_from_disk()).await;
    }
}

/// Run the KV sweep every ten minutes.
pub async fn sweep_loop(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    interval.tick().await;
    loop {
        interval.tick().await;
        let st = state.clone();
        let _ = tokio::task::spawn_blocking(move || st.sweep()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::MemoryStore;

    const V1: &str = r#"<moxy version="1"><route path="/a/*" auth="none"><upstream url="http://one"/></route><route path="/b/*" auth="none"><upstream url="http://two"/></route></moxy>"#;
    const V2: &str = r#"<moxy version="1"><route path="/a/*" auth="none"><upstream url="http://changed"/></route><route path="/b/*" auth="none"><upstream url="http://two"/></route></moxy>"#;

    fn state(dir: &Path, text: &str) -> Arc<AppState> {
        let path = dir.join("moxy.xml");
        std::fs::write(&path, text).unwrap();
        let env: EnvFn = Arc::new(|_| None);
        let (cfg, hash) = load_file(&path, &env).unwrap();
        AppState::new(path, cfg, hash, Arc::new(MemoryStore::new()), env)
    }

    #[test]
    fn reload_valid_and_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let st = state(dir.path(), V1);
        assert!(matches!(st.reload_from_disk(), ReloadOutcome::Unchanged));

        // cache entries of the changed route are discarded, others kept
        cache::store(
            st.kv.as_ref(),
            "cache#1",
            "/a/*",
            200,
            vec![],
            b"x",
            Duration::from_secs(60),
        )
        .unwrap();
        cache::store(
            st.kv.as_ref(),
            "cache#2",
            "/b/*",
            200,
            vec![],
            b"y",
            Duration::from_secs(60),
        )
        .unwrap();

        std::fs::write(&st.config_path, V2).unwrap();
        assert!(matches!(st.reload_from_disk(), ReloadOutcome::Reloaded));
        assert_eq!(st.status().generation, 2);
        assert_eq!(
            st.config().routes[0].destination.upstreams()[0].as_str(),
            "http://changed/"
        );
        assert!(st.kv.get("cache#1").unwrap().is_none());
        assert!(st.kv.get("cache#2").unwrap().is_some());

        std::fs::write(&st.config_path, "<moxy version=\"1\"><bogus/></moxy>").unwrap();
        assert!(matches!(st.reload_from_disk(), ReloadOutcome::Failed(_)));
        // previous configuration stays active, error is recorded
        assert_eq!(st.config().routes.len(), 2);
        assert!(st.status().last_error.unwrap().message.contains("bogus"));
    }

    #[test]
    fn write_config_validates_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let st = state(dir.path(), V1);
        let mut doc = st.config().doc.clone();
        doc.routes.remove(1);
        st.write_config(doc).unwrap().unwrap();
        assert_eq!(st.config().routes.len(), 1);
        let on_disk = std::fs::read_to_string(&st.config_path).unwrap();
        assert!(!on_disk.contains("/b/*"));

        let mut bad = st.config().doc.clone();
        bad.routes[0].path = "/api/*".into();
        let errs = st.write_config(bad).unwrap().unwrap_err();
        assert!(config::format_errors(&errs).contains("reserved"));
        // file untouched by an invalid write
        assert_eq!(std::fs::read_to_string(&st.config_path).unwrap(), on_disk);
    }
}
