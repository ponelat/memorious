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

/// The media types. One stored format each: JPEG photos, AAC/m4a audio,
/// H.264/AAC MP4 video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Photo,
    Audio,
    Video,
}

/// What an audio capture is for. Not a tag: it is a property of the recording
/// itself, fixed at capture, like photo-vs-audio. `Voice` is a dictated note —
/// the transcript is the record and the blob is disposable. `Music` is the
/// record itself — captured clean, never transcribed, never evicted.
/// The full policy lives in one place: `crates/core/src/retention.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioKind {
    #[default]
    Voice,
    Music,
}

impl AudioKind {
    pub fn is_voice(&self) -> bool {
        *self == AudioKind::Voice
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            AudioKind::Voice => "voice",
            AudioKind::Music => "music",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "voice" => Some(AudioKind::Voice),
            "music" => Some(AudioKind::Music),
            _ => None,
        }
    }
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
        /// What the recording *is* — decided once, at capture. Absent on the
        /// wire means `Voice`, so pre-2026-08-23 events and peers stay valid.
        /// Drives capture quality, enrichment and retention — see `retention.rs`.
        #[serde(default, skip_serializing_if = "AudioKind::is_voice")]
        audio_kind: AudioKind,
    },
    Video {
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
        Self::media_audio_kind(kind, hash, size, crypto, AudioKind::Voice)
    }

    /// `media` with an explicit audio kind (ignored for photo/video).
    pub fn media_audio_kind(
        kind: MediaKind,
        hash: String,
        size: u64,
        crypto: BlobCrypto,
        audio_kind: AudioKind,
    ) -> Self {
        let crypto = Some(crypto);
        match kind {
            MediaKind::Photo => Payload::Photo { hash, size, crypto },
            MediaKind::Audio => Payload::Audio { hash, size, crypto, audio_kind },
            MediaKind::Video => Payload::Video { hash, size, crypto },
        }
    }

    /// The audio kind of an audio payload; `None` for everything else.
    pub fn audio_kind(&self) -> Option<AudioKind> {
        match self {
            Payload::Audio { audio_kind, .. } => Some(*audio_kind),
            _ => None,
        }
    }

    /// The encryption envelope of a media payload, if any.
    pub fn blob_crypto(&self) -> Option<&BlobCrypto> {
        match self {
            Payload::Photo { crypto, .. }
            | Payload::Audio { crypto, .. }
            | Payload::Video { crypto, .. } => crypto.as_ref(),
            _ => None,
        }
    }
}

impl Event {
    /// Blob hash referenced by this event's payload, if any.
    pub fn blob_hash(&self) -> Option<&str> {
        match &self.payload {
            Payload::Photo { hash, .. }
            | Payload::Audio { hash, .. }
            | Payload::Video { hash, .. } => Some(hash),
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
