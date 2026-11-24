//! The lossless archive: events as JSON that validate against the shipped schema, decrypted media in plain formats,
//! redacted bytes left out, and a manifest whose checksums hold.

use memorious_core::event::{AudioKind, MediaKind};
use memorious_core::export_archive::{export_archive, export_archive_dir, ArchiveOpts, DirSink, EventsEncoding, ZipSink, EVENT_SCHEMA, MANIFEST_SCHEMA};
use memorious_core::node::Node;
use memorious_core::Journal;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

async fn node(dir: &std::path::Path) -> Node {
    Node::spawn(Journal::init(&dir.join("j"), "pw").unwrap()).await.unwrap()
}

fn sha(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}

#[tokio::test]
async fn the_archive_is_valid_complete_and_checksummed() {
    let dir = tempdir().unwrap();
    let node = node(dir.path()).await;
    let j = node.journal();

    let text = j.capture_text("hello\nworld").unwrap();
    let photo_bytes = vec![1u8, 2, 3, 4, 5];
    let photo = node.capture_blob(MediaKind::Photo, photo_bytes.clone()).await.unwrap();
    let voice = node.capture_audio(vec![9, 9, 9], AudioKind::Voice).await.unwrap();
    j.annotate(&voice.event_id, "a transcript").unwrap();
    let secret = node.capture_blob(MediaKind::Photo, vec![7, 7, 7]).await.unwrap();
    j.redact(&secret.event_id).unwrap();
    j.set_device_name(j.device_id(), "test box").unwrap();
    j.append_ink("2026-11", json!({ "op": "stroke", "tool": "pen", "seed": 1, "size": 5, "cid": "c1", "pts": [[0, 0, 0.5, 0], [10, 10, 0.5, 0]] })).unwrap();
    j.append_ink("2026-11", json!({ "op": "undo", "target": "c1", "cid": "c2" })).unwrap();
    j.ensure_version_seen().unwrap();
    j.ensure_peer_join().unwrap();

    let out = dir.path().join("archive");
    let report = export_archive_dir(&node, &out, ArchiveOpts::default(), 1_800_000_000_000).await.unwrap();
    assert_eq!(report.media_included, 2);
    assert_eq!(report.media_redacted, 1);

    // every line validates against the schema we ship
    let schema: Value = serde_json::from_str(EVENT_SCHEMA).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let raw = std::fs::read_to_string(out.join("events.jsonl")).unwrap();
    let lines: Vec<Value> = raw.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), report.events);
    for l in &lines {
        let errs: Vec<String> = validator.iter_errors(l).map(|e| e.to_string()).collect();
        assert!(errs.is_empty(), "{l} -> {errs:?}");
    }
    // every kind we wrote is there, including infra and the redact itself (the log is lossless)
    let kinds: std::collections::BTreeSet<&str> = lines.iter().map(|l| l["kind"].as_str().unwrap()).collect();
    for k in ["capture", "redact", "annotation", "infra"] {
        assert!(kinds.contains(k), "missing {k}");
    }
    assert!(lines.iter().any(|l| l["payload"]["infra_kind"] == "ink" && l["payload"]["data"]["op"] == "undo"));
    assert!(lines.iter().any(|l| l["event_id"] == text.event_id.as_str() && l["payload"]["text"] == "hello\nworld"));

    // manifest validates, its checksums hold, media is decrypted and byte-identical
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(out.join("manifest.json")).unwrap()).unwrap();
    let mv = jsonschema::validator_for(&serde_json::from_str::<Value>(MANIFEST_SCHEMA).unwrap()).unwrap();
    let errs: Vec<String> = mv.iter_errors(&manifest).map(|e| e.to_string()).collect();
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(manifest["events"]["sha256"], sha(raw.as_bytes()));
    assert_eq!(manifest["events"]["count"], lines.len());
    let ph = photo.blob_hash().unwrap();
    assert_eq!(std::fs::read(out.join(format!("media/{ph}.jpg"))).unwrap(), photo_bytes);
    let vh = voice.blob_hash().unwrap();
    assert_eq!(std::fs::read(out.join(format!("media/{vh}.m4a"))).unwrap(), vec![9, 9, 9]);
    let sh = secret.blob_hash().unwrap();
    assert!(!out.join(format!("media/{sh}.jpg")).exists(), "a redacted capture's bytes stay out");
    let entry = manifest["media"].as_array().unwrap().iter().find(|m| m["event_id"] == secret.event_id.as_str()).unwrap();
    assert_eq!(entry["status"], "redacted");
    let entry = manifest["media"].as_array().unwrap().iter().find(|m| m["event_id"] == photo.event_id.as_str()).unwrap();
    assert_eq!(entry["sha256"], sha(&photo_bytes));

    for f in ["README.md", "schema/event.schema.json", "schema/manifest.schema.json"] {
        assert!(out.join(f).exists(), "{f}");
    }
    node.shutdown().await;
}

