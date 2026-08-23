//! Retention end to end: a phone-like peer evicts transcribed voice audio,
//! the keep-everything peer holds it, sync does not pull it back, and
//! relaxing the policy makes the next sync restore it. Policy itself lives in
//! `crates/core/src/retention.rs` — this only exercises the plumbing.

use std::time::Duration;

use memorious_core::event::{AudioKind, MediaKind};
use memorious_core::node::Node;
use memorious_core::retention::{Retain, RetentionPolicy};
use memorious_core::Journal;
use tempfile::tempdir;

const GC_TICK: Duration = Duration::from_millis(150);

async fn wait_until(node: &Node, hash: &str, present: bool) {
    for _ in 0..60 {
        if node.has_blob(hash).await.unwrap() == present {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("blob {hash} never became present={present}");
}

#[tokio::test]
async fn phone_evicts_transcribed_voice_server_keeps_it_and_sync_restores_on_demand() {
    let dir = tempdir().unwrap();
    let jp = Journal::init(&dir.path().join("phone"), "pw").unwrap();
    let js = Journal::init_with_secret(&dir.path().join("server"), *jp.secret(), "pw").unwrap();
    let eager = RetentionPolicy {
        voice_audio: Retain::AfterDays(0),
        redacted: Retain::AfterDays(0),
    };
    jp.set_retention_policy(&eager).unwrap();

    let phone = Node::spawn_with_gc_interval(jp, GC_TICK).await.unwrap();
    let server = Node::spawn_with_gc_interval(js, GC_TICK).await.unwrap();

    let voice = phone
        .capture_audio(b"voice note bytes".to_vec(), AudioKind::Voice)
        .await
        .unwrap();
    let music = phone
        .capture_audio(b"a song".to_vec(), AudioKind::Music)
        .await
        .unwrap();
    let photo = phone
        .capture_blob(MediaKind::Photo, b"jpeg-ish".to_vec())
        .await
        .unwrap();
    let vh = voice.blob_hash().unwrap().to_string();
    let mh = music.blob_hash().unwrap().to_string();
    let ph = photo.blob_hash().unwrap().to_string();

    // Server pulls everything; server-side enrichment writes the transcript.
    server.sync_with(&phone.addr()).await.unwrap();
    assert!(server.has_blob(&vh).await.unwrap());
    server.journal().annotate(&voice.event_id, "dictated words").unwrap();

    // Nothing is evictable before the transcript arrives on the phone.
    tokio::time::sleep(GC_TICK * 3).await;
    assert!(phone.has_blob(&vh).await.unwrap(), "no transcript yet → keep");

    // Phone learns the transcript; GC then drops only the voice blob.
    phone.sync_with(&server.addr()).await.unwrap();
    wait_until(&phone, &vh, false).await;
    assert!(phone.has_blob(&mh).await.unwrap(), "music is the record");
    assert!(phone.has_blob(&ph).await.unwrap(), "photos are never evicted");
    assert!(server.has_blob(&vh).await.unwrap(), "keep-everything peer still has it");

    // Entry and transcript survive on the phone; only the bytes are gone.
    let annotations = phone.journal().annotations().unwrap();
    assert_eq!(annotations.get(&voice.event_id).unwrap(), "dictated words");
    assert!(phone.blob_bytes(&vh).await.is_err());

    // Sync must not pull the evicted blob back.
    let report = phone.sync_with(&server.addr()).await.unwrap();
    assert_eq!(report.blobs_fetched, 0);
    assert!(!phone.has_blob(&vh).await.unwrap());

    // Relaxing the policy restores it on the next sync — eviction was a cache.
    phone
        .journal()
        .set_retention_policy(&RetentionPolicy::KEEP_EVERYTHING)
        .unwrap();
    let report = phone.sync_with(&server.addr()).await.unwrap();
    assert_eq!(report.blobs_fetched, 1);
    assert_eq!(phone.blob_bytes(&vh).await.unwrap(), b"voice note bytes");
    tokio::time::sleep(GC_TICK * 3).await;
    assert!(phone.has_blob(&vh).await.unwrap(), "GC must respect the relaxed policy");
}

#[tokio::test]
async fn in_flight_captures_survive_gc() {
    // A blob ingested a moment before its event exists must not be swept.
    // The ingest tag is the only thing protecting it; retention may only drop
    // tags for hashes the log already references.
    let dir = tempdir().unwrap();
    let j = Journal::init(&dir.path().join("j"), "pw").unwrap();
    j.set_retention_policy(&RetentionPolicy::PHONE).unwrap();
    let node = Node::spawn_with_gc_interval(j, GC_TICK).await.unwrap();
    let e = node
        .capture_audio(b"voice".to_vec(), AudioKind::Voice)
        .await
        .unwrap();
    tokio::time::sleep(GC_TICK * 4).await;
    assert!(node.has_blob(e.blob_hash().unwrap()).await.unwrap());
}
