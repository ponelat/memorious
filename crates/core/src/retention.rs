//! # Media retention — which blobs a device may drop, and why
//!
//! This is the **single source of truth** for media retention across every
//! face of Memorious (server, desktop, iOS, browser). Nothing outside this
//! file decides whether a blob may be evicted; the other layers only gather
//! facts from the log and act on the answer. If you are changing retention
//! behaviour, change it here and nowhere else. UNDERSTANDING.md
//! §"Audio kinds and media retention" is the design-level statement; this
//! file is the executable one, and the tests at the bottom are the contract.
//!
//! ## The idea in one paragraph
//!
//! The event log is append-only and is never touched by retention. What a
//! device *may* forget is the **blob** behind a capture — the bytes in the
//! content-addressed store — because a blob can always be fetched again from
//! any peer that still holds it (the hash lives in the event forever). So
//! "eviction" is cache eviction, not deletion: the journal keeps every entry,
//! every transcript, and every hash; a device merely stops holding some bytes
//! locally. A peer whose policy is "keep everything" (the always-on server)
//! is what makes a pruned phone recoverable.
//!
//! ## Nothing is marked, nothing is synced
//!
//! Eligibility is **derived** from facts already in the log plus a
//! **per-device policy** that lives only in that device's local meta table.
//! There is no "evictable" flag, no retention event, no fifth event kind.
//! Two devices with different policies legitimately disagree about the same
//! blob, and that is the point: the phone frees space, the server keeps all.
//!
//! ## The rules
//!
//! A blob is a candidate for eviction on *this* device only if **every**
//! capture that references it is evictable under one of these rules
//! (`disposition`), and the device's policy window for that rule has elapsed:
//!
//! 1. **Voice audio with a transcript.** `Payload::Audio { audio_kind: Voice }`
//!    whose winning annotation is non-empty. For a dictated note the transcript
//!    *is* the record; the audio was only ever the means of producing it. The
//!    window (`policy.voice_audio`) counts from when the winning annotation
//!    arrived on this device — *local receipt*, never capture time, for the
//!    same reason enrichment's grace period does (`enrich.rs`): a note that
//!    synced late must not arrive pre-expired. An empty transcript does **not**
//!    qualify — a silent whisper pass is a failed transcription, and a better
//!    model next year should still get a chance at the audio somewhere.
//!
//! 2. **Redacted entries.** Any media capture that has been redacted. Trash is
//!    recoverable, so the window (`policy.redacted`) gives a redaction time to
//!    be undone before the bytes go; it counts from local receipt of the
//!    redact event.
//!
//! Everything else is **kept**, unconditionally:
//!
//! - `Music` audio. The recording is the record. It is never transcribed
//!    (`enrich.rs` skips it — also decided here, see `wants_transcription`)
//!    and never evicted by policy. This is the whole reason `AudioKind` exists.
//! - Photos and video — there is no text stand-in for them.
//! - Voice audio that has no transcript yet, or only an empty one.
//! - Anything within its policy window, or under a `Retain::Forever` policy.
//!
//! ## What this costs
//!
//! Once the *last* peer holding a voice blob evicts it, "latest annotation
//! wins — re-run a better model next year" is off the table for that note.
//! That is why the shipped defaults are asymmetric: phones evict, the server
//! keeps everything (`RetentionPolicy::KEEP_EVERYTHING`). Turning on eviction
//! on the always-on peer is a deliberate choice to make transcripts the only
//! record.
//!
//! ## Mechanics (for orientation; the logic is above)
//!
//! - `Journal::evictable_blob_hashes` gathers the facts for every referenced
//!   blob and applies `disposition` per capture, then ANDs per hash.
//! - `Node` hands iroh-blobs a GC "protect" callback: every referenced hash
//!   that is *not* evictable is protected; the log is the only GC root
//!   (ingest tags are dropped once the capture event exists).
//! - Sync (`fetch_missing_blobs`) skips evictable hashes, otherwise the next
//!   sync would pull back what GC just removed.
//! - A missing local blob renders as the transcript plus an "audio pruned"
//!   marker; nothing else in the entry changes.

use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::event::{AudioKind, Event, EventKind, Payload};
use crate::journal::Journal;

pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// How long a device keeps an evictable blob after it became evictable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "days")]
pub enum Retain {
    /// Never evict under this rule. The safe default.
    Forever,
    /// Evict once the rule's trigger is at least this many days old
    /// (measured from local receipt — see module docs).
    AfterDays(u32),
}

impl Retain {
    fn elapsed(&self, trigger_local_ms: i64, now_ms: i64) -> bool {
        match self {
            Retain::Forever => false,
            Retain::AfterDays(d) => now_ms - trigger_local_ms >= (*d as i64) * DAY_MS,
        }
    }
}

/// Per-device retention policy. Stored in the device's local meta table
/// (`Journal::retention_policy`), never in the event log, never synced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Rule 1: transcribed voice audio.
    pub voice_audio: Retain,
    /// Rule 2: media of redacted entries.
    pub redacted: Retain,
}

impl RetentionPolicy {
    /// The always-on peer's policy, and the default for any device that has
    /// never chosen one: hold every blob forever.
    pub const KEEP_EVERYTHING: RetentionPolicy = RetentionPolicy {
        voice_audio: Retain::Forever,
        redacted: Retain::Forever,
    };

    /// What a phone ships with: a week to re-listen to a transcribed note,
    /// a month to change your mind about a redaction.
    pub const PHONE: RetentionPolicy = RetentionPolicy {
        voice_audio: Retain::AfterDays(7),
        redacted: Retain::AfterDays(30),
    };
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self::KEEP_EVERYTHING
    }
}

/// Everything `disposition` needs to know about one capture, gathered by the
/// caller from the log. All times are unix ms. "Local" times are this
/// device's receipt times (`Store::local_received_at`), not `recorded_at`.
#[derive(Debug, Clone, Copy)]
pub struct CaptureFacts<'a> {
    pub event: &'a Event,
    /// Local receipt time of the redact event targeting this capture, if any.
    pub redacted_local_ms: Option<i64>,
    /// Local receipt time of the *winning* annotation, if it is non-empty.
    /// Callers must pass `None` for an empty winner.
    pub transcript_local_ms: Option<i64>,
    pub now_ms: i64,
}

/// Why a blob is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// Photos, video, and music: there is no stand-in for the bytes.
    IrreplaceableMedia,
    /// Voice audio with no (non-empty) transcript yet.
    AwaitingTranscript,
    /// Evictable in principle, but inside the policy window or `Forever`.
    WithinWindow,
}

/// Why a blob may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictReason {
    /// Rule 1.
    TranscribedVoice,
    /// Rule 2.
    Redacted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Keep(KeepReason),
    Evictable(EvictReason),
}

impl Disposition {
    pub fn is_evictable(&self) -> bool {
        matches!(self, Disposition::Evictable(_))
    }
}

/// The decision for one capture under one device's policy. Pure; the whole
/// policy is readable from this function plus `RetentionPolicy`.
pub fn disposition(facts: CaptureFacts<'_>, policy: &RetentionPolicy) -> Disposition {
    // Rule 2 first: redaction makes any media kind a candidate, music included —
    // the owner struck the entry out, so its bytes are no longer "the record".
    if let Some(redacted_at) = facts.redacted_local_ms {
        return if policy.redacted.elapsed(redacted_at, facts.now_ms) {
            Disposition::Evictable(EvictReason::Redacted)
        } else {
            Disposition::Keep(KeepReason::WithinWindow)
        };
    }

    match &facts.event.payload {
        Payload::Audio { audio_kind: AudioKind::Voice, .. } => match facts.transcript_local_ms {
            None => Disposition::Keep(KeepReason::AwaitingTranscript),
            Some(transcribed_at) => {
                if policy.voice_audio.elapsed(transcribed_at, facts.now_ms) {
                    Disposition::Evictable(EvictReason::TranscribedVoice)
                } else {
                    Disposition::Keep(KeepReason::WithinWindow)
                }
            }
        },
        Payload::Audio { audio_kind: AudioKind::Music, .. }
        | Payload::Photo { .. }
        | Payload::Video { .. } => Disposition::Keep(KeepReason::IrreplaceableMedia),
        // Not media at all — nothing to evict. Callers never pass these, but
        // "keep" is the only honest answer.
        _ => Disposition::Keep(KeepReason::IrreplaceableMedia),
    }
}

