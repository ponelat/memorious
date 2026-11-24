//! `derived` events: compaction and garbage collection as first-class citizens of the log.
//!
//! A `derived` event carries derived content (a transcript, an ink snapshot, ...) and says
//!
//! * `from`: which events it was made from (provenance, optional);
//! * `replaces`: which earlier events — or just the media bytes of one — it makes outdated (optional; absent or empty = it
//!   replaces nothing, like a summary);
//! * `lease_ms`: how long what it replaces is kept after this event (default 30 days).
//!
//! The log itself never shrinks silently. **Superseded** just means "a derived event held here replaces this". An event becomes
//! **collectable** once the lease on the *winning* superseder (the latest by `(recorded_at, event_id)`, the same total order the
//! canvas uses) has run out, and this device only *acts* on that if it keeps the effective set (the default; an archive peer
//! sets `keep_all`). Collecting an event leaves a **stub**: the same `event_id`, `device_id`, `seq`, `recorded_at` and `kind`
//! (sync needs contiguous per-device `seq`), with the payload replaced by `{type:"collected", by, was}`. A peer that still has
//! the full event keeps it: events are keyed by `event_id` and a stub never overwrites stored data.
//!
//! Refs (`from` / `replaces` items, kept as raw JSON so newer shapes survive):
//!
//! ```text
//! { "id": "<event_id>" }                          the whole event
//! { "id": "<event_id>", "part": "blob" }          just its media bytes (the event stays)
//! { "select": "ink", "month": "2026-10",
//!   "up_to": { "<device_id>": <max seq>, ... } }  a month's canvas ops by per-device seq bound; an op above the bound
//!                                                 (drawn offline, synced late) is NOT replaced
//! ```

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{json, Map, Value};

use crate::event::{Event, EventKind, Payload};
use crate::journal::Journal;

/// Lease when a `derived` event replaces something but doesn't say how long to wait.
pub const DEFAULT_LEASE_MS: u64 = 30 * 24 * 3600 * 1000;
const KEEP_ALL: &str = "gc_keep_all";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Whole,
    Blob,
}

/// One "derived event D makes event E (or its blob) outdated" fact.
#[derive(Debug, Clone)]
struct Supersession {
    event_id: String,
    part: Part,
    by: String,
    by_key: (i64, String),
    collect_after: i64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CollectReport {
    /// Events turned into stubs.
    pub events_collected: usize,
    /// Media blobs whose bytes may now be dropped (`part: "blob"` refs past their lease). The caller owns the blob store.
    pub blob_hashes: Vec<String>,
}

impl Journal {
    /// Append a `derived` event now. `from` / `replaces` are JSON arrays of refs (see the module docs), `fields` the content.
    pub fn append_derived(&self, what: &str, from: Value, replaces: Value, lease_ms: Option<u64>, fields: Value) -> Result<Event> {
        self.append_derived_at(what, from, replaces, lease_ms, fields, crate::store::now_ms())
    }

    /// As [`Self::append_derived`], with an explicit `recorded_at` (tests, imports).
    pub fn append_derived_at(&self, what: &str, from: Value, replaces: Value, lease_ms: Option<u64>, fields: Value, recorded_at: i64) -> Result<Event> {
        let list = |v: Value| match v {
            Value::Array(a) => Some(a),
            _ => None,
        };
        let fields = match fields {
            Value::Object(m) => m,
            Value::Null => Map::new(),
            _ => anyhow::bail!("derived content must be a JSON object"),
        };
        self.store.append_local_at(
            self.device_id(),
            EventKind::Derived,
            Payload::Derived { what: what.into(), from: list(from), replaces: list(replaces), lease_ms, fields },
            false,
            recorded_at,
        )
    }

    /// Does this device keep every event (an archive peer), instead of collecting what is superseded and past its lease?
    pub fn keep_all(&self) -> Result<bool> {
        Ok(self.store.meta_get(KEEP_ALL)?.is_some())
    }

    pub fn set_keep_all(&self, on: bool) -> Result<()> {
        if on {
            self.store.meta_set(KEEP_ALL, b"1")
        } else {
            self.store.meta_delete(KEEP_ALL)
        }
    }

