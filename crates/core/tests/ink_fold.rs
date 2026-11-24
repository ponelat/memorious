//! `ink_fold`: one event holding a closed canvas period's surviving strokes (tool, size, opacity, seed, points; no eraser marks,
//! no undo/redo, no per-item times), superseding the ops it folded. Any device may build it once the period has ended.

use memorious_core::retention::RetentionPolicy;
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

fn stroke(cid: &str, tool: &str, x: f64, y: f64) -> Value {
    json!({ "op": "stroke", "cid": cid, "tool": tool, "seed": 7, "size": 5, "pts": [[x, y, 0.5, 0], [x + 40.0, y, 0.5, 0], [x + 80.0, y, 0.5, 0]] })
}

fn items_of(e: &Event) -> Vec<Value> {
    serde_json::to_value(e).unwrap()["payload"]["items"].as_array().unwrap().clone()
}

fn fold_ops(j: &Journal, period: &str) -> Vec<Event> {
    j.store_events_for_test()
        .into_iter()
        .filter(|e| matches!(&e.payload, Payload::Ink { period: p, op, .. } if p == period && op == "ink_fold"))
        .collect()
}

#[test]
fn a_fold_keeps_surviving_strokes_and_bakes_in_erasers_and_undo() {
    let dir = tempdir().unwrap();
    let j = Journal::init(&dir.path().join("j"), "pw").unwrap();
    let p = "2026-W10";
    j.append_ink(p, stroke("a", "pencil", 0.0, 0.0)).unwrap();
    j.append_ink(p, stroke("b", "pen", 0.0, 100.0)).unwrap();
    // an eraser pass straight over b wipes it out
    j.append_ink(p, json!({ "op": "stroke", "cid": "e", "tool": "eraser", "seed": 1, "size": 10, "opacity": 1.0, "pts": [[-20, 100, 0.5, 0], [120, 100, 0.5, 0]] })).unwrap();
    j.append_ink(p, stroke("c", "pen", 0.0, 200.0)).unwrap();
    j.append_ink(p, json!({ "op": "undo", "target": "c", "cid": "u1" })).unwrap();
    let last = j.append_ink(p, stroke("d", "pen", 0.0, 300.0)).unwrap();

    let fold = j.fold_ink_period(p).unwrap().expect("a fold was written");
    let v = serde_json::to_value(&fold).unwrap();
    assert_eq!(v["kind"], "ink");
    assert_eq!(v["payload"]["op"], "ink_fold");
    assert_eq!(v["payload"]["period"], p);
    let items = items_of(&fold);
    let ids: Vec<&str> = items.iter().map(|i| i["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["a", "d"], "b was erased, c undone: neither survives, and no eraser or undo is recorded");
    assert!(items.iter().all(|i| i["tool"] != "eraser"));
    assert!(items.iter().all(|i| i.get("t").is_none()), "no per-item times in the fold");
    assert_eq!(items[0]["tool"], "pencil");
    assert_eq!(items[0]["pts"].as_array().unwrap().len(), 3);
    // it supersedes exactly the ops it folded (everything up to the last op's seq on this device)
    assert_eq!(v["payload"]["supersedes"], json!([{ "select": "ink", "period": p, "up_to": { j.device_id(): last.seq } }]));
    assert_eq!(fold.seq, last.seq + 1);
}

#[test]
fn nothing_to_fold_means_no_event_and_a_second_fold_needs_new_ops() {
    let (_d, j) = journal();
    assert!(j.fold_ink_period("2026-W11").unwrap().is_none(), "an empty period");
    j.append_ink("2026-W11", stroke("a", "pen", 0.0, 0.0)).unwrap();
    assert!(j.fold_ink_period("2026-W11").unwrap().is_some());
    assert!(j.fold_ink_period("2026-W11").unwrap().is_none(), "already folded: nothing new since");
}

#[test]
fn a_late_op_is_folded_on_top_of_the_earlier_fold_which_it_then_supersedes() {
    let (_d, j) = journal();
    let p = "2026-W12";
    j.append_ink(p, stroke("a", "pen", 0.0, 0.0)).unwrap();
    let f1 = j.fold_ink_period(p).unwrap().unwrap();
    j.append_ink(p, stroke("late", "pen", 0.0, 50.0)).unwrap(); // arrives after the fold
    let f2 = j.fold_ink_period(p).unwrap().unwrap();
    let ids: Vec<String> = items_of(&f2).iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids, vec!["a", "late"]);
    let v = serde_json::to_value(&f2).unwrap();
    assert!(v["payload"]["supersedes"].as_array().unwrap().iter().any(|r| r["id"] == f1.event_id.as_str()), "the old fold is superseded explicitly");
    assert_eq!(fold_ops(&j, p).len(), 2);
}

#[test]
fn a_device_that_already_collected_the_ops_still_folds_from_the_earlier_fold() {
    let (_d, j) = journal();
    j.set_retention_policy(&RetentionPolicy { collect_without_backup: true, ..RetentionPolicy::PHONE }).unwrap();
    let p = "2026-W13";
    j.append_ink(p, stroke("a", "pen", 0.0, 0.0)).unwrap();
    j.append_ink(p, stroke("b", "pen", 0.0, 60.0)).unwrap();
    j.fold_ink_period(p).unwrap().unwrap();
    assert_eq!(j.collect_superseded_at(now_ms() + 31 * DAY).unwrap().events_collected, 2);
    // the ops are gone (stubs); a late op arrives and the next fold is built from the fold + the late op
    j.append_ink(p, stroke("late", "pen", 0.0, 120.0)).unwrap();
    let f = j.fold_ink_period(p).unwrap().unwrap();
    let ids: Vec<String> = items_of(&f).iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids, vec!["a", "b", "late"]);
    // and /api/ink-style listing no longer carries the stubs
    let listed = j.ink_list_json(p, 0).unwrap();
    assert!(listed["events"].as_array().unwrap().iter().all(|e| e["data"]["op"].is_string()));
}

