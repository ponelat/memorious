//! `derived` events (docs: https://burrow.ponelat.com/files/memorious/event-model-proposal.md): an event that carries
//! derived content, says where it came from and, optionally, which earlier events (or parts of them) it makes outdated.
//! Replaced events are collected locally, only after the lease, only by peers that keep just the effective set, and only
//! down to a stub that keeps the envelope so sync stays contiguous.

use memorious_core::node::Node;
use memorious_core::{Event, EventKind, Journal, Payload};
use serde_json::{json, Value};
use tempfile::tempdir;

const DAY: i64 = 86_400_000;
const T0: i64 = 1_800_000_000_000;

fn lease(ms: i64) -> Option<u64> {
    Some(ms as u64)
}

fn journal() -> (tempfile::TempDir, Journal) {
    let dir = tempdir().unwrap();
    let j = Journal::init(&dir.path().join("j"), "pw").unwrap();
    (dir, j)
}

fn stroke(n: u32) -> Value {
    json!({ "op": "stroke", "cid": format!("c{n}"), "tool": "pen", "seed": n, "size": 5, "pts": [[n, n, 0.5, 0]] })
}

fn ink_at(j: &Journal, month: &str, n: u32) -> Event {
    j.append_ink(month, stroke(n)).unwrap()
}

fn is_stub(e: &Event) -> bool {
    matches!(&e.payload, Payload::Unknown { type_tag, .. } if type_tag == "collected")
}

fn by_id(j: &Journal, id: &str) -> Event {
    j.store_events_for_test().into_iter().find(|e| e.event_id == id).unwrap()
}

#[test]
fn a_derived_event_round_trips_with_unknown_properties() {
    let (_d, j) = journal();
    let a = ink_at(&j, "2026-10", 1);
    let d = j
        .append_derived_at("ink_snapshot", json!([{ "id": a.event_id }]), json!([{ "id": a.event_id }]), lease(30 * DAY), json!({ "items": [1, 2], "future_prop": { "x": true } }), T0)
        .unwrap();
    assert_eq!(d.kind, EventKind::Derived);
    let v = serde_json::to_value(&d).unwrap();
    assert_eq!(v["kind"], "derived");
    assert_eq!(v["payload"]["type"], "ink_snapshot");
    assert_eq!(v["payload"]["lease_ms"], 30 * DAY);
    assert_eq!(v["payload"]["items"], json!([1, 2]));
    assert_eq!(v["payload"]["future_prop"], json!({ "x": true }));
    let back: Event = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(serde_json::to_value(&back).unwrap(), v);
    // and what the store holds is the same
    assert_eq!(serde_json::to_value(by_id(&j, &d.event_id)).unwrap(), v);
}

#[test]
fn nothing_is_collected_before_the_lease_and_everything_replaced_after() {
    let (_d, j) = journal();
    let ops: Vec<Event> = (0..3).map(|n| ink_at(&j, "2026-10", n)).collect();
    let keep = ink_at(&j, "2026-10", 9); // not replaced
    let heads_before = j.store_heads_for_test();
    let d = j
        .append_derived_at("ink_snapshot", json!([]), json!([{ "id": ops[0].event_id }, { "id": ops[1].event_id }, { "id": ops[2].event_id }]), lease(30 * DAY), json!({ "items": [] }), T0)
        .unwrap();

    assert_eq!(j.collect_at(T0 + 29 * DAY).unwrap().events_collected, 0, "inside the lease");
    assert!(ops.iter().all(|o| !is_stub(&by_id(&j, &o.event_id))));

    let report = j.collect_at(T0 + 30 * DAY).unwrap();
    assert_eq!(report.events_collected, 3);
    for o in &ops {
        let s = by_id(&j, &o.event_id);
        assert!(is_stub(&s), "replaced op became a stub");
        // the envelope is untouched: sync's contiguity depends on (device_id, seq)
        assert_eq!((&s.device_id, s.seq, s.recorded_at, &s.kind), (&o.device_id, o.seq, o.recorded_at, &o.kind));
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["payload"]["by"], d.event_id.as_str());
        assert_eq!(v["payload"]["was"], "ink");
    }
    assert!(!is_stub(&by_id(&j, &keep.event_id)), "events nothing replaced stay whole");
    assert!(!is_stub(&by_id(&j, &d.event_id)), "the replacement keeps its content");
    assert_eq!(j.store_heads_for_test(), heads_before.iter().map(|(k, v)| (k.clone(), *v + 1)).collect(), "only the derived event added a seq");
    assert_eq!(j.collect_at(T0 + 31 * DAY).unwrap().events_collected, 0, "idempotent");
}

#[test]
fn a_derived_event_with_no_replaces_changes_nothing() {
    let (_d, j) = journal();
    let a = ink_at(&j, "2026-10", 1);
    j.append_derived_at("summary", json!([{ "id": a.event_id }]), json!([]), None, json!({ "text": "a summary" }), T0).unwrap();
    assert_eq!(j.collect_at(T0 + 365 * DAY).unwrap().events_collected, 0);
    assert!(!is_stub(&by_id(&j, &a.event_id)));
}

#[test]
fn a_month_selector_replaces_by_per_device_seq_bound_and_late_ops_stay() {
    let (_d, j) = journal();
    let ops: Vec<Event> = (0..4).map(|n| ink_at(&j, "2026-10", n)).collect();
    let other_month = ink_at(&j, "2026-11", 7);
    let bound = ops[2].seq; // covers the first three only
    j.append_derived_at(
        "ink_snapshot",
        json!([]),
        json!([{ "select": "ink", "month": "2026-10", "up_to": { j.device_id(): bound } }]),
        lease(DAY),
        json!({ "items": [] }),
        T0,
    )
    .unwrap();
    // a legacy-form op in the same month is covered by the same selector
    assert_eq!(j.collect_at(T0 + 2 * DAY).unwrap().events_collected, 3);
    for o in &ops[..3] {
        assert!(is_stub(&by_id(&j, &o.event_id)));
    }
    assert!(!is_stub(&by_id(&j, &ops[3].event_id)), "an op above the bound (drawn later / synced late) stays live");
    assert!(!is_stub(&by_id(&j, &other_month.event_id)), "another month is untouched");
}

