use serde::{Deserialize, Serialize};

/// The four event kinds. There will never be more without a fight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Capture,
    Redact,
    TokenSet,
    Annotation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Payload {
    Text {
        text: String,
    },
    Photo {
        hash: String,
        size: u64,
    },
    Audio {
        hash: String,
        size: u64,
    },
    Redact {
        target: String,
    },
    TokenSet {
        /// blake3 hex of the passcode; never the passcode itself.
        hash: String,
    },
    Annotation {
        target: String,
        text: String,
    },
}

/// Event envelope. Append-only: an Event is immutable once written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub event_id: String,
    pub device_id: String,
    pub seq: u64,
    /// Unix milliseconds.
    pub recorded_at: i64,
    pub kind: EventKind,
    pub payload: Payload,
    /// "I intend to enrich this" (captures only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub will_enrich: bool,
}

impl Event {
    /// Blob hash referenced by this event's payload, if any.
    pub fn blob_hash(&self) -> Option<&str> {
        match &self.payload {
            Payload::Photo { hash, .. } | Payload::Audio { hash, .. } => Some(hash),
            _ => None,
        }
    }

    /// Text that should be indexed for full-text search.
    pub fn fts_text(&self) -> Option<&str> {
        match &self.payload {
            Payload::Text { text } | Payload::Annotation { text, .. } => Some(text),
            _ => None,
        }
    }
}
