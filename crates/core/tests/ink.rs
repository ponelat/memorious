//! Ink on the infinite canvas: drawing ops are `infra` events (`infra_kind: "ink"`, `target` =
//! the canvas's UTC month), so they ride the ordinary sync, never show in the feed, and an
//! older peer stores and forwards them untouched.

use memorious_core::node::Node;
use memorious_core::Journal;
use serde_json::json;
use tempfile::tempdir;

fn journal() -> (tempfile::TempDir, Journal) {
    let dir = tempdir().unwrap();
    let j = Journal::init(&dir.path().join("j"), "pw").unwrap();
    (dir, j)
}

fn stroke(n: u32) -> serde_json::Value {
    json!({ "op": "stroke", "tool": "pen", "seed": n, "size": 5, "pts": [[0, 0, 0.5, 0], [10, 10, 0.5, 0]] })
}

/// Canvas order is (recorded_at, event_id) — the same on every peer, whatever order ops arrived in.
fn canvas_order(mut v: Vec<(i64, memorious_core::Event)>) -> Vec<memorious_core::Event> {
    v.sort_by(|a, b| (a.1.recorded_at, &a.1.event_id).cmp(&(b.1.recorded_at, &b.1.event_id)));
    v.into_iter().map(|(_, e)| e).collect()
}

#[test]
fn ink_ops_come_back_for_their_month_only() {
    let (_d, j) = journal();
    let a = j.append_ink("2026-11", stroke(1)).unwrap();
    let b = j.append_ink("2026-11", stroke(2)).unwrap();
    j.append_ink("2026-10", stroke(3)).unwrap();

    let nov = j.ink_events("2026-11", 0).unwrap();
    let ids: Vec<_> = canvas_order(nov).iter().map(|e| e.event_id.clone()).collect();
    assert_eq!(ids, vec![a.event_id.clone(), b.event_id.clone()]);
    assert_eq!(j.ink_events("2026-10", 0).unwrap().len(), 1);
    assert!(j.ink_events("2026-09", 0).unwrap().is_empty());
}

#[test]
fn a_cursor_returns_only_what_arrived_since() {
    let (_d, j) = journal();
    j.append_ink("2026-11", stroke(1)).unwrap();
    let first = j.ink_events("2026-11", 0).unwrap();
    let cursor = first.iter().map(|(r, _)| *r).max().unwrap();
    assert!(j.ink_events("2026-11", cursor).unwrap().is_empty());
    let b = j.append_ink("2026-11", stroke(2)).unwrap();
    let since = j.ink_events("2026-11", cursor).unwrap();
    assert_eq!(since.len(), 1);
    assert_eq!(since[0].1.event_id, b.event_id);
}

#[test]
fn ink_never_shows_in_the_feed() {
    let (_d, j) = journal();
    j.capture_text("hello").unwrap();
    j.append_ink("2026-11", stroke(1)).unwrap();
    assert_eq!(j.list().unwrap().len(), 1, "only the text capture is an entry");
}

#[test]
fn malformed_ink_is_refused() {
    let (_d, j) = journal();
    assert!(j.append_ink("november", stroke(1)).is_err(), "month must be YYYY-MM");
    assert!(j.append_ink("2026-13", stroke(1)).is_err(), "month 13 is not a month");
    assert!(j.append_ink("2026-11", json!({ "no": "op" })).is_err(), "every ink event names its op");
    assert!(j.append_ink("2026-11", json!("a string")).is_err());
}

#[test]
fn months_with_ink_are_listed() {
    let (_d, j) = journal();
    j.append_ink("2026-11", stroke(1)).unwrap();
    j.append_ink("2026-09", stroke(2)).unwrap();
    j.append_ink("2026-11", stroke(3)).unwrap();
    assert_eq!(j.ink_months().unwrap(), vec!["2026-09".to_string(), "2026-11".to_string()]);
}

