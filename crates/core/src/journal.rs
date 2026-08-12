//! The journal: a data directory holding the event log, blob store, and identity.
//!
//! Identity model: the journal secret is shared by all paired devices (possession = trust);
//! the device id is unique per device and namespaces its seq counter.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rand::TryRngCore;
use serde::{Deserialize, Serialize};

use crate::crypto::{self, KdfParams, KeySet};
use crate::event::{BlobCrypto, Event, EventKind, Payload};
use crate::store::Store;

/// 32-byte shared journal secret, hex-encoded in tickets and storage.
pub const SECRET_LEN: usize = 32;

const KEYS_FILE: &str = "keys.json";

/// Plaintext sidecar holding what unlock needs *before* the database opens:
/// the Argon2id salt (derived from the journal secret, not sensitive) and the
/// KDF parameters. Never key material.
#[derive(Serialize, Deserialize)]
struct KeysFile {
    version: u32,
    kdf: String,
    /// base64, 16 bytes.
    salt: String,
    #[serde(flatten)]
    params: KdfParams,
}

/// Traffic-light summary of replication state; see [`Journal::sync_health`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncHealth {
    pub color: String, // "green" | "yellow" | "red"
    pub pending: bool,
    pub stalest_ms: Option<i64>,
    pub peers: usize,
}

pub struct Journal {
    pub store: Store,
    root: PathBuf,
    device_id: String,
    secret: [u8; SECRET_LEN],
    keys: KeySet,
}

impl Journal {
    /// Create a brand-new journal (fresh secret) in `root`. Fails if one exists.
    pub fn init(root: &Path, password: &str) -> Result<Self> {
        let mut secret = [0u8; SECRET_LEN];
        rand::rngs::OsRng.try_fill_bytes(&mut secret)?;
        Self::init_with_secret(root, secret, password)
    }

    /// Create a journal joined to an existing one (secret from a pairing ticket).
    /// The password must be the journal's master password — media keys wrapped
    /// by other devices won't unwrap otherwise (checked after the first sync).
    pub fn init_with_secret(root: &Path, secret: [u8; SECRET_LEN], password: &str) -> Result<Self> {
        if root.join("db.sqlite").exists() {
            bail!("journal already exists at {}", root.display());
        }
        std::fs::create_dir_all(root.join("blobs")).context("create data dir")?;
        let salt = crypto::salt_from_secret(&secret);
        let params = KdfParams::default();
        let keys_file = KeysFile {
            version: 1,
            kdf: "argon2id".into(),
            salt: data_encoding::BASE64.encode(&salt),
            params,
        };
        std::fs::write(
            root.join(KEYS_FILE),
            serde_json::to_vec_pretty(&keys_file)?,
        )
        .context("write keys.json")?;
        let keys = KeySet::derive(password, &salt, &params)?;
        let store = Store::open(&root.join("db.sqlite"), &keys.db_key_hex())?;
        let device_id = format!("dev-{}", uuid::Uuid::now_v7().simple());
        store.meta_set("device_id", device_id.as_bytes())?;
        store.meta_set("journal_secret", &secret)?;
        Ok(Self {
            store,
            root: root.to_path_buf(),
            device_id,
            secret,
            keys,
        })
    }

