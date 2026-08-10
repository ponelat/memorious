//! The journal: a data directory holding the event log, blob store, and identity.
//!
//! Identity model: the journal secret is shared by all paired devices (possession = trust);
//! the device id is unique per device and namespaces its seq counter.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rand::TryRngCore;

use crate::event::{Event, EventKind, Payload};
use crate::store::Store;

/// 32-byte shared journal secret, hex-encoded in tickets and storage.
pub const SECRET_LEN: usize = 32;

pub struct Journal {
    pub store: Store,
    root: PathBuf,
    device_id: String,
    secret: [u8; SECRET_LEN],
}

impl Journal {
    /// Create a brand-new journal (fresh secret) in `root`. Fails if one exists.
    pub fn init(root: &Path) -> Result<Self> {
        let mut secret = [0u8; SECRET_LEN];
        rand::rngs::OsRng.try_fill_bytes(&mut secret)?;
        Self::init_with_secret(root, secret)
    }

    /// Create a journal joined to an existing one (secret from a pairing ticket).
    pub fn init_with_secret(root: &Path, secret: [u8; SECRET_LEN]) -> Result<Self> {
        if root.join("db.sqlite").exists() {
            bail!("journal already exists at {}", root.display());
        }
        std::fs::create_dir_all(root.join("blobs")).context("create data dir")?;
        let store = Store::open(&root.join("db.sqlite"))?;
        let device_id = format!("dev-{}", uuid::Uuid::now_v7().simple());
        store.meta_set("device_id", device_id.as_bytes())?;
        store.meta_set("journal_secret", &secret)?;
        Ok(Self {
            store,
            root: root.to_path_buf(),
            device_id,
            secret,
        })
    }