#[tokio::test]
async fn ink_syncs_to_a_peer() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();

    a.journal().append_ink("2026-11", stroke(1)).unwrap();
    a.journal().append_ink("2026-11", json!({ "op": "undo", "target": "x" })).unwrap();
    b.journal().append_ink("2026-11", stroke(2)).unwrap();

    b.sync_with(&a.addr()).await.unwrap();

    let ids = |j: &Journal| canvas_order(j.ink_events("2026-11", 0).unwrap()).iter().map(|e| e.event_id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(a.journal()).len(), 3);
    assert_eq!(ids(a.journal()), ids(b.journal()), "both peers see the same ink in the same order");
}

#[tokio::test]
async fn an_op_that_syncs_in_late_is_still_after_the_readers_cursor() {
    // A drew something *before* B drew, but B only hears about it now. B's reader cursor is by arrival, not by
    // recorded_at, so the late op still shows up in the next poll.
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();

    let early = a.journal().append_ink("2026-11", stroke(1)).unwrap(); // older recorded_at
    std::thread::sleep(std::time::Duration::from_millis(5));
    b.journal().append_ink("2026-11", stroke(2)).unwrap();
    let cursor = b.journal().ink_events("2026-11", 0).unwrap().iter().map(|(r, _)| *r).max().unwrap();

    b.sync_with(&a.addr()).await.unwrap();
    let since = b.journal().ink_events("2026-11", cursor).unwrap();
    assert_eq!(since.len(), 1);
    assert_eq!(since[0].1.event_id, early.event_id);
}

#[test]
fn a_batch_is_all_or_nothing_for_validation() {
    let (_d, j) = journal();
    let evs = j.append_ink_batch("2026-11", vec![stroke(1), stroke(2)]).unwrap();
    assert_eq!(evs.len(), 2);
    assert!(j.append_ink_batch("2026-11", vec![stroke(3), json!({"no": "op"})]).is_err());
    assert_eq!(j.ink_events("2026-11", 0).unwrap().len(), 2, "the bad batch wrote nothing");
}

#[tokio::test]
async fn subscribers_hear_about_new_ink_and_only_ink() {
    let (_d, j) = journal();
    let mut rx = j.subscribe_ink();
    j.append_ink("2026-11", stroke(1)).unwrap();
    assert_eq!(rx.recv().await.unwrap(), "2026-11");
    j.capture_text("not ink").unwrap(); // a text capture is not announced
    j.append_ink("2026-10", stroke(2)).unwrap();
    assert_eq!(rx.recv().await.unwrap(), "2026-10", "the next thing heard is the next ink, for its own month");
}

#[tokio::test]
async fn ink_that_syncs_in_from_a_peer_is_announced_too() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();
    let mut rx = b.journal().subscribe_ink();

    a.journal().append_ink("2026-11", stroke(1)).unwrap();
    b.sync_with(&a.addr()).await.unwrap();

    let heard = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.expect("announced after the sync").unwrap();
    assert_eq!(heard, "2026-11");
}

/// The JSON the HTTP face and the mobile face both hand to a client — one shaper, so they can't drift.
#[test]
fn the_json_faces_share_one_shape() {
    let (_d, j) = journal();
    let out = j.ink_append_json("2026-11", vec![stroke(1), stroke(2)]).unwrap();
    let evs = out["events"].as_array().unwrap();
    assert_eq!(evs.len(), 2);
    assert!(evs[0]["event_id"].is_string() && evs[0]["recorded_at"].is_i64());

    let list = j.ink_list_json("2026-11", 0).unwrap();
    assert_eq!(list["device_id"], j.device_id());
    let got = list["events"].as_array().unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0]["data"]["op"], "stroke");
    assert_eq!(got[0]["device_id"], j.device_id());
    let cursor = list["cursor"].as_i64().unwrap();
    assert!(cursor > 0);
    // nothing new after the cursor: it comes back unchanged, with no events
    let again = j.ink_list_json("2026-11", cursor).unwrap();
    assert_eq!(again["cursor"], cursor);
    assert!(again["events"].as_array().unwrap().is_empty());
    assert!(j.ink_list_json("nope", 0).is_err());
    assert_eq!(j.ink_months_json().unwrap()["months"], json!(["2026-11"]));
}
