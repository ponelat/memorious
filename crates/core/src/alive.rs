//! "Alive" accounting for the Peers page: which UTC 5-minute slots each device was running in, and which slots our own
//! contact with a peer worked or failed.
//!
//! A device records "I was alive" per slot (`touch_alive`, called on launch and every minute while running) and publishes
//! its last 7 days of slots in the sync handshake, so the first contact after an absence also fills in what the peer
//! missed of our history — and we of theirs. We separately record, per peer, the slots where a handshake worked
//! (`link_ok`) or was attempted and failed (`link_bad`). Peer alive ∧ we alive ∧ link failed ⇒ "up but unreachable".
//! All of it is local, derived bookkeeping in `meta`; none of it is an event.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::journal::Journal;

pub const SLOT_MS: i64 = 5 * 60 * 1000;
/// 7 days of 5-minute slots.
pub const WINDOW: i64 = 2016;

pub fn slot_of(ms: i64) -> i64 {
    ms.div_euclid(SLOT_MS)
}

/// A set of slot numbers as sorted, non-adjacent runs `[start, len]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slots {
    pub runs: Vec<[i64; 2]>,
}

impl Slots {
    pub fn add(&mut self, slot: i64) {
        self.runs.push([slot, 1]);
        self.normalize();
    }

    pub fn merge(&mut self, other: &Slots) {
        self.runs.extend(other.runs.iter().copied());
        self.normalize();
    }

    pub fn contains(&self, slot: i64) -> bool {
        self.runs.iter().any(|[s, l]| slot >= *s && slot < s + l)
    }

    /// Drop everything older than the 7-day window ending at `now_slot`.
    pub fn prune(&mut self, now_slot: i64) {
        let floor = now_slot - WINDOW + 1;
        self.runs = self
            .runs
            .iter()
            .filter_map(|[s, l]| {
                let end = s + l;
                if end <= floor {
                    None
                } else if *s < floor {
                    Some([floor, end - floor])
                } else {
                    Some([*s, *l])
                }
            })
            .collect();
    }

    fn normalize(&mut self) {
        self.runs.sort();
        let mut out: Vec<[i64; 2]> = Vec::with_capacity(self.runs.len());
        for [s, l] in self.runs.drain(..) {
            match out.last_mut() {
                Some(last) if s <= last[0] + last[1] => last[1] = last[1].max(s + l - last[0]),
                _ => out.push([s, l]),
            }
        }
        self.runs = out;
    }
}

impl Journal {
    fn load_slots(&self, key: &str) -> Result<Slots> {
        Ok(match self.store.meta_get(key)? {
            Some(b) => serde_json::from_slice(&b).unwrap_or_default(),
            None => Slots::default(),
        })
    }

    fn save_slots(&self, key: &str, slots: &Slots) -> Result<()> {
        self.store.meta_set(key, &serde_json::to_vec(slots)?)
    }

    /// Mark this device alive in the slot containing `now_ms`.
    pub fn touch_alive(&self, now_ms: i64) -> Result<()> {
        let mut s = self.load_slots("alive_self")?;
        let slot = slot_of(now_ms);
        if s.contains(slot) {
            return Ok(());
        }
        s.add(slot);
        s.prune(slot);
        self.save_slots("alive_self", &s)
    }

    /// This device's own alive slots (what it publishes).
    pub fn alive_self(&self) -> Result<Slots> {
        self.load_slots("alive_self")
    }

    /// Fold a peer's published slots into what we remember of it.
    pub fn record_peer_alive(&self, endpoint: &str, theirs: &Slots, now_ms: i64) -> Result<()> {
        let key = format!("peer_alive:{endpoint}");
        let mut s = self.load_slots(&key)?;
        s.merge(theirs);
        s.prune(slot_of(now_ms));
        self.save_slots(&key, &s)
    }

    pub fn peer_alive(&self, endpoint: &str) -> Result<Slots> {
        self.load_slots(&format!("peer_alive:{endpoint}"))
    }

    /// Record that our contact with `endpoint` worked (`ok`) or was attempted and failed, in `now_ms`'s slot.
    pub fn note_link(&self, endpoint: &str, ok: bool, now_ms: i64) -> Result<()> {
        let key = format!("{}:{endpoint}", if ok { "link_ok" } else { "link_bad" });
        let mut s = self.load_slots(&key)?;
        let slot = slot_of(now_ms);
        if s.contains(slot) {
            return Ok(());
        }
        s.add(slot);
        s.prune(slot);
        self.save_slots(&key, &s)
    }

    /// (worked, failed) slots for our contact with `endpoint`.
    pub fn link_slots(&self, endpoint: &str) -> Result<(Slots, Slots)> {
        Ok((self.load_slots(&format!("link_ok:{endpoint}"))?, self.load_slots(&format!("link_bad:{endpoint}"))?))
    }
}