    /// Every supersession currently in force, one per replaced event/part: the winning (latest) superseder.
    fn supersessions(&self) -> Result<Vec<Supersession>> {
        let mut best: HashMap<(String, bool), Supersession> = HashMap::new();
        for d in self.store.events_of_kind("derived")? {
            let Payload::Derived { replaces: Some(replaces), lease_ms, .. } = &d.payload else { continue };
            let collect_after = d.recorded_at.saturating_add(lease_ms.unwrap_or(DEFAULT_LEASE_MS).min(i64::MAX as u64) as i64);
            let key = (d.recorded_at, d.event_id.clone());
            let mut consider = |event_id: String, part: Part| {
                if event_id == d.event_id {
                    return; // a derived event never replaces itself
                }
                let k = (event_id.clone(), part == Part::Blob);
                let s = Supersession { event_id, part, by: d.event_id.clone(), by_key: key.clone(), collect_after };
                match best.get(&k) {
                    Some(old) if old.by_key >= s.by_key => {}
                    _ => {
                        best.insert(k, s);
                    }
                }
            };
            for r in replaces {
                if let Some(id) = r.get("id").and_then(Value::as_str) {
                    let part = if r.get("part").and_then(Value::as_str) == Some("blob") { Part::Blob } else { Part::Whole };
                    consider(id.to_string(), part);
                } else if r.get("select").and_then(Value::as_str) == Some("ink") {
                    let (Some(month), Some(up_to)) = (r.get("month").and_then(Value::as_str), r.get("up_to").and_then(Value::as_object)) else { continue };
                    for (_, e) in self.store.ink_events(month, 0)? {
                        if let Some(bound) = up_to.get(&e.device_id).and_then(Value::as_u64) {
                            if e.seq <= bound {
                                consider(e.event_id, Part::Whole);
                            }
                        }
                    }
                }
            }
        }
        let mut out: Vec<_> = best.into_values().collect();
        out.sort_by(|a, b| a.event_id.cmp(&b.event_id));
        Ok(out)
    }

    /// Collect what is superseded and past its lease as of `now_ms`: turn replaced events into stubs and report media blobs whose
    /// bytes may be dropped. Does nothing on a `keep_all` peer. Idempotent.
    pub fn collect_at(&self, now_ms: i64) -> Result<CollectReport> {
        let mut report = CollectReport::default();
        if self.keep_all()? {
            return Ok(report);
        }
        for s in self.supersessions()? {
            if s.collect_after > now_ms {
                continue;
            }
            let Some(e) = self.store.get_event(&s.event_id)? else { continue };
            match s.part {
                Part::Blob => {
                    if let Some(h) = e.blob_hash() {
                        report.blob_hashes.push(h.to_string());
                    }
                }
                Part::Whole => {
                    if matches!(&e.payload, Payload::Unknown { type_tag, .. } if type_tag == "collected") || e.kind == EventKind::Derived && false {
                        continue; // already a stub
                    }
                    let was = e.payload_json().get("type").and_then(Value::as_str).unwrap_or("").to_string();
                    self.store.stub_event(&e.event_id, &json!({ "type": "collected", "by": s.by, "was": was }))?;
                    report.events_collected += 1;
                }
            }
        }
        Ok(report)
    }

    /// Media blobs whose bytes a `derived` event has made disposable (`part: "blob"`) and whose lease is over. Unions into
    /// `evictable_blob_hashes`, so the blob GC drops them. Empty on a `keep_all` peer.
    pub fn derived_collectable_blob_hashes(&self, now_ms: i64) -> Result<Vec<String>> {
        if self.keep_all()? {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for s in self.supersessions()? {
            if s.part == Part::Blob && s.collect_after <= now_ms {
                if let Some(h) = self.store.get_event(&s.event_id)?.and_then(|e| e.blob_hash().map(str::to_string)) {
                    out.push(h);
                }
            }
        }
        Ok(out)
    }

    /// `collect_at(now)`.
    pub fn collect(&self) -> Result<CollectReport> {
        self.collect_at(crate::store::now_ms())
    }
}
