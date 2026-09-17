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
    // Default loopback-only (devhost/Caddy share this host's network namespace
    // on the Mac, so 127.0.0.1 is the deliberate trust boundary for the
    // passcode guard's X-Forwarded-For check). A container deployment where
    // the reverse proxy is a separate container needs HOST=0.0.0.0 — still not
    // internet-facing as long as the compose service publishes no `ports:`.
    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into());
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
    // Default friendly name: the server peer is what the browser fronts.
    journal.ensure_device_name("web")?;
    journal.ensure_peer_join()?;
    journal.ensure_version_seen()?;
    let node = Node::spawn(journal).await?;
    if let Ok(addr) = node.dialable_addr().await {
        tracing::info!("iroh peer up: {} ({} addrs)", node.endpoint().id(), addr.addrs.len());
    }
    if let Ok(ticket) = node.ticket() {
        tracing::info!("pairing ticket: {ticket}");
    }

    let state = Arc::new(AppState::new(node, downloads_dir));
    if let Some(engines) = memorious_server::sweeper::SystemEngines::detect() {
        memorious_server::sweeper::spawn(state.clone(), Arc::new(engines));
        tracing::info!("enrichment sweeper running");
    }
    // Converges with every known peer immediately, then on a timer, for as
    // long as this process is up — no external cron required.
    memorious_server::peer_ping::spawn(state.clone());
    let router = app(state.clone(), web_dist);

    // devhost proxies 127.0.0.1; bind IPv4 explicitly (Caddy won't reach [::1]).
    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    tracing::info!("http on http://{host}:{port}");
    let shutdown_state = state.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down — final peer ping");
            memorious_server::peer_ping::ping_once(&shutdown_state).await;
        })
        .await?;
    Ok(())
}

/// Ctrl-C (SIGINT) or Docker's default stop signal (SIGTERM) — either starts
/// a graceful shutdown so the final ping actually has time to run.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        let Ok(mut sig) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            return;
        };
        sig.recv().await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
