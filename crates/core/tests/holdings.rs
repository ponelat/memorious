//! Per-peer holdings: after every handshake we remember what the peer holds
//! (heads = the version-vector "ack", plus its own media tally), so status
//! can say "peer has x of y" instead of only "last seen at". Health goes
//! yellow while any known peer is behind — on events, or on media its
//! retention policy doesn't excuse — and green once everyone holds
//! everything.

use memorious_core::event::MediaKind;
use memorious_core::node::Node;
use memorious_core::Journal;
use tempfile::tempdir;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn peers_report_events_and_media_held() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let secret = *ja.secret();
    let a = Node::spawn(ja).await.unwrap();
    a.journal().capture_text("one").unwrap();
    a.capture_blob(MediaKind::Photo, b"pixels".to_vec()).await.unwrap();

    let jb = Journal::init_with_secret(&dir.path().join("b"), secret, "pw").unwrap();
    let b = Node::spawn(jb).await.unwrap();
    b.sync_with(&a.addr()).await.unwrap();

    // Both sides converged on events; both remember the other's heads.
    let a_heads = a.journal().store.heads().unwrap();
    assert_eq!(a_heads, b.journal().store.heads().unwrap());
    let a_sees_b = &a.peers().await.unwrap()[0];
    assert_eq!(a_sees_b.heads.as_ref(), Some(&a_heads));
    assert_eq!(a_sees_b.events_held, Some(2));
    assert_eq!(a_sees_b.events_missing, Some(0));
    let b_sees_a = &b.peers().await.unwrap()[0];
    assert_eq!(b_sees_a.heads.as_ref(), Some(&a_heads));
    assert_eq!(b_sees_a.events_missing, Some(0));

    // a reported its media in HelloAck: one referenced blob, held.
    let media = b_sees_a.media.clone().expect("a's media tally");
    assert_eq!((media.referenced, media.held, media.evictable), (1, 1, 0));
    assert_eq!(b_sees_a.media_missing, Some(0));
    // b's Hello went out before it had any events: an empty, complete tally.
    let media = a_sees_b.media.clone().expect("b's media tally");
    assert_eq!((media.referenced, media.held), (0, 0));
    assert_eq!(a_sees_b.media_missing, Some(0));

    // Status carries the totals the UI pairs the per-peer numbers with.
    let status = a.status_json().await.unwrap();
    assert_eq!(status["events_total"], 2);
    assert_eq!(status["media"]["held"], 1);
    assert_eq!(status["peers"][0]["events_held"], 2);
    assert_eq!(status["health"]["color"], "green");
    assert_eq!(status["health"]["peers_behind"], 0);

    // New local event: b is now behind by one, judged live against our heads.
    a.journal().capture_text("two").unwrap();
    assert_eq!(a.peers().await.unwrap()[0].events_missing, Some(1));
    let health = a.journal().sync_health(now_ms()).unwrap();
    assert_eq!(health.color, "yellow");
    assert_eq!(health.peers_behind, 1);

    // Another round: b caught up, a's record of it says so.
    b.sync_with(&a.addr()).await.unwrap();
    assert_eq!(a.peers().await.unwrap()[0].events_missing, Some(0));
    assert_eq!(a.journal().sync_health(now_ms()).unwrap().color, "green");

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn media_behind_is_yellow_until_fetched() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let secret = *ja.secret();
    let a = Node::spawn(ja).await.unwrap();
    a.capture_blob(MediaKind::Photo, b"first".to_vec()).await.unwrap();

    let jb = Journal::init_with_secret(&dir.path().join("b"), secret, "pw").unwrap();
    let b = Node::spawn(jb).await.unwrap();
    b.sync_with(&a.addr()).await.unwrap();

    // A second photo; b takes the event only (no blob fetch).
    a.capture_blob(MediaKind::Photo, b"second".to_vec()).await.unwrap();
    b.sync_events_with(&a.addr()).await.unwrap();
    // b's Hello in that round predates the new event: 1 of 1. Heads converged.
    assert_eq!(a.peers().await.unwrap()[0].events_missing, Some(0));
    // One more handshake and b confesses: 2 referenced, 1 held, nothing excused.
    b.sync_events_with(&a.addr()).await.unwrap();
    let seen = &a.peers().await.unwrap()[0];
    let media = seen.media.clone().unwrap();
    assert_eq!((media.referenced, media.held, media.evictable), (2, 1, 0));
    assert_eq!(seen.media_missing, Some(1));
    let health = a.journal().sync_health(now_ms()).unwrap();
    assert_eq!(health.color, "yellow", "heads match but media is behind");
    assert_eq!(health.peers_behind, 1);

    // A full sync fetches the blob; the handshake after that clears it.
    b.sync_with(&a.addr()).await.unwrap();
    b.sync_events_with(&a.addr()).await.unwrap();
    let seen = &a.peers().await.unwrap()[0];
    assert_eq!(seen.media_missing, Some(0));
    assert_eq!(a.journal().sync_health(now_ms()).unwrap().color, "green");

    a.shutdown().await;
    b.shutdown().await;
}
