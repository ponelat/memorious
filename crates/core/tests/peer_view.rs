//! What the Peers page needs beyond heads: each device publishes which UTC 5-minute slots it was alive in (carried in the
//! handshake, 7-day window), we record which slots our contact with a peer worked or failed, and "newest it held" is the
//! newest user-visible event within the peer's heads.

use memorious_core::alive::{slot_of, Slots, SLOT_MS};
use memorious_core::node::Node;
use memorious_core::Journal;
use tempfile::tempdir;

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

#[test]
fn slots_merge_adjacent_runs_and_prune_the_window() {
    let mut s = Slots::default();
    s.add(10);
    s.add(10);
    s.add(11);
    s.add(13);
    assert_eq!(s.runs, vec![[10, 2], [13, 1]]);
    s.add(12);
    assert_eq!(s.runs, vec![[10, 4]]);
    let mut o = Slots::default();
    o.add(2000);
    s.merge(&o);
    s.prune(2100); // window is 2016 slots: slot 10 (age 2090) is gone, 2000 stays
    assert!(!s.contains(10) && s.contains(2000));
}

#[tokio::test]
async fn alive_slots_and_link_outcomes_cross_the_handshake() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let secret = *ja.secret();
    let a = Node::spawn(ja).await.unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), secret, "pw").unwrap();
    let b = Node::spawn(jb).await.unwrap();
    // b was alive a while ago, in a slot a never saw
    let long_ago = now_ms() - 3 * 3600 * 1000;
    b.journal().touch_alive(long_ago).unwrap();

    b.sync_with(&a.addr()).await.unwrap();
    let now_slot = slot_of(now_ms());
    let b_ep = b.endpoint_id().to_string();
    let a_ep = a.endpoint_id().to_string();

    // a learned b's whole recent history, not just "now"
    let seen = a.journal().peer_alive(&b_ep).unwrap();
    assert!(seen.contains(slot_of(long_ago)), "history arrives with the handshake");
    assert!(seen.contains(now_slot) || seen.contains(now_slot - 1));
    // and b learned a's
    assert!(b.journal().peer_alive(&a_ep).unwrap().contains(now_slot) || b.journal().peer_alive(&a_ep).unwrap().contains(now_slot - 1));
    // both sides count the handshake as a working link in this slot
    let (ok, bad) = a.journal().link_slots(&b_ep).unwrap();
    assert!(ok.contains(now_slot) || ok.contains(now_slot - 1));
    assert!(bad.runs.is_empty());
    a.journal().note_link(&b_ep, false, now_ms() - 2 * SLOT_MS).unwrap();
    assert!(a.journal().link_slots(&b_ep).unwrap().1.contains(slot_of(now_ms() - 2 * SLOT_MS)));

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn newest_held_is_judged_by_the_peers_heads() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let secret = *ja.secret();
    let a = Node::spawn(ja).await.unwrap();
    a.journal().capture_text("order bolts").unwrap();
    a.journal().capture_text("call Paula about the calendar").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), secret, "pw").unwrap();
    let b = Node::spawn(jb).await.unwrap();
    b.sync_with(&a.addr()).await.unwrap();
    a.journal().capture_text("third, written after the contact").unwrap();

    let peer = &a.peers().await.unwrap()[0];
    let held = peer.newest_held.as_ref().expect("b reported heads");
    assert_eq!((held.kind.as_str(), held.title.as_str()), ("text", "call Paula about the calendar"));
    let here = a.journal().newest_event(None).unwrap().unwrap();
    assert_eq!(here.title, "third, written after the contact");

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn newest_event_names_the_drawing_and_quotes_the_transcript() {
    use memorious_core::event::MediaKind;
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();

    // a voice note with a transcript: the line quotes the beginning of it, not just "Voice note"
    let voice = a.capture_blob(MediaKind::Audio, b"pcm".to_vec()).await.unwrap();
    a.journal().annotate(&voice.event_id, "Remember to order the bolts before Friday and ask about the longer ones").unwrap();
    let n = a.journal().newest_event(None).unwrap().unwrap();
    assert_eq!(n.kind, "voice");
    assert!(n.title.starts_with("Remember to order the bolts"), "{}", n.title);

    // a squiggle is newer: it shows as a drawing with its dot count and a normalised shape
    a.journal()
        .append_ink_batch(
            "2026-W48",
            vec![serde_json::json!({"op":"stroke","tool":"pen","seed":1,"size":2,"cid":"c1","pts":[[0,0,1,0],[10,5,1,1],[20,0,1,2],[30,9,1,3]]})],
        )
        .unwrap();
    let n = a.journal().newest_event(None).unwrap().unwrap();
    assert_eq!((n.kind.as_str(), n.points), ("ink", Some(4)));
    let shape = n.shape.expect("a squiggle to draw");
    assert_eq!(shape.len(), 4);
    assert!(shape.iter().all(|p| (0.0..=1.0).contains(&p[0]) && (0.0..=1.0).contains(&p[1])));

    a.shutdown().await;
}

#[tokio::test]
async fn a_failed_attempt_is_visible_and_a_nudge_pushes_soon() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let secret = *ja.secret();
    let a = Node::spawn(ja).await.unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), secret, "pw").unwrap();
    let b = Node::spawn(jb).await.unwrap();
    a.journal().capture_text("hello").unwrap();
    a.sync_with(&b.addr()).await.unwrap();

    // contact worked: no failure on record, nothing in flight
    let p = &a.peers().await.unwrap()[0];
    assert!(!p.syncing && p.last_error.is_none() && p.last_attempt_ms.is_some());

    // the schedule the server publishes comes back in status
    a.set_sync_schedule(1_000, 900_000);
    let st = a.status_json().await.unwrap();
    assert_eq!(st["sync"]["next_ms"], 1_000);
    assert_eq!(st["sync"]["interval_ms"], 900_000);

    // b goes away; a's next attempt fails and says why
    b.shutdown().await;
    let ep = a.peers().await.unwrap()[0].endpoint_id.clone();
    let ping = a.ping_peer(&ep, None, std::time::Duration::from_secs(2)).await;
    assert!(!ping.ok);
    let p = &a.peers().await.unwrap()[0];
    assert!(p.last_error.is_some() && !p.syncing);

    // a nudge: only the latest of a burst fires, and it is announced while pending
    let g1 = a.nudge_begin(std::time::Duration::from_secs(5));
    let g2 = a.nudge_begin(std::time::Duration::from_secs(5));
    assert!(a.status_json().await.unwrap()["sync"]["nudge_at_ms"].as_i64().unwrap() > 0);
    assert!(!a.nudge_take(g1), "a newer nudge supersedes");
    assert!(a.nudge_take(g2));
    assert_eq!(a.status_json().await.unwrap()["sync"]["nudge_at_ms"], 0);
    a.shutdown().await;
}
