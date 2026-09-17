//! `EventKind::Infra`: the open-world envelope for peer/security/ops
//! plumbing (see `event.rs` for why it's structured this way). This module
//! holds the generic append/query helpers; specific facts (peer joins,
//! master-password key rewraps, ...) build on top of them elsewhere
//! (`journal.rs`).

use anyhow::Result;
use serde_json::Value;

use crate::event::{Event, EventKind, Payload};
use crate::journal::Journal;

impl Journal {
    /// Append an infra fact. `target` is the id this fact is about (a device
    /// id, a capture event id, ...) — `None` for facts about the journal as
    /// a whole.
    pub fn append_infra(&self, infra_kind: &str, target: Option<&str>, data: Value) -> Result<Event> {
        self.store.append_local(
            self.device_id(),
            EventKind::Infra,
            Payload::Infra {
                infra_kind: infra_kind.into(),
                target: target.map(|s| s.into()),
                data,
            },
            false,
        )
    }

    /// Every event of one infra kind, oldest first.
    pub fn infra_events(&self, infra_kind: &str) -> Result<Vec<Event>> {
        let mut out: Vec<Event> = self
            .store
            .all_events()?
            .into_iter()
            .filter(|e| matches!(&e.payload, Payload::Infra { infra_kind: k, .. } if k == infra_kind))
            .collect();
        out.sort_by(|a, b| (a.recorded_at, &a.event_id).cmp(&(b.recorded_at, &b.event_id)));
        Ok(out)
    }

    /// Winning infra event for one (kind, target) pair: latest `recorded_at`,
    /// event id as tie-break — same rule as `annotations()`/`latest_token_set()`.
    pub fn latest_infra_for_target(&self, infra_kind: &str, target: &str) -> Result<Option<Event>> {
        Ok(self
            .infra_events(infra_kind)?
            .into_iter()
            .filter(|e| matches!(&e.payload, Payload::Infra { target: Some(t), .. } if t == target))
            .last())
    }
}
