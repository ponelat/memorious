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

/// The two media types. One stored format each: JPEG photos, AAC/m4a audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Photo,
    Audio,
}

/// Per-blob encryption envelope carried inside a media capture payload: the
/// wrapped content key and STREAM nonce base. The event log *is* the manifest
/// — an unlocked log is possession of every media key (UNDERSTANDING.md
/// §"Encryption at rest").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobCrypto {
    /// base64: 24-byte wrap nonce ‖ AEAD-wrapped 32-byte content key.
    pub ck: String,
    /// base64: 19-byte chunk-nonce base.
    pub nonce: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Payload {
    Text {
        text: String,
    },
    Photo {
        /// blake3 of the *ciphertext* — the iroh-blobs identity.
        hash: String,
        /// Plaintext length in bytes.
        size: u64,
        /// `None` only in pre-encryption journals awaiting `migrate-encrypt`
        /// (and in tests); every capture path writes `Some`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        crypto: Option<BlobCrypto>,
    },
    Audio {
        hash: String,
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        crypto: Option<BlobCrypto>,
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

impl Payload {
    pub fn media(kind: MediaKind, hash: String, size: u64, crypto: BlobCrypto) -> Self {
        let crypto = Some(crypto);
        match kind {
            MediaKind::Photo => Payload::Photo { hash, size, crypto },
            MediaKind::Audio => Payload::Audio { hash, size, crypto },
        }
    }

    /// The encryption envelope of a media payload, if any.
    pub fn blob_crypto(&self) -> Option<&BlobCrypto> {
        match self {
            Payload::Photo { crypto, .. } | Payload::Audio { crypto, .. } => crypto.as_ref(),
            _ => None,
        }
    }
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