#[tokio::test]
async fn json_array_and_no_media_variants() {
    let dir = tempdir().unwrap();
    let node = node(dir.path()).await;
    node.journal().capture_text("one").unwrap();
    node.capture_blob(MediaKind::Photo, vec![1, 2]).await.unwrap();

    let out = dir.path().join("a");
    let opts = ArchiveOpts { events: EventsEncoding::Json, media: false };
    let report = export_archive_dir(&node, &out, opts, 1).await.unwrap();
    let arr: Value = serde_json::from_str(&std::fs::read_to_string(out.join("events.json")).unwrap()).unwrap();
    assert_eq!(arr.as_array().unwrap().len(), report.events);
    assert!(!out.join("media").exists());
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["media"][0]["status"], "unavailable");
    node.shutdown().await;
}

/// An event this schema has never heard of must still export, byte for byte, and still validate (open world).
#[tokio::test]
async fn unknown_kinds_and_extra_props_pass_through() {
    let dir = tempdir().unwrap();
    let node = node(dir.path()).await;
    let j = node.journal();
    j.append_infra("brand_new_fact", Some("x"), json!({ "anything": [1, 2, 3], "nested": { "ok": true } })).unwrap();

    let out = dir.path().join("a");
    export_archive_dir(&node, &out, ArchiveOpts::default(), 1).await.unwrap();
    let raw = std::fs::read_to_string(out.join("events.jsonl")).unwrap();
    let ev: Value = raw.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).find(|v| v["payload"]["infra_kind"] == "brand_new_fact").unwrap();
    assert_eq!(ev["payload"]["data"]["nested"]["ok"], true);
    let schema: Value = serde_json::from_str(EVENT_SCHEMA).unwrap();
    assert!(jsonschema::validator_for(&schema).unwrap().is_valid(&ev));
    node.shutdown().await;
}

#[tokio::test]
async fn the_zip_has_the_same_files() {
    let dir = tempdir().unwrap();
    let node = node(dir.path()).await;
    node.journal().capture_text("zipped").unwrap();
    node.capture_blob(MediaKind::Photo, vec![5; 100]).await.unwrap();

    let file = std::fs::File::create(dir.path().join("a.zip")).unwrap();
    let mut sink = ZipSink::new(file, "memorious-archive-test");
    export_archive(&node, &mut sink, ArchiveOpts::default(), 1).await.unwrap();
    sink.finish().unwrap();

    let mut z = zip::ZipArchive::new(std::fs::File::open(dir.path().join("a.zip")).unwrap()).unwrap();
    let names: Vec<String> = z.file_names().map(|n| n.unwrap().to_string()).collect();
    for want in ["README.md", "manifest.json", "events.jsonl", "schema/event.schema.json"] {
        assert!(names.contains(&format!("memorious-archive-test/{want}")), "{want} in {names:?}");
    }
    assert!(names.iter().any(|n| n.contains("/media/") && n.ends_with(".jpg")));
    node.shutdown().await;
    let _ = DirSink::new; // both sinks stay public API
}
