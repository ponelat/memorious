//! Isolating a peer: a new address, no peers, no sync in or out, events untouched — and a clean rejoin that exchanges only
//! what each side lacks.

use memorious_core::node::Node;
use memorious_core::Journal;
use tempfile::tempdir;

fn dump(j: &Journal) -> Vec<String> {
    // the whole log as stored JSON, in a deterministic order
    j.store_events_for_test().into_iter().map(|e| serde_json::to_string(&e).unwrap()).collect()
}

#[tokio::test]
async fn isolating_resets_connections_and_leaves_events_byte_identical() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("a");
    let j = Journal::init(&path, "pw").unwrap();
    for t in ["one", "two", "three"] {
        j.capture_text(t).unwrap();
    }
    // some peer bookkeeping the way a sync would leave it
    j.store_meta_for_test("peer_last_ok:abc", &5i64.to_le_bytes());
    j.store_meta_for_test("peer_heads:abc", b"{}");
    j.store_meta_for_test("peer_optional:abc", b"1");
    j.store_meta_for_test("last_peer_ticket", b"ticket");
    j.store_meta_for_test("last_sync_heads", b"{}");
    let before = dump(&j);
    let device = j.device_id().to_string();

    let a = Node::spawn(j).await.unwrap();
    let old_id = a.endpoint().id();
    let report = a.journal().isolate().unwrap();
    assert_eq!(report.peers_removed, 1);
    assert!(report.address_reset);
    assert!(a.journal().is_isolated().unwrap());
    assert!(a.peers().await.unwrap().is_empty());
    assert_eq!(dump(a.journal()), before, "not one event changed");
    a.shutdown().await;

    // restart: same journal, a different address, still isolated
    let j = Journal::open(&path, "pw").unwrap();
    assert_eq!(j.device_id(), device);
    assert!(j.is_isolated().unwrap());
    let a = Node::spawn(j).await.unwrap();
    assert_ne!(a.endpoint().id(), old_id, "a new address");
    assert!(a.journal().store_meta_scan_for_test("peer_").is_empty() && a.journal().store_meta_scan_for_test("last_peer_ticket").is_empty());

    // it keeps capturing: same device, the seq counter simply continues
    let e = a.journal().capture_text("alone").unwrap();
    assert_eq!(e.device_id, device);
    assert_eq!(e.seq, before.len() as u64 + 1, "the seq counter continues");
    assert_eq!(&dump(a.journal())[..before.len()], &before[..], "old events still identical, new one appended");
    a.shutdown().await;
}

#[tokio::test]
async fn an_isolated_peer_refuses_sync_in_both_directions_then_rejoins_cleanly() {
    let dir = tempdir().unwrap();
    let pa = dir.path().join("a");
    let ja = Journal::init(&pa, "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();
    for t in ["one", "two", "three"] {
        a.journal().capture_text(t).unwrap();
    }
    b.sync_with(&a.addr()).await.unwrap();
    let shared = dump(a.journal());
    let base = shared.len(); // 3 captures + the journal's own password-proof event
    assert_eq!(dump(b.journal()), shared);

    // isolate A (while running it already refuses); then restart it for the new address
    a.journal().isolate().unwrap();
    assert!(b.sync_with(&a.addr()).await.is_err(), "inbound sync is refused while isolated");
    a.shutdown().await;
    let a = Node::spawn(Journal::open(&pa, "pw").unwrap()).await.unwrap();

    // life goes on separately
    a.journal().capture_text("a-new-1").unwrap();
    a.journal().capture_text("a-new-2").unwrap();
    b.journal().capture_text("b-new").unwrap();

    assert!(a.sync_with(&b.addr()).await.is_err(), "outbound sync is refused while isolated");
    assert!(b.sync_with(&a.addr()).await.is_err(), "and so is inbound, at the new address");
    assert_eq!(dump(b.journal()).len(), base + 1, "B received nothing from isolated A");

    // rejoin: end isolation, sync once
    a.journal().end_isolation().unwrap();
    b.sync_with(&a.addr()).await.unwrap();

    let (da, db) = (dump(a.journal()), dump(b.journal()));
    assert_eq!(da.len(), base + 3, "shared + 2 from A + 1 from B, nothing duplicated");
    let ids = |v: &Vec<String>| {
        let mut ids: Vec<String> = v.iter().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["event_id"].as_str().unwrap().to_string()).collect();
        ids.sort();
        ids
    };
    assert_eq!(ids(&da), ids(&db), "both sides hold exactly the same events");
    let mut dedup = ids(&da);
    dedup.dedup();
    assert_eq!(dedup.len(), base + 3);
    // every event that existed before the isolation is byte-for-byte what it was, on both sides
    for old in &shared {
        assert!(da.contains(old) && db.contains(old), "an old event changed: {old}");
    }
    // and A's new events reached B exactly as A wrote them
    for line in da.iter().filter(|l| l.contains("a-new")) {
        assert!(db.contains(line));
    }
    a.shutdown().await;
    b.shutdown().await;
}
