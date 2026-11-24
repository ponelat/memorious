//! Isolating a peer: take it out of the network without touching a single event.
//!
//! `isolate()` resets every *connection*, not the *data*:
//!
//! * **its address** — the iroh endpoint key (`endpoint_secret`) is deleted, so the next time the node starts it is a new
//!   endpoint nobody has ever heard of (a new pairing ticket);
//! * **its peers** — every `peer_*` record (last heard from, heads, addresses, device ids, required/optional, how it was
//!   discovered), the remembered pairing ticket and the last-sync bookmark are forgotten;
//! * **inbound and outbound sync** — while isolated, the node refuses every sync / blob connection and never dials
//!   anyone (`Node` checks [`Journal::is_isolated`]).
//!
//! The journal itself is untouched: same `device_id`, same `seq` counter, every event row exactly as it was, so the peer keeps
//! capturing as the sole member of its own network, and `end_isolation()` + a normal sync with any peer of the original
//! network exchanges *only what each side lacks* (events are identified by `event_id` and `(device_id, seq)`; nothing is
//! re-written, so a peer's copy of every old event stays byte-for-byte what it was).
//!
//! The running endpoint cannot change its key in place: callers restart the node after `isolate()` (the web server exits and
//! lets its supervisor start it again; apps re-open the journal).

use anyhow::Result;

use crate::journal::Journal;

const FLAG: &str = "isolated";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct IsolateReport {
    /// Peers forgotten.
    pub peers_removed: usize,
    /// True when an endpoint key existed and was deleted (the address changes at the next start).
    pub address_reset: bool,
    /// Already isolated before this call (the reset still re-runs; it is idempotent).
    pub was_isolated: bool,
}

impl Journal {
    /// Is this peer currently isolated (no sync in or out)?
    pub fn is_isolated(&self) -> Result<bool> {
        Ok(self.store.meta_get(FLAG)?.is_some())
    }

    /// Isolate this peer: set the flag, forget every peer, delete the address key. Events are not touched. Idempotent.
    /// The node must be restarted for the new address to take effect; until then it already refuses all sync.
    pub fn isolate(&self) -> Result<IsolateReport> {
        let was_isolated = self.is_isolated()?;
        // flag first: from this instant the running node refuses connections
        self.store.meta_set(FLAG, b"1")?;
        let mut peers = 0usize;
        for (key, _) in self.store.meta_scan("peer_")? {
            if key.starts_with("peer_last_ok:") {
                peers += 1;
            }
            self.store.meta_delete(&key)?;
        }
        // shells remember the last pairing ticket and a sync bookmark under these names
        for key in ["last_peer_ticket", "last_sync_heads"] {
            self.store.meta_delete(key)?;
        }
        let address_reset = self.store.meta_get("endpoint_secret")?.is_some();
        self.store.meta_delete("endpoint_secret")?;
        Ok(IsolateReport { peers_removed: peers, address_reset, was_isolated })
    }

    /// Allow connections again. Peers stay forgotten and the address stays new: pair with the network (a ticket from any of its
    /// peers) and sync to exchange what each side lacks.
    pub fn end_isolation(&self) -> Result<()> {
        self.store.meta_delete(FLAG)
    }
}

/// Hidden helpers for integration tests (the store is crate-private).
#[doc(hidden)]
impl Journal {
    pub fn store_events_for_test(&self) -> Vec<crate::event::Event> {
        self.store.all_events().unwrap()
    }
    pub fn store_insert_remote_for_test(&self, e: &crate::event::Event) {
        self.store.insert_remote(e).unwrap();
    }
    pub fn store_fill_for_test(&self, e: &crate::event::Event) -> bool {
        self.store.insert_remote_outcome(e).map(|o| o == crate::store::RemoteOutcome::Filled).unwrap_or(false)
    }
    pub fn store_heads_for_test(&self) -> std::collections::BTreeMap<String, u64> {
        self.store.heads().unwrap()
    }
    pub fn store_meta_for_test(&self, key: &str, value: &[u8]) {
        self.store.meta_set(key, value).unwrap()
    }
    pub fn store_meta_scan_for_test(&self, prefix: &str) -> Vec<String> {
        let mut keys: Vec<String> = self.store.meta_scan(prefix).unwrap().into_iter().map(|(k, _)| k).collect();
        if let Ok(Some(_)) = self.store.meta_get(prefix) {
            keys.push(prefix.to_string());
        }
        keys
    }
}

const WRITE_ONLY: &str = "write_only";

impl Journal {
    /// Is this a write-only peer? A full peer in code that does not pull events from others: it keeps and pushes its own, and
    /// receives nothing. Meant to be brought up briefly (capture, sync, throw away) or kept as a light capture surface.
    pub fn is_write_only(&self) -> Result<bool> {
        Ok(self.store.meta_get(WRITE_ONLY)?.is_some())
    }

    pub fn set_write_only(&self, on: bool) -> Result<()> {
        if on {
            self.store.meta_set(WRITE_ONLY, b"1")
        } else {
            self.store.meta_delete(WRITE_ONLY)
        }
    }
}
