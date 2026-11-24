//! A write-only peer is a full peer in code, configured not to pull events when it joins a network: it keeps its own events,
//! pushes them, and receives nothing. It can be brought up temporarily, capture, sync, and be thrown away.

use memorious_core::node::Node;
use memorious_core::Journal;
use tempfile::tempdir;

#[tokio::test]
async fn a_write_only_peer_pushes_its_events_and_pulls_none() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    ja.capture_text("already in the network").unwrap();
    let secret = *ja.secret();
    let a = Node::spawn(ja).await.unwrap();

    let jw = Journal::init_with_secret(&dir.path().join("w"), secret, "pw").unwrap();
    jw.set_write_only(true).unwrap();
    assert!(jw.is_write_only().unwrap());
    let w = Node::spawn(jw).await.unwrap();
    let mine = w.journal().capture_text("captured on the throwaway peer").unwrap();
    w.journal().capture_text("and another").unwrap();
    let mine_count = w.journal().store_events_for_test().len();

    // W dials A: its events reach A, A's do not reach W
    let r = w.sync_events_with(&a.addr()).await.unwrap();
    assert_eq!(r.received, 0, "a write-only peer takes nothing in");
    assert!(r.sent >= 2);
    assert_eq!(w.journal().store_events_for_test().len(), mine_count, "W holds only what it wrote");
    assert!(a.journal().store_events_for_test().iter().any(|e| e.event_id == mine.event_id), "A got W's event");
    assert!(w.journal().store_events_for_test().iter().all(|e| e.device_id == w.journal().device_id()));

    // A dials W (W is a full peer in code): A learns W's events, and still sends W nothing
    let before_a = a.journal().store_events_for_test().len();
    a.journal().capture_text("later, on A").unwrap();
    a.sync_events_with(&w.addr()).await.unwrap();
    assert_eq!(w.journal().store_events_for_test().len(), mine_count, "W still holds only its own");
    assert!(a.journal().store_events_for_test().len() > before_a);
    a.shutdown().await;
    w.shutdown().await;
}

#[tokio::test]
async fn a_write_only_peer_announces_that_it_keeps_and_receives_nothing_so_it_is_never_a_backup() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jw = Journal::init_with_secret(&dir.path().join("w"), *ja.secret(), "pw").unwrap();
    jw.set_write_only(true).unwrap();
    jw.ensure_peer_policy().unwrap();
    let e = jw.infra_events("peer_policy").unwrap().pop().unwrap();
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(v["payload"]["data"]["keeps_revisions"], false, "it holds nothing of others, so it is not a backup");
    assert_eq!(v["payload"]["data"]["receives"], false);
    ja.store_insert_remote_for_test(&e);
    assert!(!ja.has_backup_peer().unwrap(), "a write-only peer is not a backup");
}

#[tokio::test]
async fn join_write_only_makes_a_peer_without_pulling_the_network() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    ja.capture_text("history the throwaway peer must not pull").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let ticket = a.ticket().unwrap();

    let w = Node::join_write_only(&dir.path().join("w"), &ticket, "pw").await.unwrap();
    assert!(w.journal().is_write_only().unwrap());
    assert!(w.journal().store_events_for_test().iter().all(|e| e.device_id == w.journal().device_id()), "no network history pulled");
    let e = w.journal().capture_text("from the temporary peer").unwrap();
    w.sync_events_with(&a.addr()).await.unwrap();
    assert!(a.journal().store_events_for_test().iter().any(|x| x.event_id == e.event_id));
    // toss it: nothing else needs it
    w.shutdown().await;
    std::fs::remove_dir_all(dir.path().join("w")).unwrap();
    assert!(a.journal().store_events_for_test().iter().any(|x| x.event_id == e.event_id), "its events live on in the network");
    a.shutdown().await;
}
