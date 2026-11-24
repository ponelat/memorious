//! Superseding, collection, and the gap bookkeeping — the *facts* side. The *decision* (does this device keep or collect what is
//! superseded, and when) is `retention.rs` rule 3; this module reads the log and applies it.
//!
//! An event may **supersede** earlier ones: it lists them in a `supersedes` property (today: an `ink_fold` op). That is a statement
//! about the data, never an instruction to delete. Each device's own retention policy decides whether it keeps the superseded events
//! (a backup: `superseded: Forever`) or **collects** them (an active peer: after a window counted from when it received the
//! superseding event). Collecting **deletes the rows** — the log is not padded with stubs — and records, in this device's own
//! `collected` table (local, never synced), the `seq` ranges the events occupied, which fold superseded them, what they were and the
//! time window they cover. A device's heads still count them, so it is not "behind"; its handshake declares these ranges as **gaps**
//! so a peer accepts the jump in sequence numbers (and records the gap, to fill later). A device that keeps revisions asks peers to
//! fill the ranges it lacks (`stubs` handshake field); the full events go back into the gap (`Store::insert_remote_outcome_with`).
//!
//! Only canvas events can ever be collected (ink ops and folds): whatever an event claims to supersede, captures, annotations,
//! redacts, key material and the like are never touched.
//!
//! Refs in `supersedes`:
//!
//! ```text
//! { "id": "<event_id>" }                          one event
//! { "select": "ink", "period": "2026-W44",
//!   "up_to": { "<device_id>": <max seq>, ... } }  a month's canvas ops by per-device seq bound; an op above the bound
//!                                                 (drawn offline, synced late) is NOT superseded
//! ```

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use serde_json::{json, Value};

use crate::event::{Event, EventKind, Payload};
use crate::journal::Journal;
use crate::retention::{superseded_disposition, Disposition};
use crate::store::{StubCounts, StubRange};

/// Most events served to a peer filling its gaps in one sync round (a big fill finishes over a few rounds).
pub const FILL_CAP: usize = 5000;
/// Most ranges a device advertises in one handshake.
const MAX_RANGES: usize = 2000;

#[derive(Debug, Clone)]
struct Supersession {
    event_id: String,
    by: String,
    by_key: (i64, String),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CollectReport {
    /// Events collected (rows deleted, ranges recorded).
    pub events_collected: usize,
}

/// The `supersedes` list an event declares, wherever it sits: an ink payload field, an unknown payload property or an unknown
/// envelope property.
fn supersedes_of(e: &Event) -> Vec<Value> {
    let from = |v: Option<&Value>| v.and_then(Value::as_array).cloned();
    let list = match &e.payload {
        Payload::Ink { fields, .. } => from(fields.get("supersedes")),
        _ => from(e.payload_extra.get("supersedes")),
    };
    list.or_else(|| from(e.extra.get("supersedes"))).unwrap_or_default()
}

/// Only canvas events may ever be collected.
fn is_collectable_kind(e: &Event) -> bool {
    e.kind == EventKind::Ink || matches!(&e.payload, Payload::Infra { infra_kind, .. } if infra_kind == "ink")
}

impl Journal {
    /// Every supersession in force: for each superseded event, its winning superseder (the latest by `(recorded_at, event_id)`).
    fn supersessions(&self) -> Result<Vec<Supersession>> {
        let mut best: HashMap<String, Supersession> = HashMap::new();
        for d in self.store.events_declaring_supersedes()? {
            let key = (d.recorded_at, d.event_id.clone());
            let mut consider = |event_id: String| {
                if event_id == d.event_id {
                    return; // never itself
                }
                let s = Supersession { event_id: event_id.clone(), by: d.event_id.clone(), by_key: key.clone() };
                match best.get(&event_id) {
                    Some(old) if old.by_key >= s.by_key => {}
                    _ => {
                        best.insert(event_id, s);
                    }
                }
            };
            for r in supersedes_of(&d) {
                if let Some(id) = r.get("id").and_then(Value::as_str) {
                    consider(id.to_string());
                } else if r.get("select").and_then(Value::as_str) == Some("ink") {
                    let (Some(month), Some(up_to)) = (r.get("period").or_else(|| r.get("month")).and_then(Value::as_str), r.get("up_to").and_then(Value::as_object)) else { continue };
                    for (_, e) in self.store.ink_events(month, 0)? {
                        if up_to.get(&e.device_id).and_then(Value::as_u64).is_some_and(|bound| e.seq <= bound) {
                            consider(e.event_id);
                        }
                    }
                }
            }
        }
        let mut out: Vec<_> = best.into_values().collect();
        out.sort_by(|a, b| a.event_id.cmp(&b.event_id));
        Ok(out)
    }

