use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use memorious_core::{Journal, Node};
use memorious_server::{app, AppState};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,iroh=warn".into()),
        )
        .init();

    let data: PathBuf = std::env::var_os("MEMORIOUS_DATA")
        .map(PathBuf::from)
        .context("MEMORIOUS_DATA env var required")?;
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "4600".into())
        .parse()
        .context("bad PORT")?;
    let web_dist = std::env::var_os("WEB_DIST").map(PathBuf::from);
    let downloads_dir = std::env::var_os("DOWNLOADS_DIR").map(PathBuf::from);
    // Headless peer: no keychain, no prompt — the password comes from the
    // environment (systemd credential / process-compose env file).
    let password = std::env::var("MEMORIOUS_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty())
        .context("MEMORIOUS_PASSWORD env var required (master password)")?;

    let journal = if data.join("db.sqlite").exists() {
        Journal::open(&data, &password)?
    } else {
        tracing::info!("no journal at {} — creating one", data.display());
        Journal::init(&data, &password)?
    };
    let node = Node::spawn(journal).await?;
    if let Ok(addr) = node.dialable_addr().await {
        tracing::info!("iroh peer up: {} ({} addrs)", node.endpoint().id(), addr.addrs.len());
    }
    if let Ok(ticket) = node.ticket() {
        tracing::info!("pairing ticket: {ticket}");
    }

    let state = Arc::new(AppState { node, downloads_dir });
    if let Some(engines) = memorious_server::sweeper::SystemEngines::detect() {
        memorious_server::sweeper::spawn(state.clone(), Arc::new(engines));
        tracing::info!("enrichment sweeper running");
    }
    let router = app(state.clone(), web_dist);

    // devhost proxies 127.0.0.1; bind IPv4 explicitly (Caddy won't reach [::1]).
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!("http on http://127.0.0.1:{port}");
    axum::serve(listener, router).await?;
    Ok(())
}
