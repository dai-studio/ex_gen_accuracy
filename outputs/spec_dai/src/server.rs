//! Process startup and shutdown.
//!
//! Startup: read env -> load moxy.xml (exit non-zero if invalid) -> open the KV store ->
//! bind host:port -> watch moxy.xml. Shutdown on SIGINT/SIGTERM: stop accepting, let
//! in-flight requests finish (max 30 s), then exit.

use crate::config;
use crate::proxy::MoxyProxy;
use crate::state::{AppState, EnvLookup, EnvSettings};
use crate::store::{KvStore, RedbStore};
use async_trait::async_trait;
use pingora::server::configuration::ServerConf;
use pingora::server::{ExecutionPhase, RunArgs, Server, ShutdownSignal, ShutdownSignalWatch};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
const SWEEP_EVERY: Duration = Duration::from_secs(600);

/// SIGINT and SIGTERM both shut down gracefully.
struct Signals;

#[async_trait]
impl ShutdownSignalWatch for Signals {
    async fn recv(&self) -> ShutdownSignal {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = term.recv() => log::info!("SIGTERM received"),
            _ = int.recv() => log::info!("SIGINT received"),
        }
        ShutdownSignal::GracefulTerminate
    }
}

pub fn init_logging(level: &str) {
    let lvl = match level.to_ascii_lowercase().as_str() {
        l @ ("error" | "warn" | "info" | "debug" | "trace") => l.to_string(),
        other => {
            eprintln!("MOXY_LOG={other} is not a valid level; using info");
            "info".to_string()
        }
    };
    // Pingora is chatty at debug level; keep it one notch quieter than Moxy itself.
    let filter = format!("{lvl},pingora_core=warn,pingora_proxy=warn");
    let _ = env_logger::Builder::new()
        .parse_filters(&filter)
        .format_timestamp_millis()
        .try_init();
}

/// Load everything and build the shared state. Errors are returned as messages so the
/// caller can log them and exit non-zero.
pub fn bootstrap(env: EnvSettings, lookup: EnvLookup) -> Result<Arc<AppState>, String> {
    let text = std::fs::read_to_string(&env.config_path)
        .map_err(|e| format!("cannot read {}: {e}", env.config_path.display()))?;
    let active = config::load_str(&text, &*lookup).map_err(|errs| {
        format!(
            "{} is invalid:\n  - {}",
            env.config_path.display(),
            errs.join("\n  - ")
        )
    })?;
    let kv = open_store(&env)?;
    Ok(AppState::new(env, lookup, active, kv))
}

pub fn open_store(env: &EnvSettings) -> Result<Arc<dyn KvStore>, String> {
    std::fs::create_dir_all(&env.data_dir)
        .map_err(|e| format!("cannot create {}: {e}", env.data_dir.display()))?;
    let path = env.data_dir.join("moxy.redb");
    let kv = RedbStore::open(&path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    Ok(Arc::new(kv))
}

pub fn run(state: Arc<AppState>) {
    let active = state.active();
    let host = &active.resolved.host;
    let addr = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{}", active.resolved.port)
    } else {
        format!("{host}:{}", active.resolved.port)
    };

    // Background sweep of expired KV entries.
    {
        let state = state.clone();
        std::thread::Builder::new()
            .name("moxy-kv-sweep".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(SWEEP_EVERY);
                    match state.kv.sweep_expired() {
                        Ok(n) if n > 0 => log::debug!("swept {n} expired KV entries"),
                        Ok(_) => {}
                        Err(e) => log::warn!("KV sweep failed: {e}"),
                    }
                }
            })
            .expect("spawn sweep thread");
    }

    let conf = ServerConf {
        threads: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2),
        grace_period_seconds: Some(SHUTDOWN_GRACE.as_secs()),
        graceful_shutdown_timeout_seconds: Some(1),
        ..Default::default()
    };
    let mut server = Server::new_with_opt_and_conf(None, conf);
    server.bootstrap();
    let mut svc = pingora::proxy::http_proxy_service(
        &server.configuration,
        MoxyProxy {
            state: state.clone(),
        },
    );
    svc.add_tcp(&addr);
    server.add_service(svc);

    // Watch moxy.xml once the listener is set up.
    let _watcher = match crate::watcher::watch(state.clone()) {
        Ok(w) => Some(w),
        Err(e) => {
            log::error!("cannot watch {}: {e}", state.env.config_path.display());
            None
        }
    };

    // Exit as soon as in-flight requests are done instead of always waiting the full
    // grace period.
    let mut phases = server.watch_execution_phase();
    {
        let state = state.clone();
        std::thread::spawn(move || {
            while let Ok(phase) = phases.blocking_recv() {
                if matches!(phase, ExecutionPhase::ShutdownGracePeriod) {
                    let start = Instant::now();
                    while state.inflight.load(Ordering::SeqCst) > 0 && start.elapsed() < SHUTDOWN_GRACE {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    log::info!("shutdown complete");
                    // redb commits are durable; exiting here closes the store file.
                    std::process::exit(0);
                }
            }
        });
    }

    log::info!(
        "moxy listening on http://{addr} (config {} version {})",
        state.env.config_path.display(),
        active.version
    );
    server.run(RunArgs {
        shutdown_signal: Box::new(Signals),
    });
    log::info!("shutdown complete");
}
