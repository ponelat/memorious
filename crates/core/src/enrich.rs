//! Enrichment scheduling, exactly as UNDERSTANDING.md:
//! capture always syncs first; the capturing device may flag `will_enrich`;
//! peers seeing the flag hold off for a grace period measured from *local
//! receipt*; after that any capable peer may enrich. Races are harmless —
//! latest annotation wins (tie-break: event id).

use std::collections::HashMap;

use anyhow::Result;

use crate::event::{Event, EventKind, Payload};
use crate::journal::Journal;
use crate::store::now_ms;

/// ~15 minutes, per the founding doc.
pub const DEFAULT_GRACE_MS: i64 = 15 * 60 * 1000;

impl Journal {
    /// Append a transcription/OCR annotation for a media capture.
    pub fn annotate(&self, target_event_id: &str, text: &str) -> Result<Event> {
        self.store.append_local(
            self.device_id(),
            EventKind::Annotation,
            Payload::Annotation {
                target: target_event_id.into(),
                text: text.into(),
            },
            false,
        )
    }

    /// Winning annotation per target: latest `recorded_at`, event id as tie-break.
    pub fn annotations(&self) -> Result<HashMap<String, String>> {
        let mut winners: HashMap<String, (i64, String, String)> = HashMap::new();
        for e in self.store.all_events()? {
            if let Payload::Annotation { target, text } = &e.payload {
                let key = (e.recorded_at, e.event_id.clone());
                match winners.get(target) {
                    Some((t, id, _)) if (*t, id.clone()) >= key => {}
                    _ => {
                        winners.insert(target.clone(), (key.0, key.1, text.clone()));
                    }
                }
            }
        }
        Ok(winners
            .into_iter()
            .map(|(target, (_, _, text))| (target, text))
            .collect())
    }

    /// Media captures this peer may enrich right now:
    /// unredacted, no annotation yet, and either captured here, or not flagged
    /// `will_enrich`, or the grace period (from local receipt) has expired.
    pub fn pending_enrichment(&self, grace_ms: i64) -> Result<Vec<Event>> {
        let annotated = self.annotations()?;
        let redacted = self.store.redacted_ids()?;
        let now = now_ms();
        let mut out = Vec::new();
        for e in self.store.all_events()? {
            if e.kind != EventKind::Capture || e.blob_hash().is_none() {
                continue;
            }
            if redacted.contains(&e.event_id) || annotated.contains_key(&e.event_id) {
                continue;
            }
            let ours = e.device_id == self.device_id();
            let grace_over = self
                .store
                .local_received_at(&e.event_id)?
                .map(|t| t + grace_ms <= now)
                .unwrap_or(true);
            if ours || !e.will_enrich || grace_over {
                out.push(e);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MediaKind;
    use tempfile::tempdir;

    fn media_capture(j: &Journal, will_enrich: bool) -> Event {
        j.store
            .append_local(
                j.device_id(),
                EventKind::Capture,
                Payload::media(MediaKind::Audio, "cafe".into(), 9),
                will_enrich,
            )
            .unwrap()
    }

    #[test]
    fn latest_annotation_wins_with_event_id_tiebreak() {
        let dir = tempdir().unwrap();
        let j = Journal::init(&dir.path().join("j")).unwrap();
        let cap = media_capture(&j, false);
        let a1 = j.annotate(&cap.event_id, "first pass").unwrap();
        let a2 = j.annotate(&cap.event_id, "better model").unwrap();
        // Same-millisecond appends: uuidv7 event ids still order a2 after a1.
        assert!(a2.event_id > a1.event_id || a2.recorded_at > a1.recorded_at);
        let winners = j.annotations().unwrap();
        assert_eq!(winners.get(&cap.event_id).unwrap(), "better model");
    }

    #[test]
    fn two_peers_converge_on_one_annotation_winner() {
        let dir = tempdir().unwrap();
        let a = Journal::init(&dir.path().join("a")).unwrap();
        let b = Journal::init_with_secret(&dir.path().join("b"), *a.secret()).unwrap();
        let cap = media_capture(&a, false);

        // b receives the capture, then both enrich concurrently.
        for e in a.store.events_missing_from(&b.store.heads().unwrap()).unwrap() {
            b.store.insert_remote(&e).unwrap();
        }
        a.annotate(&cap.event_id, "annotated by a").unwrap();
        b.annotate(&cap.event_id, "annotated by b").unwrap();

        // Union of logs both ways.
        for e in a.store.events_missing_from(&b.store.heads().unwrap()).unwrap() {
            b.store.insert_remote(&e).unwrap();
        }
        for e in b.store.events_missing_from(&a.store.heads().unwrap()).unwrap() {
            a.store.insert_remote(&e).unwrap();
        }

        let wa = a.annotations().unwrap();
        let wb = b.annotations().unwrap();
        assert_eq!(wa.get(&cap.event_id), wb.get(&cap.event_id), "same winner everywhere");
    }

    #[test]
    fn pending_respects_flag_grace_and_redaction() {
        let dir = tempdir().unwrap();
        let j = Journal::init(&dir.path().join("j")).unwrap();
        let plain = media_capture(&j, false);
        let flagged_remote = Event {
            event_id: "evt-remote".into(),
            device_id: "dev-elsewhere".into(),
            seq: 1,
            recorded_at: now_ms(),
            kind: EventKind::Capture,
            payload: Payload::media(MediaKind::Photo, "beef".into(), 4),
            will_enrich: true,
        };
        j.store.insert_remote(&flagged_remote).unwrap();

        // Long grace: our own unflagged capture is due; the remote flagged one waits.
        let due: Vec<_> = j
            .pending_enrichment(DEFAULT_GRACE_MS)
            .unwrap()
            .iter()
            .map(|e| e.event_id.clone())
            .collect();
        assert_eq!(due, vec![plain.event_id.clone()]);

        // Zero grace: the flagged one is now fair game too.
        assert_eq!(j.pending_enrichment(0).unwrap().len(), 2);

        // Annotated and redacted entries drop out.
        j.annotate(&plain.event_id, "done").unwrap();
        j.redact("evt-remote").unwrap();
        assert!(j.pending_enrichment(0).unwrap().is_empty());
    }

    #[test]
    fn own_flagged_capture_is_immediately_due_locally() {
        let dir = tempdir().unwrap();
        let j = Journal::init(&dir.path().join("j")).unwrap();
        let mine = media_capture(&j, true);
        let due = j.pending_enrichment(DEFAULT_GRACE_MS).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].event_id, mine.event_id);
    }
}