#[test]
fn due_periods_are_folded_once_they_have_ended_plus_a_grace() {
    use memorious_core::ink::{period_end_ms, week_key};
    let (_d, j) = journal();
    j.append_ink("2026-W20", stroke("old", "pen", 0.0, 0.0)).unwrap();
    let this_week = week_key(now_ms());
    j.append_ink(&this_week, stroke("now", "pen", 0.0, 0.0)).unwrap();
    let end = period_end_ms("2026-W20").unwrap();
    let folded = j.fold_due_periods(end + 5 * 60_000, 15 * 60_000).unwrap();
    assert!(folded.is_empty(), "inside the grace period after the week ended");
    let folded = j.fold_due_periods(end + 20 * 60_000, 15 * 60_000).unwrap();
    assert_eq!(folded.len(), 1);
    assert!(j.fold_due_periods(now_ms(), 15 * 60_000).unwrap().is_empty(), "the current week is not folded, the old one already is");
    assert!(fold_ops(&j, &this_week).is_empty());
}

#[test]
fn a_fold_is_far_smaller_than_the_ops_it_replaces_when_most_ink_was_erased_or_undone() {
    let (_d, j) = journal();
    let p = "2026-W14";
    for i in 0..30 {
        j.append_ink(p, stroke(&format!("s{i}"), "pen", 0.0, i as f64 * 40.0)).unwrap();
        j.append_ink(p, json!({ "op": "undo", "target": format!("s{i}"), "cid": format!("u{i}") })).unwrap();
    }
    j.append_ink(p, stroke("keep", "pencil", 0.0, 5000.0)).unwrap();
    let f = j.fold_ink_period(p).unwrap().unwrap();
    assert_eq!(items_of(&f).len(), 1);
}