/// Whether enrichment should transcribe this capture. Audio is transcribed
/// only when it is voice; music is never run through a speech model. Photos
/// (OCR) are unaffected. Kept here so the "music is the record" decision has
/// exactly one home.
pub fn wants_transcription(event: &Event) -> bool {
    !matches!(
        event.payload,
        Payload::Audio { audio_kind: AudioKind::Music, .. }
    )
}

const POLICY_META_KEY: &str = "retention_policy";

impl Journal {
    /// This device's retention policy. `KEEP_EVERYTHING` until one is set —
    /// a device never evicts anything it was not explicitly told to.
    pub fn retention_policy(&self) -> Result<RetentionPolicy> {
        Ok(match self.store.meta_get(POLICY_META_KEY)? {
            Some(bytes) => serde_json::from_slice(&bytes)?,
            None => RetentionPolicy::KEEP_EVERYTHING,
        })
    }

    /// The policy this device was explicitly given, if any. Faces use this to
    /// install their default exactly once (a phone: `PHONE`) without
    /// overriding a later choice.
    pub fn retention_policy_if_set(&self) -> Result<Option<RetentionPolicy>> {
        Ok(match self.store.meta_get(POLICY_META_KEY)? {
            Some(bytes) => Some(serde_json::from_slice(&bytes)?),
            None => None,
        })
    }

    /// Local meta only — never an event, never synced.
    pub fn set_retention_policy(&self, policy: &RetentionPolicy) -> Result<()> {
        self.store
            .meta_set(POLICY_META_KEY, &serde_json::to_vec(policy)?)
    }

    /// Blob hashes this device may drop right now under `policy`: gathers the
    /// facts for every media capture, asks `disposition`, and keeps a hash only
    /// if *every* capture referencing it says evictable. Sorted, deduplicated.
    pub fn evictable_blob_hashes(&self, policy: &RetentionPolicy, now_ms: i64) -> Result<Vec<String>> {
        if *policy == RetentionPolicy::KEEP_EVERYTHING {
            return Ok(Vec::new());
        }
        let events = self.store.all_events()?;

        // Winning annotation per target (same rule as `Journal::annotations`),
        // but we need the event, for its local receipt time.
        let mut winners: HashMap<&str, &Event> = HashMap::new();
        let mut redactions: HashMap<&str, &Event> = HashMap::new();
        for e in &events {
            match &e.payload {
                Payload::Annotation { target, .. } => {
                    let better = match winners.get(target.as_str()) {
                        Some(w) => (e.recorded_at, &e.event_id) > (w.recorded_at, &w.event_id),
                        None => true,
                    };
                    if better {
                        winners.insert(target, e);
                    }
                }
                Payload::Redact { target } => {
                    // First redaction wins the clock: the earliest strike-through
                    // is when the entry left the timeline.
                    redactions.entry(target).or_insert(e);
                }
                _ => {}
            }
        }

        // Rows that predate the `local_received_at` column carry 0; for those
        // the event's own timestamp is the best clock we have.
        let local_ms = |e: &Event| -> Result<Option<i64>> {
            Ok(Some(match self.store.local_received_at(&e.event_id)? {
                Some(t) if t > 0 => t,
                _ => e.recorded_at,
            }))
        };

        // hash → all captures agree it may go?
        let mut verdict: HashMap<&str, bool> = HashMap::new();
        for e in &events {
            if e.kind != EventKind::Capture {
                continue;
            }
            let Some(hash) = e.blob_hash() else { continue };
            let transcript_local_ms = match winners.get(e.event_id.as_str()) {
                Some(a) if matches!(&a.payload, Payload::Annotation { text, .. } if !text.trim().is_empty()) => {
                    local_ms(a)?
                }
                _ => None,
            };
            let redacted_local_ms = match redactions.get(e.event_id.as_str()) {
                Some(r) => local_ms(r)?,
                None => None,
            };
            let d = disposition(
                CaptureFacts { event: e, redacted_local_ms, transcript_local_ms, now_ms },
                policy,
            );
            let slot = verdict.entry(hash).or_insert(true);
            *slot = *slot && d.is_evictable();
        }
        let mut out: Vec<String> = verdict
            .into_iter()
            .filter(|(_, ok)| *ok)
            .map(|(h, _)| h.to_string())
            .collect();
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(payload: Payload) -> Event {
        Event {
            event_id: "evt-1".into(),
            device_id: "dev-a".into(),
            seq: 1,
            recorded_at: 0,
            kind: EventKind::Capture,
            payload,
            will_enrich: false,
        }
    }

    fn voice() -> Event {
        capture(Payload::Audio {
            hash: "a".into(),
            size: 1,
            crypto: None,
            audio_kind: AudioKind::Voice,
        })
    }

    fn music() -> Event {
        capture(Payload::Audio {
            hash: "m".into(),
            size: 1,
            crypto: None,
            audio_kind: AudioKind::Music,
        })
    }

    fn facts(e: &Event) -> CaptureFacts<'_> {
        CaptureFacts {
            event: e,
            redacted_local_ms: None,
            transcript_local_ms: None,
            now_ms: 100 * DAY_MS,
        }
    }

