//! `media_lost`: the log references a blob that no device can supply any more.
//!
//! An infra event (`infra_kind: "media_lost"`, `target`: the blob hash, `data:
//! {reason?}`), so it needs no wire change and an older peer simply stores and
//! forwards it. Nothing is deleted — the capture stays, its placeholder stays,
//! the event stays. What changes is accounting: a lost blob is not "missing"
//! (the health light stops waiting for bytes that cannot arrive) and fetchers
//! stop asking for it. A device that still holds the bytes keeps them, and
//! counts them as held.

use std::collections::HashSet;

use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::json;

use crate::event::{Event, Payload};
use crate::journal::Journal;

pub const MEDIA_LOST: &str = "media_lost";

/// One declared-lost blob, as the log records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LostMedia {
    pub hash: String,
    pub reason: Option<String>,
    pub event_id: String,
    pub recorded_at: i64,
    /// Capture events that reference the blob (their placeholders stay).
    pub captures: Vec<String>,
}

impl Journal {
    /// Declare `hash` lost for good. `Ok(None)` if it already is (idempotent);
    /// an error if the log never references it (a typo must not become a fact).
    pub fn mark_media_lost(&self, hash: &str, reason: Option<&str>) -> Result<Option<Event>> {
        if !self.store.referenced_blob_hashes()?.iter().any(|h| h == hash) {
            bail!("no capture references blob {hash}");
        }
        if self.lost_blob_hashes()?.contains(hash) {
            return Ok(None);
        }
        let data = match reason.map(str::trim).filter(|r| !r.is_empty()) {
            Some(r) => json!({ "reason": r }),
            None => json!({}),
        };
        self.append_infra(MEDIA_LOST, Some(hash), data).map(Some)
    }

    /// Hashes any device has declared lost.
    pub fn lost_blob_hashes(&self) -> Result<HashSet<String>> {
        Ok(self
            .store
            .infra_events_of(MEDIA_LOST)?
            .into_iter()
            .filter_map(|e| match e.payload {
                Payload::Infra { infra_kind, target: Some(t), .. } if infra_kind == MEDIA_LOST => Some(t),
                _ => None,
            })
            .collect())
    }

    /// Every declared-lost blob, oldest declaration first.
    pub fn lost_media(&self) -> Result<Vec<LostMedia>> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for e in self.infra_events(MEDIA_LOST)? {
            let Payload::Infra { target: Some(hash), data, .. } = &e.payload else { continue };
            if !seen.insert(hash.clone()) {
                continue;
            }
            let captures = self
                .store
                .capture_for_hash(hash)?
                .map(|(id, _)| vec![id])
                .unwrap_or_default();
            out.push(LostMedia {
                hash: hash.clone(),
                reason: data.get("reason").and_then(|r| r.as_str()).map(String::from),
                event_id: e.event_id.clone(),
                recorded_at: e.recorded_at,
                captures,
            });
        }
        Ok(out)
    }
}
