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
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NewestEvent {
    pub event_id: String,
    /// `text` | `photo` | `voice` | `music` | `video` | `ink`.
    pub kind: String,
    pub title: String,
    pub recorded_at: i64,
    /// Drawing strokes: how many points, and a normalised (0..1) downsample to draw the squiggle with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub points: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape: Option<Vec<[f32; 2]>>,
}

fn snippet(text: &str) -> String {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > 70 {
        format!("{}…", t.chars().take(69).collect::<String>())
    } else {
        t
    }
}

/// Up to 24 points of a stroke's `pts` ([[x,y,..]]), scaled into 0..1 keeping the aspect ratio.
fn squiggle(pts: &[serde_json::Value]) -> Vec<[f32; 2]> {
    let xy: Vec<(f32, f32)> = pts
        .iter()
        .filter_map(|p| Some((p.get(0)?.as_f64()? as f32, p.get(1)?.as_f64()? as f32)))
        .collect();
    if xy.is_empty() {
        return Vec::new();
    }
    let step = (xy.len() as f32 / 24.0).max(1.0);
    let mut picked: Vec<(f32, f32)> = (0..xy.len()).step_by(1).filter(|i| (*i as f32 % step) < 1.0).map(|i| xy[i]).collect();
    if let Some(last) = xy.last() {
        if picked.last() != Some(last) {
            picked.push(*last);
        }
    }
    let (minx, maxx) = picked.iter().fold((f32::MAX, f32::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
    let (miny, maxy) = picked.iter().fold((f32::MAX, f32::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
    let span = (maxx - minx).max(maxy - miny).max(f32::EPSILON);
    picked.iter().map(|p| [(p.0 - minx) / span, (p.1 - miny) / span]).collect()
}

impl Journal {
    /// One line for an event: text quoted, media by its transcript/OCR when there is one, a stroke by its dots.
    fn summarize(&self, e: &Event) -> Option<NewestEvent> {
        let quoted = |fallback: &str| -> String {
            self.store.latest_annotation_text(&e.event_id).ok().flatten().map(|t| snippet(&t)).filter(|t| !t.is_empty()).unwrap_or_else(|| fallback.to_string())
        };
        let mut points = None;
        let mut shape = None;
        let (kind, title) = match (&e.kind, &e.payload) {
            (_, Payload::Text { text }) => ("text", snippet(text.lines().next().unwrap_or(""))),
            (_, Payload::Photo { .. }) => ("photo", quoted("Photo")),
            (_, Payload::Audio { audio_kind: AudioKind::Voice, .. }) => ("voice", quoted("Voice note")),
            (_, Payload::Audio { .. }) => ("music", quoted("Audio")),
            (_, Payload::Video { .. }) => ("video", quoted("Video")),
            (EventKind::Ink, Payload::Ink { op, fields, .. }) if op == "stroke" => {
                let pts = fields.get("pts").and_then(|p| p.as_array()).cloned().unwrap_or_default();
                points = Some(pts.len() as u32);
                shape = Some(squiggle(&pts));
                ("ink", "Drawing".to_string())
            }
            _ => return None,
        };
        Some(NewestEvent { event_id: e.event_id.clone(), kind: kind.into(), title, recorded_at: e.recorded_at, points, shape })
    }
}

impl Journal {
    /// The newest capture or ink event; with `heads`, only among events a device holding exactly those heads would have.
    pub fn newest_event(&self, heads: Option<&Heads>) -> Result<Option<NewestEvent>> {
        let mut best: Option<Event> = None;
        for e in self.store.newest_visible_events(heads)? {
            if self.summarize(&e).is_some() && best.as_ref().is_none_or(|b| (e.recorded_at, &e.event_id) > (b.recorded_at, &b.event_id)) {
                best = Some(e);
            }
        }
        Ok(best.as_ref().and_then(|e| self.summarize(e)))
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
