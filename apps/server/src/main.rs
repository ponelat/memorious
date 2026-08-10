use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use journal_core::{Journal, Node};
use journal_server::{app, AppState};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,iroh=warn".into()),
        )
        .init();

    let data: PathBuf = std::env::var_os("JOURNAL_DATA")
        .map(PathBuf::from)
        .context("JOURNAL_DATA env var required")?;
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "4600".into())
        .parse()
        .context("bad PORT")?;
    let web_dist = std::env::var_os("WEB_DIST").map(PathBuf::from);

    let journal = if data.join("db.sqlite").exists() {
        Journal::open(&data)?
    } else {
        tracing::info!("no journal at {} — creating one", data.display());
        Journal::init(&data)?
    };
    let node = Node::spawn(journal).await?;
    if let Ok(addr) = node.dialable_addr().await {
        tracing::info!("iroh peer up: {} ({} addrs)", node.endpoint().id(), addr.addrs.len());
    }
    if let Ok(ticket) = node.ticket() {
        tracing::info!("pairing ticket: {ticket}");
    }

    let state = Arc::new(AppState { node });
    let router = app(state, web_dist);

    // devhost proxies 127.0.0.1; bind IPv4 explicitly (Caddy won't reach [::1]).
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!("http on http://127.0.0.1:{port}");
    axum::serve(listener, router).await?;
    Ok(())
}
