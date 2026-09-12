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

/// Device ids look like `dev-<uuid>`; annotation targets with this prefix are
/// device names, everything else is enrichment on an event.
const DEVICE_ID_PREFIX: &str = "dev-";
const DEVICE_NAME_MAX: usize = 64;
/// Annotation target carrying the master-password proof (see `write_password_proof`).
pub const PASSWORD_PROOF_TARGET: &str = "journal:password-proof";

/// Span of the visible timeline (non-redacted captures).
#[derive(Debug, Clone, serde::Serialize)]
pub struct TimelineStats {
    pub entries: usize,
    /// Unix ms of the oldest / newest visible entry; `None` when empty.
    pub first_recorded_at: Option<i64>,
    pub last_recorded_at: Option<i64>,
}

/// Bytes on disk under this journal's root.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StorageUsage {
    /// The SQLCipher database (incl. WAL/shm sidecars).
    pub db_bytes: u64,
    /// The iroh-blobs store (sealed media ciphertext).
    pub blobs_bytes: u64,
}

/// Traffic-light summary of replication state; see [`Journal::sync_health`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncHealth {
    pub color: String, // "green" | "yellow" | "red"
    pub pending: bool,
    pub stalest_ms: Option<i64>,
    pub peers: usize,
    /// Known peers whose last-seen holdings fall short of ours: behind on
    /// events, or missing media their own retention policy does not excuse.
    pub peers_behind: usize,
}

/// What a device holds of the journal's media, as it reports itself in the
/// sync handshake (and as we remember it for each peer). Counts are over the
/// blobs the event log references; `evictable` is the slice its retention
/// policy currently lets go, so `held + evictable >= referenced` means
/// "complete, by its own rules". `policy` explains the gap.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaHeld {
    pub referenced: u64,
    pub held: u64,
    pub evictable: u64,
    /// Bytes in the blob store (everything on disk, not just referenced).
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<crate::retention::RetentionPolicy>,
}

impl MediaHeld {
    /// Referenced blobs neither held nor excused by policy.
    pub fn missing(&self) -> u64 {
        self.referenced
            .saturating_sub(self.held)
            .saturating_sub(self.evictable)
    }
    pub fn complete(&self) -> bool {
        self.missing() == 0
    }
}