    /// Open an existing journal with its master password.
    pub fn open(root: &Path, password: &str) -> Result<Self> {
        if !root.join("db.sqlite").exists() {
            bail!("no journal at {} (run init first)", root.display());
        }
        let keys_path = root.join(KEYS_FILE);
        if !keys_path.exists() {
            bail!(
                "journal at {} predates encryption at rest — run `memorious migrate-encrypt`",
                root.display()
            );
        }
        let keys_file: KeysFile = serde_json::from_slice(
            &std::fs::read(&keys_path).context("read keys.json")?,
        )
        .context("parse keys.json")?;
        if keys_file.version != 1 || keys_file.kdf != "argon2id" {
            bail!("keys.json from a newer memorious — upgrade this build");
        }
        let salt_vec = data_encoding::BASE64
            .decode(keys_file.salt.as_bytes())
            .context("keys.json salt")?;
        let salt: [u8; crypto::SALT_LEN] = salt_vec
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("keys.json salt has wrong length"))?;
        let keys = KeySet::derive(password, &salt, &keys_file.params)?;
        let store = Store::open(&root.join("db.sqlite"), &keys.db_key_hex())?;
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
            keys,
        })
    }

    // ---- media keys ----

    /// Wrap a sealed blob's key material into a capture event's envelope.
    pub fn wrap_blob_keys(&self, sealed: &crypto::Sealed) -> Result<BlobCrypto> {
        self.keys.wrap(&sealed.ck, &sealed.nonce_base)
    }

    /// Recover a blob's (content key, nonce base) from its capture payload.
    pub fn unwrap_blob_keys(
        &self,
        crypto: &BlobCrypto,
    ) -> Result<([u8; crypto::KEY_LEN], [u8; crypto::NONCE_BASE_LEN])> {
        self.keys.unwrap(crypto)
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

    // ---- sync health ----

    /// A completed event sync with `peer` (either direction): remember when,
    /// and snapshot our heads so "pending to push" is detectable later.
    pub fn record_sync_contact(&self, peer: &str, now_ms: i64) -> Result<()> {
        self.store
            .meta_set(&format!("peer_last_ok:{peer}"), &now_ms.to_le_bytes())?;
        let heads = self.store.heads()?;
        self.store
            .meta_set("last_sync_heads", &serde_json::to_vec(&heads)?)?;
        Ok(())
    }

    /// Traffic light for the sync status UX (UNDERSTANDING.md): red = a known
    /// peer unheard-from for 48h (outranks all), yellow = local data no peer
    /// has picked up yet, green = converged (or solo — nowhere to push).
    pub fn sync_health(&self, now_ms: i64) -> Result<SyncHealth> {
        const STALE_MS: i64 = 48 * 3600 * 1000;
        let peers = self.store.meta_scan("peer_last_ok:")?;
        if peers.is_empty() {
            return Ok(SyncHealth { color: "green".into(), pending: false, stalest_ms: None, peers: 0 });
        }
        let stalest = peers
            .iter()
            .filter_map(|(_, v)| v.as_slice().try_into().ok().map(i64::from_le_bytes))
            .min()
            .unwrap_or(0);
        let pending = match self.store.meta_get("last_sync_heads")? {
            Some(bytes) => {
                let then: crate::store::Heads = serde_json::from_slice(&bytes).unwrap_or_default();
                self.store.heads()? != then
            }
            None => true,
        };
        let color = if now_ms - stalest > STALE_MS {
            "red"
        } else if pending {
            "yellow"
        } else {
            "green"
        };
        Ok(SyncHealth {
            color: color.into(),
            pending,
            stalest_ms: Some(stalest),
            peers: peers.len(),
        })
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

    const PW: &str = "test password";

    #[test]
    fn init_open_round_trip_preserves_identity() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("j");
        let j = Journal::init(&root, PW).unwrap();
        let (dev, secret) = (j.device_id().to_string(), *j.secret());
        drop(j);
        let j = Journal::open(&root, PW).unwrap();
        assert_eq!(j.device_id(), dev);
        assert_eq!(j.secret(), &secret);
        assert!(root.join("blobs").is_dir());
        assert!(root.join("keys.json").is_file());
    }

    #[test]
    fn wrong_password_fails_to_open() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("j");
        Journal::init(&root, PW).unwrap();
        let err = Journal::open(&root, "not it").err().expect("must fail");
        assert!(format!("{err:#}").contains("wrong master password"));
    }

    #[test]
    fn pre_encryption_journal_gets_a_pointed_error() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("j");
        std::fs::create_dir_all(&root).unwrap();
        // A legacy journal: db.sqlite present, no keys.json.
        crate::store::Store::open_plaintext(&root.join("db.sqlite")).unwrap();
        let err = Journal::open(&root, PW).err().expect("must fail");
        assert!(format!("{err:#}").contains("migrate-encrypt"));
    }

    #[test]
    fn init_refuses_existing_journal() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("j");
        Journal::init(&root, PW).unwrap();
        assert!(Journal::init(&root, PW).is_err());
    }

    #[test]
    fn init_with_secret_joins_existing_journal() {
        let dir = tempdir().unwrap();
        let a = Journal::init(&dir.path().join("a"), PW).unwrap();
        let b = Journal::init_with_secret(&dir.path().join("b"), *a.secret(), PW).unwrap();
        assert_eq!(a.secret(), b.secret());
        assert_ne!(a.device_id(), b.device_id());
        // Same secret + same password ⇒ identical wrapping keys: a key wrapped
        // on one device unwraps on the other (the sync story for media).
        let sealed = crate::crypto::seal(b"pixels").unwrap();
        let envelope = a.wrap_blob_keys(&sealed).unwrap();
        let (ck, nb) = b.unwrap_blob_keys(&envelope).unwrap();
        assert_eq!(ck, sealed.ck);
        assert_eq!(nb, sealed.nonce_base);
        // A device joined with the wrong password cannot unwrap.
        let c = Journal::init_with_secret(&dir.path().join("c"), *a.secret(), "typo").unwrap();
        assert!(c.unwrap_blob_keys(&envelope).is_err());
    }

    #[test]
    fn list_hides_redacted_trash_shows_them() {
        let dir = tempdir().unwrap();
        let j = Journal::init(&dir.path().join("j"), PW).unwrap();
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
        let j = Journal::init(&dir.path().join("j"), PW).unwrap();
        assert!(j.redact("nope").is_err());
        let r = j.capture_text("x").unwrap();
        let redaction = j.redact(&r.event_id).unwrap();
        // a redaction itself can't be redacted
        assert!(j.redact(&redaction.event_id).is_err());
    }

    #[test]
    fn passcode_latest_token_set_wins() {
        let dir = tempdir().unwrap();
        let j = Journal::init(&dir.path().join("j"), PW).unwrap();
        assert!(!j.check_passcode("anything").unwrap()); // none set yet
        j.set_passcode("first").unwrap();
        assert!(j.check_passcode("first").unwrap());
        j.set_passcode("second").unwrap();
        assert!(!j.check_passcode("first").unwrap());
        assert!(j.check_passcode("second").unwrap());
    }
}