    /// Collect what this device's policy lets go as of `now_ms`: superseded canvas events whose window (counted from when this
    /// device received the winning superseder) is over become stubs. Does nothing on a device that keeps revisions, and — unless
    /// the policy says otherwise — waits until some peer has announced it keeps revisions. Idempotent.
    pub fn collect_superseded_at(&self, now_ms: i64) -> Result<CollectReport> {
        let mut report = CollectReport::default();
        let policy = self.retention_policy()?;
        if policy.keeps_revisions() || (!policy.collect_without_backup && !self.has_backup_peer()?) {
            return Ok(report);
        }
        let mut to_collect: Vec<(Event, String)> = Vec::new();
        for s in self.supersessions()? {
            let local = match self.store.local_received_at(&s.by)? {
                Some(t) if t > 0 => t,
                _ => match self.store.get_event(&s.by)? {
                    Some(by) => by.recorded_at,
                    None => continue, // the superseder is not held: nothing to rely on
                },
            };
            if !matches!(superseded_disposition(local, &policy, now_ms), Disposition::Evictable(_)) {
                continue;
            }
            let Some(e) = self.store.get_event(&s.event_id)? else { continue };
            if !is_collectable_kind(&e) {
                continue;
            }
            to_collect.push((e, s.by.clone()));
        }
        report.events_collected = self.store.collect_events(&to_collect)?;
        Ok(report)
    }

    /// `collect_superseded_at(now)`.
    pub fn collect_superseded(&self) -> Result<CollectReport> {
        self.collect_superseded_at(crate::store::now_ms())
    }

    /// Stubs on this device, for the detail pages.
    pub fn stub_counts(&self) -> Result<StubCounts> {
        self.store.stub_counts()
    }

    /// The stub ranges this device wants filled, to put in a handshake: `Some` only when its policy keeps revisions (otherwise
    /// it would just collect them again) and it holds any stubs.
    pub fn wanted_stub_ranges(&self) -> Result<Option<Vec<StubRange>>> {
        if !self.retention_policy()?.keeps_revisions() {
            return Ok(None);
        }
        let ranges = self.store.stub_ranges(MAX_RANGES)?;
        Ok((!ranges.is_empty()).then_some(ranges))
    }

    /// The ranges this device collected, to declare as gaps in the handshake (always, whatever its policy: the peer must accept the
    /// jump in sequence numbers). `None` when there are none.
    pub fn declared_gaps(&self) -> Result<Option<Vec<StubRange>>> {
        let ranges = self.store.stub_ranges(MAX_RANGES)?;
        Ok((!ranges.is_empty()).then_some(ranges))
    }

    /// Count stubs a peer filled for us (shown per peer).
    pub fn record_stubs_filled(&self, peer: &str, n: usize) -> Result<()> {
        if n == 0 {
            return Ok(());
        }
        let key = format!("peer_stubs_filled:{peer}");
        let prev = self.store.meta_get(&key)?.and_then(|b| b.as_slice().try_into().ok().map(u64::from_le_bytes)).unwrap_or(0);
        self.store.meta_set(&key, &(prev + n as u64).to_le_bytes())
    }

    pub fn peer_stubs_filled(&self, peer: &str) -> u64 {
        self.store.meta_get(&format!("peer_stubs_filled:{peer}")).ok().flatten().and_then(|b| b.as_slice().try_into().ok().map(u64::from_le_bytes)).unwrap_or(0)
    }

    // ---- declaring whether this device keeps revisions ----

    /// Announce, with a `peer_policy` infra event (target = this device), whether this device keeps revisions. Idempotent: only
    /// writes when the answer differs from the last announcement. Called when the policy changes and on launch.
    pub fn ensure_peer_policy(&self) -> Result<()> {
        // a write-only peer holds nothing of other devices', so it is never a backup whatever its retention says
        let receives = !self.is_write_only()?;
        let keeps = receives && self.retention_policy()?.keeps_revisions();
        let last = self.peer_declarations()?.get(self.device_id()).copied();
        if last != Some((keeps, receives)) {
            self.append_infra("peer_policy", Some(self.device_id()), json!({ "keeps_revisions": keeps, "receives": receives }))?;
        }
        Ok(())
    }

    /// Latest `(keeps_revisions, receives)` announced per device. `receives` is additive: absent (older announcements) = true.
    fn peer_declarations(&self) -> Result<BTreeMap<String, (bool, bool)>> {
        let mut latest: BTreeMap<String, ((i64, String), (bool, bool))> = BTreeMap::new();
        for e in self.infra_events("peer_policy")? {
            let Payload::Infra { target: Some(dev), data, .. } = &e.payload else { continue };
            let Some(keeps) = data.get("keeps_revisions").and_then(Value::as_bool) else { continue };
            let receives = data.get("receives").and_then(Value::as_bool).unwrap_or(true);
            let k = (e.recorded_at, e.event_id.clone());
            if latest.get(dev).is_none_or(|(old, _)| *old < k) {
                latest.insert(dev.clone(), (k, (keeps, receives)));
            }
        }
        Ok(latest.into_iter().map(|(d, (_, v))| (d, v)).collect())
    }

    /// Latest announcement per device: does it keep revisions? (Latest by `(recorded_at, event_id)`.)
    pub fn peer_policies(&self) -> Result<BTreeMap<String, bool>> {
        Ok(self.peer_declarations()?.into_iter().map(|(d, (keeps, _))| (d, keeps)).collect())
    }

    /// Has some OTHER device announced that it keeps revisions?
    pub fn has_backup_peer(&self) -> Result<bool> {
        Ok(self.peer_policies()?.iter().any(|(d, keeps)| *keeps && d != self.device_id()))
    }
}
