//! One-shot migration of a pre-encryption journal to encryption at rest.
//!
//! Rebuilds the journal beside itself: same secret, device id, endpoint key,
//! event ids/seqs/timestamps; every media blob re-encrypted under a fresh
//! content key (so blob hashes — ciphertext identities — change and payloads
//! are rewritten). The old directory stays as `<dir>.pre-encryption`. Other
//! devices re-pair fresh afterwards; mixed old/new peers can't sync anyway
//! (the sync ALPN changed with this feature).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use iroh_blobs::api::Store as BlobStore;
use iroh_blobs::store::fs::FsStore;

use crate::event::Payload;
use crate::journal::Journal;
use crate::store::{Heads, Store};

#[derive(Debug, Default)]
pub struct MigrateReport {
    pub events: usize,
    pub media: usize,
    pub media_bytes: u64,
    pub backup_dir: PathBuf,
}

pub async fn migrate_encrypt(root: &Path, password: &str) -> Result<MigrateReport> {
    if root.join("keys.json").exists() {
        bail!("journal at {} is already encrypted", root.display());
    }
    if !root.join("db.sqlite").exists() {
        bail!("no journal at {}", root.display());
    }
    let staging = sibling(root, ".encrypting")?;
    let backup = sibling(root, ".pre-encryption")?;
    if staging.exists() || backup.exists() {
        bail!(
            "leftover {} or {} from an earlier attempt — remove it first",
            staging.display(),
            backup.display()
        );
    }

    let old_store = Store::open_plaintext(&root.join("db.sqlite"))?;
    let old_meta = old_store.meta_all()?;
    let secret_bytes = old_store
        .meta_get("journal_secret")?
        .context("journal missing secret")?;
    let secret: [u8; crate::journal::SECRET_LEN] = secret_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("malformed journal secret"))?;
    let old_fs = FsStore::load(root.join("blobs")).await?;
    let old_blobs: BlobStore = old_fs.clone().into();

    let mut report = MigrateReport::default();
    {
        let new_journal = Journal::init_with_secret(&staging, secret, password)?;
        // Identity travels wholesale: device id, endpoint secret, peer ticket…
        for (key, value) in &old_meta {
            new_journal.store.meta_set(key, value)?;
        }
        let new_fs = FsStore::load(staging.join("blobs")).await?;
        let new_blobs: BlobStore = new_fs.clone().into();

        // events_missing_from(∅) = everything, per device in seq order —
        // exactly what insert_remote needs to accept exact copies.
        for mut event in old_store.events_missing_from(&Heads::new())? {
            let media = match &event.payload {
                Payload::Photo { hash, size, .. } => {
                    Some((crate::event::MediaKind::Photo, hash.clone(), *size))
                }
                Payload::Audio { hash, size, .. } => {
                    Some((crate::event::MediaKind::Audio, hash.clone(), *size))
                }
                _ => None,
            };
            if let Some((kind, hash, size)) = media {
                let plaintext = old_blobs
                    .get_bytes(hash.parse::<iroh_blobs::Hash>().context("bad hash in log")?)
                    .await
                    .with_context(|| format!("read blob {hash} — is it fully synced here?"))?;
                report.media += 1;
                report.media_bytes += plaintext.len() as u64;
                let mut sealed = crate::crypto::seal(&plaintext)?;
                let envelope = new_journal.wrap_blob_keys(&sealed)?;
                let ciphertext = std::mem::take(&mut sealed.ciphertext);
                let tag = new_blobs.add_bytes(ciphertext).await?;
                event.payload = Payload::media(kind, tag.hash.to_hex().to_string(), size, envelope);
            }
            new_journal.store.insert_remote(&event)?;
            report.events += 1;
        }
        new_blobs.shutdown().await?;
    }
    old_blobs.shutdown().await?;
    drop(old_store);

    std::fs::rename(root, &backup).context("move old journal aside")?;
    std::fs::rename(&staging, root).context("move encrypted journal into place")?;
    report.backup_dir = backup;
    Ok(report)
}

