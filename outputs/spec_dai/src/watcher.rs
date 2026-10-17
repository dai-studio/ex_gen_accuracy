//! Watches moxy.xml and reloads it (debounced 500 ms).

use crate::state::AppState;
use notify::{RecursiveMode, Watcher};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// Start watching. The returned watcher must be kept alive.
pub fn watch(state: Arc<AppState>) -> notify::Result<notify::RecommendedWatcher> {
    let path = state.env.config_path.clone();
    let file_name = path.file_name().map(|f| f.to_os_string());
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| ".".into());
    let (tx, rx) = mpsc::channel::<()>();
    // Watch the directory, not the file: editors and our own writes replace the file
    // with an atomic rename.
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if ev.paths.iter().any(|p| p.file_name().map(|f| f.to_os_string()) == file_name) {
                let _ = tx.send(());
            }
        }
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;
    std::thread::Builder::new()
        .name("moxy-config-watch".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                // Debounce: wait until no event arrived for DEBOUNCE.
                while rx.recv_timeout(DEBOUNCE).is_ok() {}
                match state.reload_from_disk() {
                    Ok(true) => log::info!("moxy.xml changed; new config is active"),
                    Ok(false) => log::debug!("moxy.xml event without content change"),
                    Err(_) => {} // already logged and recorded for /admin
                }
            }
        })
        .expect("spawn watcher thread");
    Ok(watcher)
}