    #[test]
    fn voice_without_transcript_is_kept_even_under_eager_policy() {
        let e = voice();
        let d = disposition(facts(&e), &RetentionPolicy::PHONE);
        assert_eq!(d, Disposition::Keep(KeepReason::AwaitingTranscript));
    }

    #[test]
    fn transcribed_voice_is_evictable_only_after_window() {
        let e = voice();
        let now = 100 * DAY_MS;
        let fresh = CaptureFacts {
            transcript_local_ms: Some(now - 6 * DAY_MS),
            ..facts(&e)
        };
        let stale = CaptureFacts {
            transcript_local_ms: Some(now - 7 * DAY_MS),
            ..facts(&e)
        };
        assert_eq!(
            disposition(fresh, &RetentionPolicy::PHONE),
            Disposition::Keep(KeepReason::WithinWindow)
        );
        assert_eq!(
            disposition(stale, &RetentionPolicy::PHONE),
            Disposition::Evictable(EvictReason::TranscribedVoice)
        );
    }

    #[test]
    fn keep_everything_never_evicts() {
        let e = voice();
        let ancient = CaptureFacts {
            transcript_local_ms: Some(0),
            redacted_local_ms: Some(0),
            ..facts(&e)
        };
        assert!(!disposition(ancient, &RetentionPolicy::KEEP_EVERYTHING).is_evictable());
    }

    #[test]
    fn music_and_photos_are_never_evicted_by_transcript() {
        let m = music();
        let p = capture(Payload::Photo { hash: "p".into(), size: 1, crypto: None });
        for e in [&m, &p] {
            let f = CaptureFacts {
                transcript_local_ms: Some(0),
                ..facts(e)
            };
            assert_eq!(
                disposition(f, &RetentionPolicy::PHONE),
                Disposition::Keep(KeepReason::IrreplaceableMedia)
            );
        }
    }

    #[test]
    fn redaction_makes_anything_evictable_after_its_window() {
        let m = music();
        let now = 100 * DAY_MS;
        let recent = CaptureFacts {
            redacted_local_ms: Some(now - 29 * DAY_MS),
            ..facts(&m)
        };
        let old = CaptureFacts {
            redacted_local_ms: Some(now - 30 * DAY_MS),
            ..facts(&m)
        };
        assert_eq!(
            disposition(recent, &RetentionPolicy::PHONE),
            Disposition::Keep(KeepReason::WithinWindow)
        );
        assert_eq!(
            disposition(old, &RetentionPolicy::PHONE),
            Disposition::Evictable(EvictReason::Redacted)
        );
    }

    #[test]
    fn only_voice_wants_transcription() {
        assert!(wants_transcription(&voice()));
        assert!(!wants_transcription(&music()));
        let p = capture(Payload::Photo { hash: "p".into(), size: 1, crypto: None });
        assert!(wants_transcription(&p));
    }