fn sibling(root: &Path, suffix: &str) -> Result<PathBuf> {
    let name = root
        .file_name()
        .context("journal path has no directory name")?
        .to_string_lossy();
    Ok(root.with_file_name(format!("{name}{suffix}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventKind;
    use tempfile::tempdir;

    /// Build a pre-encryption journal the way old builds did: plaintext
    /// SQLite, plaintext blobs, no keys.json, no crypto in payloads.
    async fn legacy_journal(root: &Path, media: &[u8]) -> (String, String) {
        std::fs::create_dir_all(root.join("blobs")).unwrap();
        let store = Store::open_plaintext(&root.join("db.sqlite")).unwrap();
        let device_id = "dev-legacy".to_string();
        store.meta_set("device_id", device_id.as_bytes()).unwrap();
        store.meta_set("journal_secret", &[42u8; 32]).unwrap();
        store.meta_set("endpoint_secret", &[7u8; 32]).unwrap();
        let fs = FsStore::load(root.join("blobs")).await.unwrap();
        let blobs: BlobStore = fs.clone().into();
        let tag = blobs.add_bytes(media.to_vec()).await.unwrap();
        let hash = tag.hash.to_hex().to_string();
        store
            .append_local(
                &device_id,
                EventKind::Capture,
                Payload::Text { text: "kept as-is".into() },
                false,
            )
            .unwrap();
        let photo = store
            .append_local(
                &device_id,
                EventKind::Capture,
                Payload::Photo { hash: hash.clone(), size: media.len() as u64, crypto: None },
                false,
            )
            .unwrap();
        store
            .append_local(
                &device_id,
                EventKind::Annotation,
                Payload::Annotation { target: photo.event_id.clone(), text: "a kayak".into() },
                false,
            )
            .unwrap();
        blobs.shutdown().await.unwrap();
        (hash, photo.event_id)
    }

    #[tokio::test]
    async fn migrate_preserves_events_and_reencrypts_media() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("journal");
        let media = b"jpeg bytes, honest".to_vec();
        let (old_hash, photo_id) = legacy_journal(&root, &media).await;

        let report = migrate_encrypt(&root, "swordfish").await.unwrap();
        assert_eq!(report.events, 3);
        assert_eq!(report.media, 1);
        assert!(report.backup_dir.join("db.sqlite").exists());

        // The migrated journal opens with the password and kept its identity.
        let j = Journal::open(&root, "swordfish").unwrap();
        assert_eq!(j.device_id(), "dev-legacy");
        assert_eq!(j.secret(), &[42u8; 32]);
        assert_eq!(
            j.store.meta_get("endpoint_secret").unwrap().unwrap(),
            vec![7u8; 32]
        );

        // Same events, same ids/seqs; media payload rewritten.
        let events = j.store.all_events().unwrap();
        assert_eq!(events.len(), 3);
        let photo = j.store.get_event(&photo_id).unwrap().unwrap();
        let (new_hash, crypto) = match &photo.payload {
            Payload::Photo { hash, size, crypto } => {
                assert_eq!(*size, media.len() as u64);
                (hash.clone(), crypto.clone().expect("crypto envelope"))
            }
            other => panic!("expected photo payload, got {other:?}"),
        };
        assert_ne!(new_hash, old_hash, "identity is now the ciphertext hash");
        // FTS survived the rebuild.
        assert_eq!(j.store.search("kayak").unwrap().len(), 1);

        // And the media round-trips through a full node.
        let node = crate::Node::spawn(j).await.unwrap();
        let got = node.blob_bytes(&new_hash).await.unwrap();
        assert_eq!(got, media);
        // …but the blob on disk is ciphertext.
        let raw = node
            .journal()
            .store
            .capture_payload_for_hash(&new_hash)
            .unwrap();
        assert!(raw.is_some());
        node.shutdown().await;
        let _ = crypto;

        // Migrating twice is refused.
        assert!(migrate_encrypt(&root, "swordfish").await.is_err());
    }
}
