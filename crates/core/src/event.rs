use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The event kinds. `Infra` (2026-09-16) is a deliberate, one-time exception to
/// "no more without a fight": it's an open-world envelope for peer/security/ops
/// plumbing (see `Payload::Infra`), not a single-purpose kind, so it should
/// never need a sibling — a new fact is a new `infra_kind` string, not a new
/// `EventKind`.
///
/// `Unknown` is never written by any device — it's what a kind invented
/// *after* a peer's build decodes to on that peer, so an unrecognized kind
/// never breaks sync (round-trips losslessly; see `Payload::Unknown` and its
/// module-level note).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    Capture,
    Redact,
    TokenSet,
    Annotation,
    Infra,
    Unknown(String),
}

impl EventKind {
    fn as_str(&self) -> &str {
        match self {
            EventKind::Capture => "capture",
            EventKind::Redact => "redact",
            EventKind::TokenSet => "token_set",
            EventKind::Annotation => "annotation",
            EventKind::Infra => "infra",
            EventKind::Unknown(s) => s,
        }
    }
}

impl Serialize for EventKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EventKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "capture" => EventKind::Capture,
            "redact" => EventKind::Redact,
            "token_set" => EventKind::TokenSet,
            "annotation" => EventKind::Annotation,
            "infra" => EventKind::Infra,
            other => EventKind::Unknown(other.to_string()),
        })
    }
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

/// The wire/storage shape of every payload kind this build understands.
/// Never used directly outside this module — `Payload` wraps it with an
/// `Unknown` escape hatch (see module docs on `Payload`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PayloadKnown {
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
    /// Open-world envelope for peer/security/ops plumbing — a device join, a
    /// key rewrap after a master-password change, a future upgrade notice,
    /// etc. `infra_kind` picks the fact; `data` is its free-form shape.
    /// Adding a new `infra_kind` never needs a wire/schema change, so this
    /// kind should never need a sibling — see `EventKind::Infra`.
    Infra {
        infra_kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        data: serde_json::Value,
    },
}

/// An event payload. Structurally this is `PayloadKnown` plus one escape
/// hatch: `Unknown`, what a payload `type` this build has never heard of
/// decodes to.
///
/// This matters because `Payload` is internally tagged (`type` + flattened
/// fields, not `{type, data}`) — serde's built-in `#[serde(other)]` fallback
/// only supports a unit variant with no way to keep the rest of the object,
/// so a naive "unknown tag" case would either fail to deserialize (killing
/// the whole sync round that carried it — see `AGENTS.md` "Versioning rules
/// that bite") or silently drop data a peer further down the sync chain
/// might actually understand. `Unknown` instead keeps the *entire original
/// JSON object* verbatim in `data` and re-emits it unchanged on the wire, so
/// an event this build can't interpret still stores, syncs, and forwards
/// losslessly — every future `infra_kind` (and, if it ever comes to that,
/// every future `EventKind`) is safe for any peer already running this or a
/// later build, permanently, with no further coordinated rollout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    Text {
        text: String,
    },
    Photo {
        hash: String,
        size: u64,
        crypto: Option<BlobCrypto>,
    },
    Audio {
        hash: String,
        size: u64,
        crypto: Option<BlobCrypto>,
        audio_kind: AudioKind,
    },
    Video {
        hash: String,
        size: u64,
        crypto: Option<BlobCrypto>,
    },
    Redact {
        target: String,
    },
    TokenSet {
        hash: String,
    },
    Annotation {
        target: String,
        text: String,
    },
    Infra {
        infra_kind: String,
        target: Option<String>,
        data: serde_json::Value,
    },
    /// A payload `type` this build doesn't recognize. `data` is the full,
    /// original JSON object — see the type-level doc comment above.
    Unknown {
        type_tag: String,
        data: serde_json::Value,
    },
}

impl From<PayloadKnown> for Payload {
    fn from(known: PayloadKnown) -> Self {
        match known {
            PayloadKnown::Text { text } => Payload::Text { text },
            PayloadKnown::Photo { hash, size, crypto } => Payload::Photo { hash, size, crypto },
            PayloadKnown::Audio { hash, size, crypto, audio_kind } => {
                Payload::Audio { hash, size, crypto, audio_kind }
            }
            PayloadKnown::Video { hash, size, crypto } => Payload::Video { hash, size, crypto },
            PayloadKnown::Redact { target } => Payload::Redact { target },
            PayloadKnown::TokenSet { hash } => Payload::TokenSet { hash },
            PayloadKnown::Annotation { target, text } => Payload::Annotation { target, text },
            PayloadKnown::Infra { infra_kind, target, data } => {
                Payload::Infra { infra_kind, target, data }
            }
        }
    }
}

