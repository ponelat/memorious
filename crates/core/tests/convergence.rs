//! M1 acceptance: two peers, offline captures, sync → identical timelines, media included.

use memorious_core::node::Node;
use memorious_core::Journal;
use tempfile::tempdir;

fn timeline_ids(j: &Journal) -> Vec<String> {
    j.list()
        .unwrap()
        .iter()
        .map(|e| e.event_id.clone())
        .collect()
}

#[tokio::test]
async fn two_peers_converge_including_media() {
    let dir = tempdir().unwrap();

    // Journal born on A; B joins with A's secret (as ticket redemption would).
    let ja = Journal::init(&dir.path().join("a")).unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret()).unwrap();

    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();

    // Captures made "offline of each other".
    a.journal().capture_text("from a: one").unwrap();
    a.journal().capture_text("from a: two").unwrap();
    b.journal().capture_text("from b: one").unwrap();
    let photo_bytes = b"not really a jpeg but bytes all the same".to_vec();
    let photo = a
        .capture_blob(memorious_core::event::MediaKind::Photo, photo_bytes.clone())
        .await
        .unwrap();

    // One round-trip sync started from B.
    b.sync_with(&a.addr()).await.unwrap();

    assert_eq!(timeline_ids(a.journal()), timeline_ids(b.journal()));
    assert_eq!(a.journal().list().unwrap().len(), 4);

    // Media arrived on B, byte-identical.
    let hash = photo.blob_hash().unwrap();
    let got = b.blob_bytes(hash).await.unwrap();
    assert_eq!(got, photo_bytes);

    // Redaction made after sync propagates on the next sync.
    let toss = b.journal().capture_text("delete me").unwrap();
    b.journal().redact(&toss.event_id).unwrap();
    b.sync_with(&a.addr()).await.unwrap();
    assert_eq!(timeline_ids(a.journal()), timeline_ids(b.journal()));

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn wrong_secret_is_rejected() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a")).unwrap();
    let jb = Journal::init(&dir.path().join("b")).unwrap(); // different journal, own secret

    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();

    a.journal().capture_text("private").unwrap();
    assert!(b.sync_with(&a.addr()).await.is_err());
    assert_eq!(b.journal().list().unwrap().len(), 0);

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn interrupted_sync_converges_on_retry() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a")).unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret()).unwrap();

    let a = Node::spawn(ja).await.unwrap();
    for i in 0..50 {
        a.journal().capture_text(&format!("entry {i}")).unwrap();
    }

    // First attempt dies mid-flight: B connects, then is dropped hard.
    {
        let b = Node::spawn(Journal::open(&dir.path().join("b")).unwrap())
            .await
            .unwrap();
        let addr = a.addr();
        let handle = tokio::spawn(async move { b.sync_with(&addr).await.map(|_| ()) });
        // Let it get partway, then abort the task (connection torn down mid-protocol).
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        handle.abort();
        let _ = handle.await;
    }

    // Retry with a fresh node over the same data dir must converge, not error.
    let b = Node::spawn(Journal::open(&dir.path().join("b")).unwrap())
        .await
        .unwrap();
    b.sync_with(&a.addr()).await.unwrap();
    assert_eq!(timeline_ids(a.journal()), timeline_ids(b.journal()));
    assert_eq!(b.journal().list().unwrap().len(), 50);

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn ticket_pairing_round_trip() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a")).unwrap();
    let a = Node::spawn(ja).await.unwrap();
    a.journal().capture_text("hello new device").unwrap();

    // Ticket string carries secret + address; a fresh device joins from it alone.
    let ticket = a.ticket().unwrap();
    let (b, report) = Node::join_from_ticket(&dir.path().join("b"), &ticket)
        .await
        .unwrap();
    assert!(report.received > 0);
    assert_eq!(timeline_ids(a.journal()), timeline_ids(b.journal()));

    a.shutdown().await;
    b.shutdown().await;
}