    #[test]
    fn audio_kind_defaults_to_voice_on_the_wire() {
        // Pre-2026-08-23 events carry no `audio_kind`; they must read as voice
        // and must serialize back without the field (old peers stay happy).
        let old = r#"{"type":"audio","hash":"h","size":3}"#;
        let p: Payload = serde_json::from_str(old).unwrap();
        assert_eq!(p.audio_kind(), Some(AudioKind::Voice));
        assert_eq!(serde_json::to_string(&p).unwrap(), old);

        let m = Payload::Audio {
            hash: "h".into(),
            size: 3,
            crypto: None,
            audio_kind: AudioKind::Music,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""audio_kind":"music""#), "{s}");
        assert_eq!(serde_json::from_str::<Payload>(&s).unwrap(), m);
    }

    #[test]
    fn journal_evictable_hashes_follow_policy() {
        use crate::journal::Journal;
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::init(&dir.path().join("j"), "pw").unwrap();
        let add = |p: Payload| {
            j.store
                .append_local(j.device_id(), EventKind::Capture, p, false)
                .unwrap()
        };
        let voice = add(Payload::Audio { hash: "v".into(), size: 1, crypto: None, audio_kind: AudioKind::Voice });
        let silent = add(Payload::Audio { hash: "s".into(), size: 1, crypto: None, audio_kind: AudioKind::Voice });
        let untranscribed = add(Payload::Audio { hash: "u".into(), size: 1, crypto: None, audio_kind: AudioKind::Voice });
        let music = add(Payload::Audio { hash: "m".into(), size: 1, crypto: None, audio_kind: AudioKind::Music });
        let photo = add(Payload::Photo { hash: "p".into(), size: 1, crypto: None });
        let trashed = add(Payload::Photo { hash: "t".into(), size: 1, crypto: None });
        j.annotate(&voice.event_id, "the transcript").unwrap();
        j.annotate(&silent.event_id, "").unwrap();
        j.annotate(&music.event_id, "[Music]").unwrap();
        j.redact(&trashed.event_id).unwrap();
        let _ = (untranscribed, photo);

        let eager = RetentionPolicy { voice_audio: Retain::AfterDays(0), redacted: Retain::AfterDays(0) };
        let mut got = j.evictable_blob_hashes(&eager, crate::store::now_ms()).unwrap();
        got.sort();
        assert_eq!(got, vec!["t".to_string(), "v".to_string()]);

        assert!(j
            .evictable_blob_hashes(&RetentionPolicy::KEEP_EVERYTHING, crate::store::now_ms())
            .unwrap()
            .is_empty());

        // A later, better annotation that is empty un-qualifies the blob again:
        // the winner is what counts.
        j.annotate(&voice.event_id, "").unwrap();
        let got = j.evictable_blob_hashes(&eager, crate::store::now_ms()).unwrap();
        assert_eq!(got, vec!["t".to_string()]);
    }

    #[test]
    fn retention_policy_persists_per_device_in_meta() {
        use crate::journal::Journal;
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::init(&dir.path().join("j"), "pw").unwrap();
        assert_eq!(j.retention_policy().unwrap(), RetentionPolicy::KEEP_EVERYTHING);
        j.set_retention_policy(&RetentionPolicy::PHONE).unwrap();
        drop(j);
        let j = Journal::open(&dir.path().join("j"), "pw").unwrap();
        assert_eq!(j.retention_policy().unwrap(), RetentionPolicy::PHONE);
        // Policy never enters the log.
        assert!(j.store.all_events().unwrap().iter().all(|e| e.kind == EventKind::Capture || e.kind == EventKind::Annotation || e.kind == EventKind::Redact || e.kind == EventKind::TokenSet));
    }

    #[test]
    fn policy_round_trips_as_json() {
        let s = serde_json::to_string(&RetentionPolicy::PHONE).unwrap();
        assert_eq!(
            s,
            r#"{"voice_audio":{"mode":"after_days","days":7},"redacted":{"mode":"after_days","days":30}}"#
        );
        assert_eq!(
            serde_json::from_str::<RetentionPolicy>(&s).unwrap(),
            RetentionPolicy::PHONE
        );
        let f = serde_json::to_string(&RetentionPolicy::KEEP_EVERYTHING).unwrap();
        assert_eq!(f, r#"{"voice_audio":{"mode":"forever"},"redacted":{"mode":"forever"}}"#);
    }
}