impl Payload {
    /// This payload's shape as `PayloadKnown`, for serialization — `None` for
    /// `Unknown`, which serializes its stored `data` directly instead.
    fn as_known(&self) -> Option<PayloadKnown> {
        Some(match self {
            Payload::Text { text } => PayloadKnown::Text { text: text.clone() },
            Payload::Photo { hash, size, crypto } => {
                PayloadKnown::Photo { hash: hash.clone(), size: *size, crypto: crypto.clone() }
            }
            Payload::Audio { hash, size, crypto, audio_kind } => PayloadKnown::Audio {
                hash: hash.clone(),
                size: *size,
                crypto: crypto.clone(),
                audio_kind: *audio_kind,
            },
            Payload::Video { hash, size, crypto } => {
                PayloadKnown::Video { hash: hash.clone(), size: *size, crypto: crypto.clone() }
            }
            Payload::Redact { target } => PayloadKnown::Redact { target: target.clone() },
            Payload::TokenSet { hash } => PayloadKnown::TokenSet { hash: hash.clone() },
            Payload::Annotation { target, text } => {
                PayloadKnown::Annotation { target: target.clone(), text: text.clone() }
            }
            Payload::Infra { infra_kind, target, data } => PayloadKnown::Infra {
                infra_kind: infra_kind.clone(),
                target: target.clone(),
                data: data.clone(),
            },
            Payload::Unknown { .. } => return None,
        })
    }
}

impl Serialize for Payload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.as_known() {
            Some(known) => known.serialize(serializer),
            // Re-emit the original object verbatim — see the type-level doc
            // comment on `Payload` for why this must not re-nest the fields.
            None => match self {
                Payload::Unknown { data, .. } => data.serialize(serializer),
                _ => unreachable!("as_known() returns None only for Unknown"),
            },
        }
    }
}

impl<'de> Deserialize<'de> for Payload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match serde_json::from_value::<PayloadKnown>(value.clone()) {
            Ok(known) => Ok(known.into()),
            Err(_) => {
                let type_tag = value
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                Ok(Payload::Unknown { type_tag, data: value })
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `kind`/payload `type` this build has never heard of must still
    /// round-trip losslessly: store it, read it back, and re-serialize it
    /// byte-for-byte — the whole point of `Payload::Unknown`/`EventKind::Unknown`
    /// (see their doc comments). Before this, an unrecognized `type` failed
    /// `Payload`'s deserialization outright, which — per `Msg::Event` parsing
    /// in `node.rs` — aborted the entire sync round carrying it.
    #[test]
    fn unrecognized_kind_and_payload_round_trip_losslessly() {
        let wire = serde_json::json!({
            "event_id": "ev-from-the-future",
            "device_id": "dev-abc",
            "seq": 1,
            "recorded_at": 1_726_000_000_000i64,
            "kind": "upgrade_notice_v2",
            "payload": {
                "type": "upgrade_notice_v2",
                "min_build": 42,
                "nested": { "a": [1, 2, 3] }
            }
        });

        let event: Event = serde_json::from_value(wire.clone()).expect("unknown kind must not fail to parse");
        assert_eq!(event.kind, EventKind::Unknown("upgrade_notice_v2".into()));
        match &event.payload {
            Payload::Unknown { type_tag, data } => {
                assert_eq!(type_tag, "upgrade_notice_v2");
                assert_eq!(data, &wire["payload"]);
            }
            other => panic!("expected Payload::Unknown, got {other:?}"),
        }

        // Re-serializing must reproduce exactly what a peer that DOES
        // understand this kind would still be able to parse.
        let round_tripped = serde_json::to_value(&event).unwrap();
        assert_eq!(round_tripped, wire);
    }

    #[test]
    fn known_kinds_still_round_trip_unchanged() {
        let payload = Payload::Infra {
            infra_kind: "peer_join".into(),
            target: Some("dev-abc".into()),
            data: serde_json::json!({}),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "infra", "infra_kind": "peer_join", "target": "dev-abc", "data": {}})
        );
        let back: Payload = serde_json::from_value(json).unwrap();
        assert_eq!(back, payload);
    }
}
