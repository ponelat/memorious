//! Superseded ink, device-local retention, declared peer policy, stub counts and filling stubs from a peer that still has
//! the full events. Design: https://burrow.ponelat.com/files/memorious/event-model-proposal.md (and the thread after it):
//! an event may SUPERSEDE earlier ones (it never deletes anything); each device's own retention policy decides whether it
//! keeps the superseded events or collects them down to stubs; a device announces whether it keeps revisions with a
//! `peer_policy` infra event; a device that keeps revisions asks peers for full copies of its stubs.

use memorious_core::node::Node;
use memorious_core::retention::{Retain, RetentionPolicy};
use memorious_core::store::now_ms;
use memorious_core::{Event, Journal, Payload};
use serde_json::{json, Value};
use tempfile::tempdir;

const DAY: i64 = 86_400_000;

fn journal() -> (tempfile::TempDir, Journal) {
    let dir = tempdir().unwrap();
    let j = Journal::init(&dir.path().join("j"), "pw").unwrap();
    (dir, j)
}

fn stroke(n: u32) -> Value {
    json!({ "op": "stroke", "cid": format!("c{n}"), "tool": "pen", "seed": n, "size": 5, "pts": [[n, n, 0.5, 0]] })
}

/// `n` strokes in a month, then an ink_fold op that supersedes exactly those (by per-device seq bound).
fn month_with_snapshot(j: &Journal, month: &str, n: u32) -> (Vec<Event>, Event) {
    let ops: Vec<Event> = (0..n).map(|i| j.append_ink(month, stroke(i)).unwrap()).collect();
    let bound = ops.last().unwrap().seq;
    let snap = j
        .append_ink(month, json!({ "op": "ink_fold", "cid": "snap", "items": [], "supersedes": [{ "select": "ink", "period": month, "up_to": { j.device_id(): bound } }] }))
        .unwrap();
    (ops, snap)
}

fn is_stub(e: &Event) -> bool {
    matches!(&e.payload, Payload::Unknown { type_tag, .. } if type_tag == "collected")
}

fn by_id(j: &Journal, id: &str) -> Event {
    j.store_events_for_test().into_iter().find(|e| e.event_id == id).unwrap()
}

/// A phone-like policy that collects without needing a known backup (the guard has its own test).
fn phone() -> RetentionPolicy {
    RetentionPolicy { collect_without_backup: true, ..RetentionPolicy::PHONE }
}

#[test]
fn a_device_that_keeps_revisions_never_collects() {
    let (_d, j) = journal(); // default policy: keep everything
    let (ops, _) = month_with_snapshot(&j, "2026-10", 3);
    assert_eq!(j.collect_superseded_at(now_ms() + 3650 * DAY).unwrap().events_collected, 0);
    assert!(ops.iter().all(|o| !is_stub(&by_id(&j, &o.event_id))));
}

#[test]
fn a_phone_collects_superseded_ops_after_its_window_counted_from_when_it_got_the_snapshot() {
    let (_d, j) = journal();
    j.set_retention_policy(&phone()).unwrap();
    let (ops, snap) = month_with_snapshot(&j, "2026-10", 3);
    let keep = j.append_ink("2026-11", stroke(9)).unwrap(); // another month, nothing supersedes it
    let heads = j.store_heads_for_test();

    assert_eq!(j.collect_superseded_at(now_ms() + 29 * DAY).unwrap().events_collected, 0, "inside the 30-day window");
    assert_eq!(j.collect_superseded_at(now_ms() + 31 * DAY).unwrap().events_collected, 3);
    for o in &ops {
        let s = by_id(&j, &o.event_id);
        assert!(is_stub(&s));
        // the envelope is untouched: sync's contiguity depends on (device_id, seq)
        assert_eq!((&s.device_id, s.seq, s.recorded_at, &s.kind), (&o.device_id, o.seq, o.recorded_at, &o.kind));
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["payload"]["by"], snap.event_id.as_str());
        assert_eq!(v["payload"]["was"], "ink");
    }
    assert!(!is_stub(&by_id(&j, &snap.event_id)) && !is_stub(&by_id(&j, &keep.event_id)));
    assert_eq!(j.store_heads_for_test(), heads, "no seq changes");
    assert_eq!(j.collect_superseded_at(now_ms() + 40 * DAY).unwrap().events_collected, 0, "idempotent");
    // counts for the detail pages
    let c = j.stub_counts().unwrap();
    assert_eq!(c.total, 3);
    assert_eq!(c.by_type.get("ink"), Some(&3));
}

