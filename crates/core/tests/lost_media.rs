//! `media_lost` infra event: a blob the log references that no device holds
//! any more is declared gone for good. It stops counting as "missing" (so the
//! health light can go green again) and is never fetched, but the capture and
//! the event stay in the log (append-only) and the status says how many.

use memorious_core::event::MediaKind;
use memorious_core::node::Node;
use memorious_core::Journal;
use tempfile::tempdir;

#[tokio::test]
async fn lost_media_stops_counting_as_missing_and_syncs() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let secret = *ja.secret();
    let a = Node::spawn(ja).await.unwrap();
    a.capture_blob(MediaKind::Photo, b"first".to_vec()).await.unwrap();

    let jb = Journal::init_with_secret(&dir.path().join("b"), secret, "pw").unwrap();
    let b = Node::spawn(jb).await.unwrap();
    b.sync_with(&a.addr()).await.unwrap();

    // A second photo reaches b as an event only, and the bytes never follow.
    a.capture_blob(MediaKind::Photo, b"second".to_vec()).await.unwrap();
    b.sync_events_with(&a.addr()).await.unwrap();
    let tally = b.media_held().await.unwrap();
    assert_eq!((tally.referenced, tally.held, tally.lost), (2, 1, 0));
    assert_eq!(tally.missing(), 1);

    // Declared lost on b: no longer missing, and the fact is a log event.
    let target = a.journal().store.referenced_blob_hashes().unwrap()[1].clone();
    let ev = b.journal().mark_media_lost(&target, Some("source device wiped")).unwrap();
    assert!(ev.is_some());
    // Idempotent: declaring it twice appends nothing.
    assert!(b.journal().mark_media_lost(&target, None).unwrap().is_none());
    let tally = b.media_held().await.unwrap();
    assert_eq!((tally.held, tally.lost), (1, 1));
    assert_eq!(tally.missing(), 0);
    let lost = b.journal().lost_media().unwrap();
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].hash, target);
    assert_eq!(lost[0].reason.as_deref(), Some("source device wiped"));

    // It travels: a learns it from b and stops counting it too.
    a.sync_with(&b.addr()).await.unwrap();
    assert_eq!(a.journal().lost_media().unwrap().len(), 1);

    a.shutdown().await;
    b.shutdown().await;
}
