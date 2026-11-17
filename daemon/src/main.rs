use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentdock_daemon::acp_host::{Host, HostConfig};
use agentdock_daemon::api::{AppState, router};
use agentdock_daemon::discovery;
use agentdock_daemon::todo::TodoStore;
use anyhow::{Context, bail};
use serde_json::json;
use tracing_subscriber::EnvFilter;

const DEFAULT_ADDR: &str = "127.0.0.1:7820";
const SCAN_INTERVAL: Duration = Duration::from_secs(3);

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {},
        _ = term.recv() => {},
    }
    tracing::info!("shutdown signal received");
}

fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share/agentdock")
}

/// Periodically import confirmed todo markers from session files.
async fn scan_loop(host: Arc<Host>, todos: Arc<TodoStore>) {
    let dir = discovery::kiro_sessions_dir();
    let mut first = true;
    loop {
        let started = Instant::now();
        let (todos2, dir2) = (todos.clone(), dir.clone());
        match tokio::task::spawn_blocking(move || todos2.scan_dir(&dir2)).await {
            Ok(Ok(added)) => {
                if first {
                    tracing::info!(
                        "initial todo scan done in {:?}, {} new",
                        started.elapsed(),
                        added.len()
                    );
                    first = false;
                }
                for todo in added {
                    tracing::info!(id = todo.id, title = %todo.title, "todo imported");
                    host.emit(json!({"type": "todo.changed", "action": "added", "todo": todo}));
                }
            }
            Ok(Err(e)) => tracing::warn!("todo scan failed: {}", e.message),
            Err(e) => tracing::error!("todo scan task panicked: {e}"),
        }
        tokio::time::sleep(SCAN_INTERVAL).await;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let addr: SocketAddr = std::env::var("AGENTDOCK_ADDR")
        .unwrap_or_else(|_| DEFAULT_ADDR.into())
        .parse()
        .context("AGENTDOCK_ADDR must be ip:port")?;
    // Phase 1 security boundary: never expose the daemon beyond this machine.
    if !addr.ip().is_loopback() {
        bail!("refusing to listen on non-loopback address {addr} (phase 1 is local only)");
    }

    let db_path = std::env::var_os("AGENTDOCK_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir().join("agentdock.sqlite3"));
    let todos = Arc::new(
        TodoStore::open(&db_path)
            .map_err(|e| anyhow::anyhow!("open {}: {}", db_path.display(), e.message))?,
    );
    let host = Host::new(HostConfig::default());
    let scanner = tokio::spawn(scan_loop(host.clone(), todos.clone()));

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!(
        "agentdock-daemon listening on ws://{addr}/ws (db {})",
        db_path.display()
    );
    let state = AppState {
        host: host.clone(),
        todos,
    };
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    scanner.abort();
    host.shutdown_all().await;
    tracing::info!("all agent processes closed");
    Ok(())
}
