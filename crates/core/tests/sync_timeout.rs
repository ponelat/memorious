//! Sync against an unreachable peer must fail fast. Without a guard, a dead
//! peer costs iroh's own dial timeout (~30s) on every foreground sync — the
//! sheet sits on "syncing…" the whole time. Data transfer stays unbounded on
//! purpose: a big media sync over a slow link may legitimately take minutes;
//! only *reaching* the peer is budgeted.

use std::time::{Duration, Instant};

use memorious_core::node::Node;
use memorious_core::Journal;
use tempfile::tempdir;

#[tokio::test]
async fn sync_with_blackholed_peer_fails_fast() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();
    let b_id = b.addr().id;
    b.shutdown().await;

    // The owner's real failure shape: a stored address whose IP blackholes
    // (different network, packets silently dropped) and a live relay the
    // peer is not connected to.
    let mut addr = iroh::EndpointAddr::from(b_id);
    addr.addrs
        .insert(iroh::TransportAddr::Ip("10.255.255.1:9".parse().unwrap()));
    addr.addrs.insert(iroh::TransportAddr::Relay(
        "https://euw1.relay.iroh.network./".parse().unwrap(),
    ));

    let start = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(60), a.sync_with(&addr))
        .await
        .expect("sync_with hung past 60s");
    let took = start.elapsed();
    assert!(result.is_err(), "blackholed peer must not sync");
    assert!(
        took < Duration::from_secs(15),
        "sync gave up after {took:?}; unreachable peers must fail fast"
    );
    a.shutdown().await;
}
