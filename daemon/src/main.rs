use std::net::SocketAddr;

use anyhow::{Context, bail};
use tracing_subscriber::EnvFilter;

const DEFAULT_ADDR: &str = "127.0.0.1:7820";

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

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!("agentdock-daemon listening on ws://{addr}/ws");
    axum::serve(listener, agentdock_daemon::api::router()).await?;
    Ok(())
}