/// A peer's holdings as of our last completed handshake with it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerHoldings {
    pub heads: Option<crate::store::Heads>,
    pub media: Option<MediaHeld>,
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
        let journal = Self::init_with_secret(root, secret, password)?;
        // The creator knows the password by definition: publish the proof
        // every later device must pass (see `prove_password`).
        journal.write_password_proof()?;
        Ok(journal)
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

    /// Remove what [`Self::init_with_secret`] just created, after a pairing
    /// that failed before the journal was worth keeping. Only the files init
    /// writes (database + sidecars, keys.json, the empty blob store) — never
    /// anything else that may live under `root`. Best effort, errors ignored:
    /// the caller is already returning the real error.
    pub fn discard_created(root: &Path) {
        for name in ["db.sqlite", "db.sqlite-wal", "db.sqlite-shm", "db.sqlite-journal", KEYS_FILE] {
            let _ = std::fs::remove_file(root.join(name));
        }
        let _ = std::fs::remove_dir_all(root.join("blobs"));
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
        let journal = Self {
            store,
            root: root.to_path_buf(),
            device_id,
            secret,
            keys,
        };
        journal.ensure_password_proof()?;
        Ok(journal)
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

    // ---- master password proof ----
    //
    // The pairing ticket authorizes replication of the event log; the master
    // password authorizes reading media (per-blob keys are wrapped under a
    // password-derived key). Until 2026-09-12 the only cross-device password
    // check was unwrapping a media key, so a journal with no media yet
    // accepted ANY password on join — and the joiner's own captures were then
    // wrapped under keys no other device could open. The proof is a throwaway
    // content key wrapped exactly like a media key, published as an annotation
    // on a reserved target (replicates like device names, older peers ignore
    // it). Anyone holding the right password unwraps it; nobody else can.

    /// Publish the password proof (creator, or a device that has proven the
    /// password some other way). Latest annotation wins, so re-publishing is
    /// harmless.
    pub fn write_password_proof(&self) -> Result<Event> {
        let mut ck = [0u8; crypto::KEY_LEN];
        rand::rngs::OsRng.try_fill_bytes(&mut ck)?;
        let mut nonce_base = [0u8; crypto::NONCE_BASE_LEN];
        rand::rngs::OsRng.try_fill_bytes(&mut nonce_base)?;
        let proof = self.keys.wrap(&ck, &nonce_base)?;
        self.annotate(PASSWORD_PROOF_TARGET, &serde_json::to_string(&proof)?)
    }

    /// Every proof annotation in the log, latest first.
    fn password_proofs(&self) -> Result<Vec<Event>> {
        let mut proofs: Vec<Event> = self
            .store
            .all_events()?
            .into_iter()
            .filter(|e| matches!(&e.payload, Payload::Annotation { target, .. } if target == PASSWORD_PROOF_TARGET))
            .collect();
        proofs.sort_by(|a, b| (b.recorded_at, &b.event_id).cmp(&(a.recorded_at, &a.event_id)));
        Ok(proofs)
    }

    fn parse_proof(e: &Event) -> Result<BlobCrypto> {
        match &e.payload {
            Payload::Annotation { text, .. } => serde_json::from_str(text).context("password proof"),
            _ => bail!("not a proof event"),
        }
    }

    /// The journal's published proof (any device's, latest), if one exists.
    pub fn password_proof(&self) -> Result<Option<BlobCrypto>> {
        self.password_proofs()?.first().map(Self::parse_proof).transpose()
    }

    /// Prove this device's master password against what OTHER devices wrote:
    /// their latest proof, else a media key one of them wrapped. Our own
    /// proof or captures prove nothing (a device that joined with the wrong
    /// password would happily vouch for itself). `Ok(true)` = proven,
    /// `Ok(false)` = nothing to check against (a legacy journal with no
    /// media, seen only from a joiner), `Err` = mismatch.
    pub fn prove_password(&self) -> Result<bool> {
        if let Some(proof) = self
            .password_proofs()?
            .iter()
            .find(|e| e.device_id != self.device_id)
        {
            self.keys
                .unwrap(&Self::parse_proof(proof)?)
                .context("master password doesn't match this journal")?;
            return Ok(true);
        }
        for ev in self.store.all_events()? {
            if ev.device_id == self.device_id {
                continue;
            }
            if let Some(crypto) = ev.payload.blob_crypto() {
                self.keys
                    .unwrap(crypto)
                    .context("master password doesn't match this journal")?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// On open: fail if the password is provably wrong; publish the proof if
    /// none exists and this device can vouch for it (it created the journal,
    /// or it unwrapped another device's media key). A joiner of a legacy
    /// media-less journal can do neither and leaves it to the creator.
    fn ensure_password_proof(&self) -> Result<()> {
        let proven = self.prove_password()?;
        if self.password_proof()?.is_some() {
            return Ok(());
        }
        let creator = self
            .store
            .all_events()?
            .into_iter()
            .min_by(|a, b| (a.recorded_at, &a.event_id).cmp(&(b.recorded_at, &b.event_id)))
            .map(|first| first.device_id == self.device_id)
            .unwrap_or(true);
        if proven || creator {
            self.write_password_proof()?;
        }
        Ok(())
    }

    // ---- sync health ----

    /// A completed event sync with `peer` (either direction): remember when,
    /// and snapshot our heads so "pending to push" is detectable later.
    /// `device` maps the peer's endpoint id to its journal device id when the
    /// peer told us; `discovery` records how the peer entered our world
    /// ("ticket"/"inbound") — set at first contact, never rewritten.
    pub fn record_sync_contact(
        &self,
        peer: &str,
        now_ms: i64,
        device: Option<&str>,
        discovery: &str,
    ) -> Result<()> {
        self.store
            .meta_set(&format!("peer_last_ok:{peer}"), &now_ms.to_le_bytes())?;
        if let Some(device) = device {
            self.store
                .meta_set(&format!("peer_device:{peer}"), device.as_bytes())?;
        }
        if self.store.meta_get(&format!("peer_discovery:{peer}"))?.is_none() {
            self.store
                .meta_set(&format!("peer_discovery:{peer}"), discovery.as_bytes())?;
        }
        let heads = self.store.heads()?;
        self.store
            .meta_set("last_sync_heads", &serde_json::to_vec(&heads)?)?;
        Ok(())
    }

    /// Remember what a peer holds, as of a completed handshake: the heads
    /// both sides converged on (events sync both ways, so ours after the
    /// exchange are exactly the peer's) and the media tally it reported in
    /// its Hello/HelloAck (taken before that sync's blob fetch — the next
    /// handshake corrects it). This is the version-vector "ack": what the
    /// peer has, per device, not merely when we last heard from it.
    pub fn record_peer_holdings(
        &self,
        peer: &str,
        heads: &crate::store::Heads,
        media: Option<&MediaHeld>,
    ) -> Result<()> {
        self.store
            .meta_set(&format!("peer_heads:{peer}"), &serde_json::to_vec(heads)?)?;
        if let Some(media) = media {
            self.store
                .meta_set(&format!("peer_media:{peer}"), &serde_json::to_vec(media)?)?;
        }
        Ok(())
    }

    pub fn peer_holdings(&self, peer: &str) -> Result<PeerHoldings> {
        let heads = match self.store.meta_get(&format!("peer_heads:{peer}"))? {
            Some(bytes) => serde_json::from_slice(&bytes).ok(),
            None => None,
        };
        let media = match self.store.meta_get(&format!("peer_media:{peer}"))? {
            Some(bytes) => serde_json::from_slice(&bytes).ok(),
            None => None,
        };
        Ok(PeerHoldings { heads, media })
    }

    /// Events a peer lacks, judged by its last-seen heads against ours: the
    /// sum over devices of `ours - theirs`. `None` when we have never
    /// recorded the peer's heads (synced before this build).
    pub fn peer_events_missing(&self, peer_heads: &crate::store::Heads) -> Result<u64> {
        let mut missing = 0u64;
        for (device, ours) in self.store.heads()? {
            let theirs = peer_heads.get(&device).copied().unwrap_or(0);
            missing += ours.saturating_sub(theirs);
        }
        Ok(missing)
    }

    /// Sum of heads: how many events the log holds across all devices.
    pub fn events_total(&self) -> Result<u64> {
        Ok(self.store.heads()?.values().sum())
    }

    /// Traffic light for the sync status UX (UNDERSTANDING.md): red = a known
    /// peer unheard-from for 48h (outranks all), yellow = local data no peer
    /// has picked up yet OR a known peer last seen holding less than we do
    /// (events, or media its retention policy doesn't excuse), green =
    /// every peer holds everything (or solo — nowhere to push).
    pub fn sync_health(&self, now_ms: i64) -> Result<SyncHealth> {
        const STALE_MS: i64 = 48 * 3600 * 1000;
        let peers = self.store.meta_scan("peer_last_ok:")?;
        if peers.is_empty() {
            return Ok(SyncHealth { color: "green".into(), pending: false, stalest_ms: None, peers: 0, peers_behind: 0 });
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
        // A peer counts as behind when its last-seen holdings fall short of
        // ours: events (version vector) or media beyond what its own retention
        // policy excuses. Peers we never recorded holdings for don't count —
        // there is nothing to judge them by.
        let mut peers_behind = 0;
        for (key, _) in &peers {
            let peer = key.trim_start_matches("peer_last_ok:");
            let holdings = self.peer_holdings(peer)?;
            let events_behind = match &holdings.heads {
                Some(h) => self.peer_events_missing(h)? > 0,
                None => false,
            };
            let media_behind = holdings.media.as_ref().is_some_and(|m| !m.complete());
            if events_behind || media_behind {
                peers_behind += 1;
            }
        }
        let color = if now_ms - stalest > STALE_MS {
            "red"
        } else if pending || peers_behind > 0 {
            "yellow"
        } else {
            "green"
        };
        Ok(SyncHealth {
            color: color.into(),
            pending,
            stalest_ms: Some(stalest),
            peers: peers.len(),
            peers_behind,
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

    // ---- device names ----
    // A name is an annotation event whose target is a *device id* instead of
    // an event id (the `dev-` prefix keeps the namespaces apart). It syncs and
    // resolves latest-wins exactly like enrichment annotations, so any device
    // can rename any other and every peer converges on the same names.

    /// Name a device (this one or any other). Editable: latest wins.
    pub fn set_device_name(&self, device_id: &str, name: &str) -> Result<Event> {
        if !device_id.starts_with(DEVICE_ID_PREFIX) {
            bail!("not a device id: {device_id}");
        }
        let name = name.trim();
        if name.is_empty() {
            bail!("device name is empty");
        }
        if name.chars().count() > DEVICE_NAME_MAX {
            bail!("device name too long ({DEVICE_NAME_MAX} chars max)");
        }
        self.annotate(device_id, name)
    }

    /// Friendly name per device id (latest annotation wins).
    pub fn device_names(&self) -> Result<std::collections::HashMap<String, String>> {
        Ok(self
            .annotations()?
            .into_iter()
            .filter(|(target, _)| target.starts_with(DEVICE_ID_PREFIX))
            .collect())
    }

    /// First-run default: name this device `default` unless it has a name
    /// already (from an earlier run or set by a peer).
    pub fn ensure_device_name(&self, default: &str) -> Result<()> {
        if !self.device_names()?.contains_key(self.device_id()) {
            let device_id = self.device_id().to_string();
            self.set_device_name(&device_id, default)?;
        }
        Ok(())
    }

    // ---- journal stats ----

    /// Entry count and time span of the visible timeline.
    pub fn timeline(&self) -> Result<TimelineStats> {
        let list = self.list()?; // (recorded_at, event_id) order
        Ok(TimelineStats {
            entries: list.len(),
            first_recorded_at: list.first().map(|e| e.recorded_at),
            last_recorded_at: list.last().map(|e| e.recorded_at),
        })
    }

    /// Disk footprint of the database and blob store.
    pub fn storage_usage(&self) -> Result<StorageUsage> {
        let mut db_bytes = 0;
        for suffix in ["", "-wal", "-shm"] {
            let path = self.root.join(format!("db.sqlite{suffix}"));
            if let Ok(meta) = std::fs::metadata(&path) {
                db_bytes += meta.len();
            }
        }
        let mut blobs_bytes = 0;
        let mut stack = vec![self.blobs_dir()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    stack.push(entry.path());
                } else {
                    blobs_bytes += meta.len();
                }
            }
        }
        Ok(StorageUsage { db_bytes, blobs_bytes })
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

    /// Check a passcode against the latest token-set event. The comparison
    /// is constant-time (`blake3::Hash`'s equality), so a wrong guess costs
    /// the same however many leading characters it gets right.
    pub fn check_passcode(&self, passcode: &str) -> Result<bool> {
        let Some(active) = self.active_passcode_hash()? else {
            return Ok(false); // no passcode set: browser access denied
        };
        let Ok(active) = blake3::Hash::from_hex(&active) else {
            return Ok(false);
        };
        Ok(blake3::hash(passcode.as_bytes()) == active)
    }

    /// Latest wins: recorded_at, then device_id, then seq (same-device
    /// same-ms sets) — one indexed query in the store, never a log scan.
    pub fn active_passcode_hash(&self) -> Result<Option<String>> {
        Ok(self.store.latest_token_set()?.and_then(|e| match e.payload {
            Payload::TokenSet { hash } => Some(hash),
            _ => None,
        }))
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
    fn device_names_default_then_edit_latest_wins() {
        let dir = tempdir().unwrap();
        let j = Journal::init(&dir.path().join("j"), PW).unwrap();
        assert!(j.device_names().unwrap().is_empty());

        // First-run default sticks; a second ensure never overwrites.
        j.ensure_device_name("desktop (macOS)").unwrap();
        j.ensure_device_name("web").unwrap();
        let me = j.device_id().to_string();
        assert_eq!(j.device_names().unwrap().get(&me).map(String::as_str), Some("desktop (macOS)"));

        // An explicit rename wins (latest annotation).
        j.set_device_name(&me, "study mac").unwrap();
        assert_eq!(j.device_names().unwrap().get(&me).map(String::as_str), Some("study mac"));

        // Any device can (re)name any other device.
        j.set_device_name("dev-feedbeef", "old phone").unwrap();
        assert_eq!(
            j.device_names().unwrap().get("dev-feedbeef").map(String::as_str),
            Some("old phone")
        );

        // Guard rails: device ids only, no empty or silly-long names.
        assert!(j.set_device_name("not-a-device", "x").is_err());
        assert!(j.set_device_name(&me, "   ").is_err());
        assert!(j.set_device_name(&me, &"x".repeat(65)).is_err());

        // Naming never leaks into the entry timeline.
        assert_eq!(j.list().unwrap().len(), 0);
    }

    #[test]
    fn timeline_and_storage_stats() {
        let dir = tempdir().unwrap();
        let j = Journal::init(&dir.path().join("j"), PW).unwrap();
        let t = j.timeline().unwrap();
        assert_eq!(t.entries, 0);
        assert_eq!(t.first_recorded_at, None);

        let first = j.capture_text("first").unwrap();
        let last = j.capture_text("last").unwrap();
        let toss = j.capture_text("toss").unwrap();
        j.redact(&toss.event_id).unwrap();

        let t = j.timeline().unwrap();
        assert_eq!(t.entries, 2);
        assert_eq!(t.first_recorded_at, Some(first.recorded_at));
        assert_eq!(t.last_recorded_at, Some(last.recorded_at));

        let s = j.storage_usage().unwrap();
        assert!(s.db_bytes > 0, "SQLite file must count: {s:?}");
        assert_eq!(s.blobs_bytes, 0, "no media captured yet: {s:?}");
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