#[test]
fn a_keep_everything_peer_never_collects() {
    let (_d, j) = journal();
    let a = ink_at(&j, "2026-10", 1);
    j.append_derived_at("ink_snapshot", json!([]), json!([{ "id": a.event_id }]), lease(DAY), json!({}), T0).unwrap();
    j.set_keep_all(true).unwrap();
    assert_eq!(j.collect_at(T0 + 100 * DAY).unwrap().events_collected, 0);
    assert!(!is_stub(&by_id(&j, &a.event_id)));
    j.set_keep_all(false).unwrap();
    assert_eq!(j.collect_at(T0 + 100 * DAY).unwrap().events_collected, 1);
}

#[test]
fn a_new_snapshot_replaces_the_old_one_and_each_waits_out_its_own_lease() {
    let (_d, j) = journal();
    let op = ink_at(&j, "2026-10", 1);
    let s1 = j.append_derived_at("ink_snapshot", json!([]), json!([{ "id": op.event_id }]), lease(30 * DAY), json!({ "items": ["with-secret"] }), T0).unwrap();
    // later: a re-snapshot without the redacted stroke replaces the old snapshot
    let s2 = j.append_derived_at("ink_snapshot", json!([{ "id": s1.event_id }]), json!([{ "id": s1.event_id }]), lease(30 * DAY), json!({ "items": [] }), T0 + 40 * DAY).unwrap();

    let r = j.collect_at(T0 + 41 * DAY).unwrap();
    assert_eq!(r.events_collected, 1, "the op's lease is over; the old snapshot's lease (from s2) is not");
    assert!(is_stub(&by_id(&j, &op.event_id)) && !is_stub(&by_id(&j, &s1.event_id)));
    assert_eq!(j.collect_at(T0 + 71 * DAY).unwrap().events_collected, 1);
    assert!(is_stub(&by_id(&j, &s1.event_id)) && !is_stub(&by_id(&j, &s2.event_id)), "the newest snapshot survives");
}

#[test]
fn only_the_blob_part_is_collected_when_the_ref_says_blob() {
    let (_d, j) = journal();
    let a = j.capture_text("not media").unwrap();
    j.append_derived_at("transcript", json!([{ "id": a.event_id }]), json!([{ "id": a.event_id, "part": "blob" }]), lease(DAY), json!({ "text": "t" }), T0).unwrap();
    let r = j.collect_at(T0 + 2 * DAY).unwrap();
    assert_eq!(r.events_collected, 0, "the event stays; only its bytes go");
    assert!(!is_stub(&by_id(&j, &a.event_id)));
}

#[tokio::test]
async fn derived_events_and_stubs_sync_and_a_stub_never_overwrites_a_full_event() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    let op = ink_at(&ja, "2026-10", 1);
    let (a, b) = (Node::spawn(ja).await.unwrap(), Node::spawn(jb).await.unwrap());
    b.sync_events_with(&a.addr()).await.unwrap(); // B holds the full op
    a.journal().append_derived_at("ink_snapshot", json!([]), json!([{ "id": op.event_id }]), lease(DAY), json!({ "items": [] }), T0).unwrap();
    a.journal().collect_at(T0 + 2 * DAY).unwrap();
    assert!(is_stub(&by_id(a.journal(), &op.event_id)));

    b.sync_events_with(&a.addr()).await.unwrap();
    assert!(!is_stub(&by_id(b.journal(), &op.event_id)), "B keeps its full copy: a stub never replaces it");
    let derived_on_b = b.journal().store_events_for_test().into_iter().filter(|e| e.kind == EventKind::Derived).count();
    assert_eq!(derived_on_b, 1, "the derived event reached B");
    // B can now collect by itself, since it holds the derived event and the lease is over
    assert_eq!(b.journal().collect_at(T0 + 2 * DAY).unwrap().events_collected, 1);
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn a_blob_ref_makes_the_bytes_evictable_once_the_lease_is_over() {
    use memorious_core::event::MediaKind;
    use memorious_core::retention::RetentionPolicy;
    let dir = tempdir().unwrap();
    let j = Journal::init(&dir.path().join("j"), "pw").unwrap();
    let node = Node::spawn(j).await.unwrap();
    let photo = node.capture_blob(MediaKind::Photo, vec![1, 2, 3]).await.unwrap();
    let hash = photo.blob_hash().unwrap().to_string();
    let j = node.journal();
    j.append_derived_at("transcript", json!([{ "id": photo.event_id }]), json!([{ "id": photo.event_id, "part": "blob" }]), lease(DAY), json!({ "text": "t" }), T0).unwrap();
    let keep_all = RetentionPolicy::KEEP_EVERYTHING;
    assert!(j.evictable_blob_hashes(&keep_all, T0 + DAY / 2).unwrap().is_empty(), "inside the lease");
    assert_eq!(j.evictable_blob_hashes(&keep_all, T0 + 2 * DAY).unwrap(), vec![hash.clone()], "past the lease, even under a keep-everything retention policy");
    j.set_keep_all(true).unwrap();
    assert!(j.evictable_blob_hashes(&keep_all, T0 + 2 * DAY).unwrap().is_empty(), "an archive peer keeps it");
    node.shutdown().await;
}