    /// Open an existing journal.
    pub fn open(root: &Path) -> Result<Self> {
        if !root.join("db.sqlite").exists() {
            bail!("no journal at {} (run init first)", root.display());
        }
        let store = Store::open(&root.join("db.sqlite"))?;
        let device_id = String::from_utf8(
            store
                .meta_get("device_id")?
                .context("journal missing device_id")?,
        )?;
        let secret_bytes = store
            .meta_get("journal_secret")?
            .context("journal missing secret")?;
        let secret: [u8; SECRET_LEN] = secret_bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("malformed journal secret"))?;
        Ok(Self {
            store,
            root: root.to_path_buf(),
            device_id,
            secret,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn secret(&self) -> &[u8; SECRET_LEN] {
        &self.secret
    }

    // ---- capture ----

    pub fn capture_text(&self, text: &str) -> Result<Event> {
        self.store.append_local(
            &self.device_id,
            EventKind::Capture,
            Payload::Text { text: text.into() },
            false,
        )
    }

    pub fn redact(&self, target_event_id: &str) -> Result<Event> {
        let target = self
            .store
            .get_event(target_event_id)?
            .with_context(|| format!("no such event {target_event_id}"))?;
        if target.kind != EventKind::Capture {
            bail!("only captures can be redacted");
        }
        self.store.append_local(
            &self.device_id,
            EventKind::Redact,
            Payload::Redact {
                target: target_event_id.into(),
            },
            false,
        )
    }

    /// The visible timeline: captures in (recorded_at, event_id) order, redacted ones removed.
    pub fn list(&self) -> Result<Vec<Event>> {
        let redacted = self.store.redacted_ids()?;
        Ok(self
            .store
            .all_events()?
            .into_iter()
            .filter(|e| e.kind == EventKind::Capture && !redacted.contains(&e.event_id))
            .collect())
    }

    /// Redacted captures (the trash view).
    pub fn trash(&self) -> Result<Vec<Event>> {
        let redacted = self.store.redacted_ids()?;
        Ok(self
            .store
            .all_events()?
            .into_iter()
            .filter(|e| e.kind == EventKind::Capture && redacted.contains(&e.event_id))
            .collect())
    }

    // ---- browser passcode ----

    /// Set the browser passcode. Stores only the blake3 hash; latest token-set wins.
    pub fn set_passcode(&self, passcode: &str) -> Result<Event> {
        self.store.append_local(
            &self.device_id,
            EventKind::TokenSet,
            Payload::TokenSet {
                hash: blake3::hash(passcode.as_bytes()).to_hex().to_string(),
            },
            false,
        )
    }

    /// Check a passcode against the latest token-set event.
    /// Ordering: recorded_at, then device_id as the tiebreak (log ordering).
    pub fn check_passcode(&self, passcode: &str) -> Result<bool> {
        let Some(active) = self.active_passcode_hash()? else {
            return Ok(false); // no passcode set: browser access denied
        };
        Ok(blake3::hash(passcode.as_bytes()).to_hex().to_string() == active)
    }

    pub fn active_passcode_hash(&self) -> Result<Option<String>> {
        // Latest wins: recorded_at, then device_id, then seq (same-device same-ms sets).
        let mut latest: Option<((i64, String, u64), String)> = None;
        for e in self.store.all_events()? {
            if let Payload::TokenSet { hash } = &e.payload {
                let key = (e.recorded_at, e.device_id.clone(), e.seq);
                if latest.as_ref().map(|(k, _)| key > *k).unwrap_or(true) {
                    latest = Some((key, hash.clone()));
                }
            }
        }
        Ok(latest.map(|(_, h)| h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn init_open_round_trip_preserves_identity() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("j");
        let j = Journal::init(&root).unwrap();
        let (dev, secret) = (j.device_id().to_string(), *j.secret());
        drop(j);
        let j = Journal::open(&root).unwrap();
        assert_eq!(j.device_id(), dev);
        assert_eq!(j.secret(), &secret);
        assert!(root.join("blobs").is_dir());
    }

    #[test]
    fn init_refuses_existing_journal() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("j");
        Journal::init(&root).unwrap();
        assert!(Journal::init(&root).is_err());
    }

    #[test]
    fn init_with_secret_joins_existing_journal() {
        let dir = tempdir().unwrap();
        let a = Journal::init(&dir.path().join("a")).unwrap();
        let b = Journal::init_with_secret(&dir.path().join("b"), *a.secret()).unwrap();
        assert_eq!(a.secret(), b.secret());
        assert_ne!(a.device_id(), b.device_id());
    }

    #[test]
    fn list_hides_redacted_trash_shows_them() {
        let dir = tempdir().unwrap();
        let j = Journal::init(&dir.path().join("j")).unwrap();
        let keep = j.capture_text("keep").unwrap();
        let toss = j.capture_text("toss").unwrap();
        j.redact(&toss.event_id).unwrap();
        let listed: Vec<_> = j.list().unwrap().iter().map(|e| e.event_id.clone()).collect();
        assert_eq!(listed, vec![keep.event_id]);
        let trashed: Vec<_> = j.trash().unwrap().iter().map(|e| e.event_id.clone()).collect();
        assert_eq!(trashed, vec![toss.event_id]);
    }

    #[test]
    fn redact_requires_existing_capture() {
        let dir = tempdir().unwrap();
        let j = Journal::init(&dir.path().join("j")).unwrap();
        assert!(j.redact("nope").is_err());
        let r = j.capture_text("x").unwrap();
        let redaction = j.redact(&r.event_id).unwrap();
        // a redaction itself can't be redacted
        assert!(j.redact(&redaction.event_id).is_err());
    }

    #[test]
    fn passcode_latest_token_set_wins() {
        let dir = tempdir().unwrap();
        let j = Journal::init(&dir.path().join("j")).unwrap();
        assert!(!j.check_passcode("anything").unwrap()); // none set yet
        j.set_passcode("first").unwrap();
        assert!(j.check_passcode("first").unwrap());
        j.set_passcode("second").unwrap();
        assert!(!j.check_passcode("first").unwrap());
        assert!(j.check_passcode("second").unwrap());
    }
}