#[test]
fn collection_waits_until_a_backup_peer_is_known() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    ja.set_retention_policy(&RetentionPolicy::PHONE).unwrap(); // no override
    let (ops, _) = month_with_snapshot(&ja, "2026-10", 2);
    let far = now_ms() + 365 * DAY;
    assert_eq!(ja.collect_superseded_at(far).unwrap().events_collected, 0, "no peer is known to keep revisions");
    assert!(!is_stub(&by_id(&ja, &ops[0].event_id)));

    // a second device declares it keeps revisions (its announcement syncs like any event)
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    jb.ensure_peer_policy().unwrap(); // default policy keeps everything
    for e in jb.store_events_for_test().iter().filter(|e| e.device_id == jb.device_id()) {
        ja.store_insert_remote_for_test(e);
    }
    assert!(ja.has_backup_peer().unwrap());
    assert_eq!(ja.collect_superseded_at(far).unwrap().events_collected, 2);
}

#[test]
fn only_ink_can_be_collected_whatever_an_event_claims_to_supersede() {
    let (_d, j) = journal();
    j.set_retention_policy(&phone()).unwrap();
    let text = j.capture_text("precious").unwrap();
    let note = j.annotate(&text.event_id, "a note").unwrap();
    j.append_ink("2026-10", json!({ "op": "ink_fold", "cid": "bad", "supersedes": [{ "id": text.event_id }, { "id": note.event_id }] })).unwrap();
    assert_eq!(j.collect_superseded_at(now_ms() + 365 * DAY).unwrap().events_collected, 0);
    assert!(!is_stub(&by_id(&j, &text.event_id)) && !is_stub(&by_id(&j, &note.event_id)));
}

#[test]
fn the_latest_snapshot_wins_and_a_replaced_snapshot_is_collected_too() {
    let (_d, j) = journal();
    j.set_retention_policy(&phone()).unwrap();
    let (ops, s1) = month_with_snapshot(&j, "2026-10", 2);
    // a re-snapshot (e.g. after an erase) supersedes the old snapshot as well
    let s2 = j.append_ink("2026-10", json!({ "op": "ink_fold", "cid": "snap2", "items": [], "supersedes": [{ "id": s1.event_id }] })).unwrap();
    let n = j.collect_superseded_at(now_ms() + 31 * DAY).unwrap().events_collected;
    assert_eq!(n, 3, "the two ops and the old snapshot");
    assert!(is_stub(&by_id(&j, &s1.event_id)) && is_stub(&by_id(&j, &ops[0].event_id)));
    assert!(!is_stub(&by_id(&j, &s2.event_id)), "the newest snapshot keeps its content");
}

#[test]
fn announcing_whether_a_device_keeps_revisions_is_an_infra_event_and_the_latest_wins() {
    let (_d, j) = journal();
    let policies = |j: &Journal| j.peer_policies().unwrap();
    assert!(policies(&j).is_empty());
    j.ensure_peer_policy().unwrap();
    assert_eq!(policies(&j).get(j.device_id()), Some(&true), "default keeps everything");
    let n = j.infra_events("peer_policy").unwrap().len();
    j.ensure_peer_policy().unwrap();
    assert_eq!(j.infra_events("peer_policy").unwrap().len(), n, "nothing re-announced while it hasn't changed");

    j.set_retention_policy(&RetentionPolicy::PHONE).unwrap(); // announces by itself
    assert_eq!(policies(&j).get(j.device_id()), Some(&false));
    j.set_retention_policy(&RetentionPolicy::KEEP_EVERYTHING).unwrap();
    assert_eq!(policies(&j).get(j.device_id()), Some(&true));
    let e = j.infra_events("peer_policy").unwrap().pop().unwrap();
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(v["payload"]["infra_kind"], "peer_policy");
    assert_eq!(v["payload"]["target"], j.device_id());
    assert_eq!(v["payload"]["data"]["keeps_revisions"], true);
}

