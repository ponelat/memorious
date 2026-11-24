//! `ink_fold`: one event holding a closed canvas period's surviving strokes.
//!
//! A canvas period (a UTC ISO week, or a legacy month) is the fold of its append-only ink ops. Once the period has ended any device
//! may write an `ink_fold` op: `{op:"ink_fold", period, items:[...], supersedes:[...]}` where `items` are the strokes (and images)
//! that survive, in draw order, each with tool, size, opacity, seed and its points. It holds **no eraser strokes** (erased points
//! are cut out of the strokes they wiped), **no undo/redo history** and **no per-item times** — whoever needs the full history asks a
//! peer that keeps revisions. It `supersedes` the period's ops up to a per-device `seq` bound (an op above the bound, drawn offline
//! and synced late, stays live and is folded into the *next* fold) and any earlier fold of the period. Superseding deletes nothing;
//! each device's retention policy decides whether it keeps or collects the superseded ops (`retention.rs` rule 3, `supersede.rs`).
//!
//! Fidelity: a stroke is cut where the erasers left under 20% of it (`canvas_fold::ERASED_BELOW`); a lightly faded part keeps its
//! full opacity. The renderer regenerates the dots from the points and the seed, so the fold stays compact (points, not dabs).

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::canvas_fold::{self, Item, StrokeItem, Tool};
use crate::event::{Event, Payload};
use crate::ink::{ink_data, is_period_key, period_end_ms};
use crate::journal::Journal;

fn is_stub(e: &Event) -> bool {
    matches!(&e.payload, Payload::Unknown { type_tag, .. } if type_tag == "collected")
}

fn is_fold(e: &Event) -> bool {
    matches!(&e.payload, Payload::Ink { op, .. } if op == "ink_fold")
}

/// The per-device `seq` bound an `ink_fold` declares in its `supersedes` selector.
fn bound_of(fold: &Event) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    if let Payload::Ink { fields, .. } = &fold.payload {
        for r in fields.get("supersedes").and_then(Value::as_array).into_iter().flatten() {
            if r.get("select").and_then(Value::as_str) == Some("ink") {
                for (dev, seq) in r.get("up_to").and_then(Value::as_object).into_iter().flatten() {
                    if let Some(s) = seq.as_u64() {
                        let e = out.entry(dev.clone()).or_insert(0);
                        *e = (*e).max(s);
                    }
                }
            }
        }
    }
    out
}

/// An item as the op JSON the fold library reads (a surviving stroke / image becomes an op again, ids kept).
fn item_as_op(it: &Item) -> Value {
    match it {
        Item::Stroke(s) => {
            let mut v = json!({ "op": "stroke", "cid": s.id, "tool": s.tool, "seed": s.seed, "size": s.size, "pts": s.pts });
            if let Some(o) = s.op {
                v["opacity"] = json!(o);
            }
            v
        }
        Item::Image(i) => json!({ "op": "image", "cid": i.id, "hash": i.hash, "rect": i.rect }),
    }
}

/// Bake the erasers into the strokes they wiped and drop everything that is history: the result is what the canvas shows.
fn bake(items: Vec<Item>) -> Vec<Item> {
    let mut out = Vec::new();
    for (k, it) in items.iter().enumerate() {
        match it {
            Item::Image(_) => out.push(it.clone()),
            Item::Stroke(s) if s.tool == Tool::Eraser => {} // subtractive history, baked into the strokes below
            Item::Stroke(s) => {
                let layer = canvas_fold::layer_of(it);
                let erasers = canvas_fold::erasers_after(&items, k, layer);
                let runs = canvas_fold::visible_runs(&s.pts, &erasers);
                let untouched = runs.len() == 1 && runs[0].len() == s.pts.len();
                for (n, run) in runs.into_iter().enumerate() {
                    let id = if untouched { s.id.clone() } else { format!("{}~r{n}", s.id) };
                    out.push(Item::Stroke(StrokeItem { id, tool: s.tool, seed: s.seed, size: s.size, op: s.op, only: None, pts: run }));
                }
            }
        }
    }
    out
}

impl Journal {
    /// Fold one canvas period into an `ink_fold` event, if there is anything new to fold: the ops (and any earlier fold) this device
    /// holds, with an earlier fold as the base so a device that has already collected the ops can still fold. Returns the new event,
    /// or `None` when the period is empty or nothing arrived since the latest fold.
    pub fn fold_ink_period(&self, period: &str) -> Result<Option<Event>> {
        if !is_period_key(period) {
            bail!("ink period must be YYYY-Www or YYYY-MM, got {period:?}");
        }
        let mut evs: Vec<Event> = self.store.ink_events(period, 0)?.into_iter().map(|(_, e)| e).filter(|e| !is_stub(e)).collect();
        evs.sort_by(|a, b| (a.recorded_at, &a.event_id).cmp(&(b.recorded_at, &b.event_id)));
        let prev = evs.iter().rev().find(|e| is_fold(e)).cloned();
        let prev_bound = prev.as_ref().map(bound_of).unwrap_or_default();

        // the ops this fold is made of: everything not already inside the previous fold
        let fresh: Vec<&Event> = evs.iter().filter(|e| !is_fold(e) && e.seq > prev_bound.get(&e.device_id).copied().unwrap_or(0)).collect();
        if fresh.is_empty() {
            return Ok(None);
        }
        let mut ops: Vec<Value> = Vec::new();
        if let Some(Event { payload: Payload::Ink { fields, .. }, .. }) = &prev {
            if let Ok(base) = serde_json::from_value::<Vec<Item>>(fields.get("items").cloned().unwrap_or(Value::Null)) {
                ops.extend(base.iter().map(item_as_op));
            }
        }
        ops.extend(fresh.iter().map(|e| ink_data(e)));
        let items = bake(canvas_fold::fold_wire_ops(&ops));

        let mut up_to = prev_bound;
        for e in &fresh {
            let s = up_to.entry(e.device_id.clone()).or_insert(0);
            *s = (*s).max(e.seq);
        }
        let mut supersedes = vec![json!({ "select": "ink", "period": period, "up_to": up_to })];
        // every earlier fold of the period is superseded too (even one from a device whose ops sit above the bound)
        supersedes.extend(evs.iter().filter(|e| is_fold(e)).map(|e| json!({ "id": e.event_id })));
        Ok(Some(self.append_ink(
            period,
            json!({ "op": "ink_fold", "cid": uuid::Uuid::now_v7().to_string(), "items": items, "supersedes": supersedes }),
        )?))
    }

    /// Fold every period that ended at least `grace_ms` ago and has ops not yet folded. (`grace_ms` lets late-syncing peers' ops
    /// arrive first; whatever still arrives later is folded into the next pass.) Safe to call often; safe on any device.
    pub fn fold_due_periods(&self, now_ms: i64, grace_ms: i64) -> Result<Vec<Event>> {
        let mut out = Vec::new();
        for period in self.store.ink_months()? {
            let Some(end) = period_end_ms(&period) else { continue };
            if now_ms >= end.saturating_add(grace_ms) {
                if let Some(e) = self.fold_ink_period(&period)? {
                    out.push(e);
                }
            }
        }
        Ok(out)
    }
}
