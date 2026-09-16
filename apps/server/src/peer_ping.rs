//! In-process peer-ping scheduler: converges with every known peer on a
//! timer, once immediately at startup and again on every interval, plus once
//! more on graceful shutdown. Replaces relying on an external cron (the Mac's
//! `com.ponelat.memorious-peer-ping` LaunchAgent) to be the only thing that
//! ever dials out — every server now does it for itself while it's running.

use std::sync::Arc;
use std::time::Duration;

use crate::AppState;

const PING_TIMEOUT: Duration = Duration::from_secs(4);

/// One round: ping every known peer, log the reachable count.
pub async fn ping_once(state: &AppState) {
    match state.node.ping_peers(PING_TIMEOUT).await {
        Ok(pings) => {
            let ok = pings.iter().filter(|p| p.ok).count();
            tracing::info!("peer ping: {ok}/{} reachable", pings.len());
        }
        Err(err) => tracing::warn!("peer ping failed: {err:#}"),
    }
}

/// Run forever: ping now, sleep `PEER_PING_INTERVAL_MS` (default 15 min —
/// matches the old LaunchAgent cadence), repeat. Caller keeps the returned
/// handle only if it wants to abort the loop explicitly; otherwise it runs
/// for the life of the process.
pub fn spawn(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    let interval_ms = std::env::var("PEER_PING_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(900_000u64);
    tokio::spawn(async move {
        loop {
            ping_once(&state).await;
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        }
    })
}