#[test]
fn an_old_stored_policy_without_the_new_fields_still_loads_as_keep_revisions() {
    // policies saved before the superseded rule existed
    let old = json!({ "voice_audio": { "mode": "after_days", "days": 7 }, "redacted": { "mode": "forever" } });
    let p: RetentionPolicy = serde_json::from_value(old).unwrap();
    assert_eq!(p.voice_audio, Retain::AfterDays(7));
    assert_eq!(p.superseded, Retain::Forever);
    assert!(!p.collect_without_backup);
}

#[tokio::test]
async fn status_reports_stub_counts() {
    let dir = tempdir().unwrap();
    let j = Journal::init(&dir.path().join("j"), "pw").unwrap();
    j.set_retention_policy(&phone()).unwrap();
    month_with_snapshot(&j, "2026-10", 4);
    j.collect_superseded_at(now_ms() + 31 * DAY).unwrap();
    let node = Node::spawn(j).await.unwrap();
    let st = node.status_json().await.unwrap();
    assert_eq!(st["stubs"]["total"], 4);
    assert_eq!(st["stubs"]["by_type"]["ink"], 4);
    assert_eq!(st["keeps_revisions"], false);
    node.shutdown().await;
}

#[tokio::test]
async fn a_peer_that_keeps_revisions_fills_its_stubs_from_a_peer_that_still_has_the_full_events() {
    let dir = tempdir().unwrap();
    // A: a backup (keeps everything) that holds the full history
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let (ops, snap) = month_with_snapshot(&ja, "2026-10", 3);
    let jp = Journal::init_with_secret(&dir.path().join("p"), *ja.secret(), "pw").unwrap();
    jp.set_retention_policy(&phone()).unwrap();
    let jc = Journal::init_with_secret(&dir.path().join("c"), *ja.secret(), "pw").unwrap();
    let (a, p, c) = (Node::spawn(ja).await.unwrap(), Node::spawn(jp).await.unwrap(), Node::spawn(jc).await.unwrap());

    // the phone syncs from A, then collects what the snapshot superseded
    p.sync_events_with(&a.addr()).await.unwrap();
    assert_eq!(p.journal().collect_superseded_at(now_ms() + 31 * DAY).unwrap().events_collected, 3);
    assert_eq!(p.journal().stub_counts().unwrap().total, 3);

    // a new backup C pulls from the phone first: it gets the snapshot and STUBS
    c.sync_events_with(&p.addr()).await.unwrap();
    assert_eq!(c.journal().stub_counts().unwrap().total, 3, "C holds stubs for now");
    assert!(!is_stub(&by_id(c.journal(), &snap.event_id)));

    // ...then connects to A, which has the full events: C asks for them, because its policy keeps revisions
    let r = c.sync_events_with(&a.addr()).await.unwrap();
    assert_eq!(r.stubs_filled, 3);
    assert_eq!(c.journal().stub_counts().unwrap().total, 0);
    for o in &ops {
        assert_eq!(serde_json::to_value(by_id(c.journal(), &o.event_id)).unwrap(), serde_json::to_value(o).unwrap(), "byte-identical to the original");
    }

    // the phone does not keep revisions, so it never asks A to refill its stubs
    let r = p.sync_events_with(&a.addr()).await.unwrap();
    assert_eq!(r.stubs_filled, 0);
    assert_eq!(p.journal().stub_counts().unwrap().total, 3);
    for n in [a, p, c] {
        n.shutdown().await;
    }
}

#[test]
fn a_fill_must_match_the_stub_envelope_and_never_overwrites_a_full_event() {
    let (_d, j) = journal();
    j.set_retention_policy(&phone()).unwrap();
    let (ops, _) = month_with_snapshot(&j, "2026-10", 1);
    let full = ops[0].clone();
    // a full event arriving for an id we hold in full is an ordinary duplicate
    assert!(!j.store_fill_for_test(&full), "never overwrites a full event");
    j.collect_superseded_at(now_ms() + 31 * DAY).unwrap();
    assert!(is_stub(&by_id(&j, &full.event_id)));
    // same id, different envelope (timestamp): refused
    let mut forged = full.clone();
    forged.recorded_at += 1;
    assert!(!j.store_fill_for_test(&forged));
    assert!(is_stub(&by_id(&j, &full.event_id)));
    // the genuine event fills it
    assert!(j.store_fill_for_test(&full));
    assert_eq!(serde_json::to_value(by_id(&j, &full.event_id)).unwrap(), serde_json::to_value(&full).unwrap());
}
