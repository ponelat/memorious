//! Required vs optional peers: the traffic light is judged by the peers marked
//! required (the default), not by every peer ever seen. An optional peer that is
//! stale or behind never turns the light red or yellow; with no required peer at
//! all there is nothing to be late to, so it is green. The mark is a local
//! preference of this device (it is not synced).

use memorious_core::node::Node;
use memorious_core::store::Heads;
use memorious_core::Journal;
use tempfile::tempdir;

const HOUR: i64 = 3600 * 1000;
const NOW: i64 = 1_800_000_000_000;

fn journal() -> (tempfile::TempDir, Journal) {
    let dir = tempdir().unwrap();
    let j = Journal::init(&dir.path().join("j"), "pw").unwrap();
    (dir, j)
}

#[test]
fn peers_are_required_by_default_and_the_mark_sticks() {
    let (_d, j) = journal();
    j.record_sync_contact("p1", NOW, None, "ticket").unwrap();
    assert!(j.peer_required("p1").unwrap(), "unmarked peers count, as before");
    j.set_peer_required("p1", false).unwrap();
    assert!(!j.peer_required("p1").unwrap());
    j.set_peer_required("p1", true).unwrap();
    assert!(j.peer_required("p1").unwrap());
}

#[test]
fn an_optional_peer_never_turns_health_red() {
    let (_d, j) = journal();
    j.record_sync_contact("fresh", NOW, None, "ticket").unwrap();
    j.record_sync_contact("stale", NOW - 49 * HOUR, None, "ticket").unwrap();
    let h = j.sync_health(NOW).unwrap();
    assert_eq!(h.color, "red", "a required peer unheard-from for 48h is red");
    assert_eq!((h.peers, h.required_peers), (2, 2));

    j.set_peer_required("stale", false).unwrap();
    let h = j.sync_health(NOW).unwrap();
    assert_eq!(h.color, "green", "the stale peer is optional now");
    assert_eq!((h.peers, h.required_peers), (2, 1), "still known, just not judged");

    j.set_peer_required("stale", true).unwrap();
    assert_eq!(j.sync_health(NOW).unwrap().color, "red");
}

#[test]
fn an_optional_peer_that_is_behind_does_not_make_health_yellow() {
    let (_d, j) = journal();
    j.capture_text("one").unwrap();
    let ours = j.store.heads().unwrap();
    // Both peers were seen just now; one holds everything, the other nothing.
    j.record_sync_contact("current", NOW, None, "ticket").unwrap();
    j.record_peer_holdings("current", &ours, None).unwrap();
    j.record_sync_contact("behind", NOW, None, "ticket").unwrap();
    j.record_peer_holdings("behind", &Heads::new(), None).unwrap();

    let h = j.sync_health(NOW).unwrap();
    assert_eq!((h.color.as_str(), h.peers_behind), ("yellow", 1));

    j.set_peer_required("behind", false).unwrap();
    let h = j.sync_health(NOW).unwrap();
    assert_eq!((h.color.as_str(), h.peers_behind), ("green", 0));

    // A required peer that falls behind still counts.
    j.set_peer_required("current", true).unwrap();
    j.capture_text("two").unwrap();
    j.record_sync_contact("behind", NOW, None, "ticket").unwrap();   // optional peer syncs; "current" doesn't
    let h = j.sync_health(NOW).unwrap();
    assert_eq!((h.color.as_str(), h.peers_behind), ("yellow", 1), "required peer lacks event two");
}

#[test]
fn with_no_required_peer_health_is_green() {
    let (_d, j) = journal();
    j.capture_text("one").unwrap();
    j.record_sync_contact("a", NOW - 100 * HOUR, None, "ticket").unwrap();
    j.record_sync_contact("b", NOW - 100 * HOUR, None, "ticket").unwrap();
    assert_eq!(j.sync_health(NOW).unwrap().color, "red");
    j.set_peer_required("a", false).unwrap();
    j.set_peer_required("b", false).unwrap();
    let h = j.sync_health(NOW).unwrap();
    assert_eq!(h.color, "green", "nothing required, nothing late");
    assert_eq!((h.peers, h.required_peers, h.peers_behind), (2, 0, 0));
    assert!(!h.pending);
}

#[tokio::test]
async fn status_reports_which_peers_are_required() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let secret = *ja.secret();
    let a = Node::spawn(ja).await.unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), secret, "pw").unwrap();
    let b = Node::spawn(jb).await.unwrap();
    b.sync_with(&a.addr()).await.unwrap();

    let peers = a.peers().await.unwrap();
    assert!(peers[0].required, "a peer is required until marked otherwise");
    let id = peers[0].endpoint_id.clone();
    a.journal().set_peer_required(&id, false).unwrap();

    let status = a.status_json().await.unwrap();
    assert_eq!(status["peers"][0]["required"], false);
    assert_eq!(status["health"]["required_peers"], 0);
    assert_eq!(status["health"]["peers"], 1);

    a.shutdown().await;
    b.shutdown().await;
}
