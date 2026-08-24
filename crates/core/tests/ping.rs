//! Peer ping: "which peers can I reach right now?" A ping is the normal sync
//! handshake with a stopwatch and a timeout — reachable literally means "able
//! to sync" (and a behind peer is healed by the probe itself). Peer addresses
//! are remembered from real contacts (`peer_addr:` meta, both directions), so
//! any previously-synced peer can be probed without a fresh ticket.

use std::time::Duration;

use memorious_core::node::Node;
use memorious_core::Journal;
use tempfile::tempdir;

const PING_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn ping_reports_reachable_then_unreachable() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();

    // No contact yet: nothing to ping.
    assert!(a.ping_peers(PING_TIMEOUT).await.unwrap().is_empty());

    // One real sync records the address in BOTH directions.
    a.journal().capture_text("hello").unwrap();
    b.sync_with(&a.addr()).await.unwrap();

    // Initiator side (b dialed a).
    let pings = b.ping_peers(PING_TIMEOUT).await.unwrap();
    assert_eq!(pings.len(), 1);
    assert!(pings[0].ok, "a is up: {:?}", pings[0].error);
    assert!(pings[0].rtt_ms.is_some());

    // Responder side (a learned b's dialable addr from Hello).
    let pings = a.ping_peers(PING_TIMEOUT).await.unwrap();
    assert_eq!(pings.len(), 1);
    assert_eq!(pings[0].endpoint_id, b.addr().id.to_string());
    assert!(pings[0].ok, "b is up: {:?}", pings[0].error);

    // A ping refreshes last-contact, so the health light stays honest.
    let before = a.peers().await.unwrap()[0].last_ok_ms;
    tokio::time::sleep(Duration::from_millis(5)).await;
    a.ping_peers(PING_TIMEOUT).await.unwrap();
    assert!(a.peers().await.unwrap()[0].last_ok_ms > before);

    // Peer gone: ping says so instead of hanging.
    b.shutdown().await;
    let pings = a.ping_peers(Duration::from_secs(2)).await.unwrap();
    assert_eq!(pings.len(), 1);
    assert!(!pings[0].ok);
    assert!(pings[0].rtt_ms.is_none());
}

#[tokio::test]
async fn ping_heals_a_behind_peer() {
    // "Able to sync" is proven by syncing: a probe against a peer that has
    // events we lack converges the logs as a side effect.
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();
    b.sync_with(&a.addr()).await.unwrap();

    a.journal().capture_text("captured while b was away").unwrap();
    let pings = b.ping_peers(PING_TIMEOUT).await.unwrap();
    assert!(pings[0].ok);
    assert_eq!(b.journal().list().unwrap().len(), 1, "ping pulled the event");
}
