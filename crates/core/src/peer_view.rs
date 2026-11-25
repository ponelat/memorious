//! Read-side helpers for the Peers page: the newest user-visible event (here, or as far as a peer's heads say it holds),
//! and the 7-day history the page draws.

use anyhow::Result;
use serde::Serialize;
use serde_json::json;

use crate::alive::{slot_of, WINDOW};
use crate::event::{AudioKind, Event, EventKind, Payload};
use crate::journal::Journal;
use crate::store::Heads;

/// A user-visible event, summarized for one line of UI.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NewestEvent {
    pub event_id: String,
    /// `text` | `photo` | `voice` | `music` | `video` | `ink`.
    pub kind: String,
    pub title: String,
    pub recorded_at: i64,
}

fn summarize(e: &Event) -> Option<NewestEvent> {
    let (kind, title) = match (&e.kind, &e.payload) {
        (_, Payload::Text { text }) => ("text", text.lines().next().unwrap_or("").chars().take(60).collect::<String>()),
        (_, Payload::Photo { .. }) => ("photo", "Photo".to_string()),
        (_, Payload::Audio { audio_kind: AudioKind::Voice, .. }) => ("voice", "Voice note".to_string()),
        (_, Payload::Audio { .. }) => ("music", "Audio".to_string()),
        (_, Payload::Video { .. }) => ("video", "Video".to_string()),
        (EventKind::Ink, _) => ("ink", "Canvas stroke".to_string()),
        _ => return None,
    };
    Some(NewestEvent { event_id: e.event_id.clone(), kind: kind.into(), title, recorded_at: e.recorded_at })
}

impl Journal {
    /// The newest capture or ink event; with `heads`, only among events a device holding exactly those heads would have.
    pub fn newest_event(&self, heads: Option<&Heads>) -> Result<Option<NewestEvent>> {
        let mut best: Option<Event> = None;
        for e in self.store.newest_visible_events(heads)? {
            if summarize(&e).is_some() && best.as_ref().is_none_or(|b| (e.recorded_at, &e.event_id) > (b.recorded_at, &b.event_id)) {
                best = Some(e);
            }
        }
        Ok(best.as_ref().and_then(summarize))
    }

    /// The window the Peers page draws for one peer: our alive slots, the peer's (published ∪ every slot we saw it answer),
    /// and the slots where our contact with it failed. Slot numbers are absolute (5 minutes since the epoch).
    pub fn peer_history(&self, endpoint: &str, now_ms: i64) -> Result<serde_json::Value> {
        let (ok, bad) = self.link_slots(endpoint)?;
        let mut theirs = self.peer_alive(endpoint)?;
        theirs.merge(&ok);
        let now = slot_of(now_ms);
        Ok(json!({
            "endpoint_id": endpoint,
            "from": now - WINDOW + 1,
            "to": now,
            "mine": self.alive_self()?.runs,
            "theirs": theirs.runs,
            "linked": ok.runs,
            "failed": bad.runs,
        }))
    }
}
