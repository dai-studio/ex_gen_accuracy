//! The serve service: startup sequence, Pingora server and graceful shutdown.

use crate::kv::{KvStore, RedbStore};
use crate::proxy::MoxyProxy;
use crate::state::{self, AppState, EnvFn};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use pingora::server::configuration::ServerConf;
use pingora::server::{ExecutionPhase, RunArgs, Server, ShutdownSignal, ShutdownSignalWatch};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const SERVICE_VERSION: &str = "0.2.0";
/// How long in-flight requests may run after a shutdown signal.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// The three environment variables Moxy reads directly (only at startup).
#[derive(Debug, Clone)]
pub struct Settings {
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub log_level: log::LevelFilter,
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        let config_path = std::env::var_os("MOXY_CONFIG")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("moxy.xml"));
        let data_dir = std::env::var_os("MOXY_DATA_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("data"));
        let log_level = match std::env::var("MOXY_LOG").ok().as_deref().map(str::trim) {
            None | Some("") => log::LevelFilter::Info,
            Some(v) => parse_level(v).with_context(|| {
                format!("MOXY_LOG must be error, warn, info, debug or trace (got '{v}')")
            })?,
        };
        Ok(Self {
            config_path,
            data_dir,
            log_level,
        })
    }
}

pub fn parse_level(v: &str) -> Option<log::LevelFilter> {
    match v.to_ascii_lowercase().as_str() {
        "error" => Some(log::LevelFilter::Error),
        "warn" => Some(log::LevelFilter::Warn),
        "info" => Some(log::LevelFilter::Info),
        "debug" => Some(log::LevelFilter::Debug),
        "trace" => Some(log::LevelFilter::Trace),
        _ => None,
    }
}

/// Both SIGINT and SIGTERM begin a graceful shutdown.
struct Signals;

#[async_trait]
impl ShutdownSignalWatch for Signals {
    async fn recv(&self) -> ShutdownSignal {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = term.recv() => log::info!("SIGTERM received, shutting down"),
            _ = int.recv() => log::info!("SIGINT received, shutting down"),
        }
        ShutdownSignal::GracefulTerminate
    }
}

pub fn process_env() -> EnvFn {
    Arc::new(|name: &str| std::env::var(name).ok())
}

/// Start the server and block until it shuts down.
pub fn serve(settings: Settings) -> Result<()> {
    // 1. environment variables have been read into `settings`.
    // 2. load and validate moxy.xml; refuse to start if it is missing or invalid.
    let env = process_env();
    let (config, hash) = state::load_file(&settings.config_path, &env).map_err(|errors| {
        anyhow::anyhow!(
            "invalid configuration {}: {}",
            settings.config_path.display(),
            crate::config::format_errors(&errors)
        )
    })?;
    log::info!(
        "loaded {} with {} route(s)",
        settings.config_path.display(),
        config.routes.len()
    );

    // 3. open the key-value store.
    let kv: Arc<dyn KvStore> = Arc::new(RedbStore::open_dir(&settings.data_dir)?);
    log::info!(
        "key-value store: {}",
        settings.data_dir.join(crate::kv::STORE_FILE).display()
    );

    // 4. bind to the configured address.
    let addr = format!("{}:{}", config.server.host, config.server.port);
    if let Err(e) = std::net::TcpListener::bind(&addr) {
        bail!("cannot bind to {addr}: {e}");
    }
    let state = AppState::new(settings.config_path.clone(), config, hash, kv.clone(), env);

    let conf = ServerConf {
        threads: std::thread::available_parallelism()
            .map(|n| n.get().min(8))
            .unwrap_or(2),
        grace_period_seconds: Some(SHUTDOWN_GRACE.as_secs()),
        graceful_shutdown_timeout_seconds: Some(1),
        ..Default::default()
    };
    let mut server = Server::new_with_opt_and_conf(None, conf);
    server.bootstrap();
    let mut service = pingora::proxy::http_proxy_service(
        &server.configuration,
        MoxyProxy {
            state: state.clone(),
        },
    );
    service.add_tcp(&addr);
    server.add_service(service);

    // 5. once listening, watch moxy.xml and run the periodic sweep.
    let mut phases = server.watch_execution_phase();
    let bg_state = state.clone();
    std::thread::Builder::new()
        .name("moxy-background".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("background runtime");
            rt.block_on(async move {
                loop {
                    match phases.recv().await {
                        Ok(ExecutionPhase::Running) => break,
                        Ok(_) => continue,
                        Err(_) => return,
                    }
                }
                log::info!("moxy serve {SERVICE_VERSION} listening on http://{addr}");
                tokio::spawn(state::sweep_loop(bg_state.clone()));
                state::watch_config(bg_state).await;
            });
        })?;

    // Graceful shutdown: once listeners close, wait for in-flight requests
    // (at most thirty seconds), then close the store and exit.
    let mut shutdown_phases = server.watch_execution_phase();
    let sd_state = state.clone();
    std::thread::Builder::new()
        .name("moxy-shutdown".into())
        .spawn(move || {
            loop {
                match shutdown_phases.blocking_recv() {
                    Ok(ExecutionPhase::GracefulTerminate) | Ok(ExecutionPhase::ShutdownStarted) => {
                        break;
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
            let start = Instant::now();
            // allow the listeners a moment to stop accepting
            std::thread::sleep(Duration::from_millis(100));
            while sd_state.inflight() > 0 && start.elapsed() < SHUTDOWN_GRACE {
                std::thread::sleep(Duration::from_millis(50));
            }
            let remaining = sd_state.inflight();
            if remaining > 0 {
                log::warn!(
                    "{remaining} request(s) still running after {}s; exiting anyway",
                    SHUTDOWN_GRACE.as_secs()
                );
            }
            if let Err(e) = sd_state.kv.close() {
                log::error!("closing the key-value store failed: {e:#}");
            }
            log::info!("shutdown complete");
            std::process::exit(0);
        })?;

    server.run(RunArgs {
        shutdown_signal: Box::new(Signals),
    });
    let _ = kv.close();
    Ok(())
}
