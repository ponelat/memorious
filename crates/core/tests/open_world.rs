//! The protocol is open-world: properties this build has never heard of (on the envelope or inside a known payload),
//! kinds and payload types it doesn't know, and enum values it doesn't know all survive storage, sync and export
//! unchanged, so an older client in the middle of a sync chain never strips what a newer one wrote.

use memorious_core::event::Event;
use memorious_core::node::Node;
use memorious_core::Journal;
use serde_json::{json, Value};
use tempfile::tempdir;

/// Events as a future client might write them, from a device nobody has seen.
fn future_events() -> Vec<Value> {
    let base = |seq: u64, kind: &str, payload: Value| {
        json!({
            "event_id": format!("ev-future-{seq}"), "device_id": "dev-from-the-future", "seq": seq,
            "recorded_at": 1_900_000_000_000i64 + seq as i64, "kind": kind, "payload": payload,
            "flags": { "pinned": true, "labels": ["a", "b"] },          // unknown ENVELOPE property
        })
    };
    vec![
        // a known payload with a property this build doesn't know
        base(1, "capture", json!({ "type": "text", "text": "hi", "mood": { "valence": 0.25, "tags": ["x"] } })),
        // a known payload with an enum value this build doesn't know (audio_kind was voice|music)
        base(2, "capture", json!({ "type": "audio", "hash": "a".repeat(64), "size": 3, "audio_kind": "podcast", "chapters": [1, 2] })),
        // a known payload that spells out a default (audio_kind: voice is normally omitted)
        base(3, "capture", json!({ "type": "audio", "hash": "b".repeat(64), "size": 4, "audio_kind": "voice" })),
        // a payload type this build doesn't know
        base(4, "capture", json!({ "type": "hologram", "mesh": [1, 2, 3] })),
        // a kind this build doesn't know
        base(5, "constellation", json!({ "type": "star", "magnitude": 2.5 })),
        // an infra_kind this build doesn't know, with a big integer and a float that must not be reformatted
        base(6, "infra", json!({ "type": "infra", "infra_kind": "brand_new", "target": "t", "data": { "n": 9007199254740993u64, "f": 1.5 } })),
    ]
}

#[tokio::test]
async fn unknown_properties_kinds_and_values_survive_storage() {
    let dir = tempdir().unwrap();
    let j = Journal::init(&dir.path().join("j"), "pw").unwrap();
    for v in future_events() {
        let e: Event = serde_json::from_value(v).unwrap();
        j.store_insert_remote_for_test(&e);
    }
    let stored: Vec<Value> = j.store_events_for_test().into_iter().filter(|e| e.device_id == "dev-from-the-future").map(|e| serde_json::to_value(&e).unwrap()).collect();
    assert_eq!(stored, future_events(), "what comes out of storage is exactly what went in");
}

#[tokio::test]
async fn unknown_properties_survive_a_sync_hop_through_an_old_peer() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    let jc = Journal::init_with_secret(&dir.path().join("c"), *ja.secret(), "pw").unwrap();
    for v in future_events() {
        ja.store_insert_remote_for_test(&serde_json::from_value::<Event>(v).unwrap());
    }
    let (a, b, c) = (Node::spawn(ja).await.unwrap(), Node::spawn(jb).await.unwrap(), Node::spawn(jc).await.unwrap());
    // A -> B (the "old" middle peer) -> C
    b.sync_events_with(&a.addr()).await.unwrap();
    c.sync_events_with(&b.addr()).await.unwrap();
    let theirs = |n: &Node| -> Vec<Value> {
        n.journal().store_events_for_test().into_iter().filter(|e| e.device_id == "dev-from-the-future").map(|e| serde_json::to_value(&e).unwrap()).collect()
    };
    assert_eq!(theirs(&c), future_events(), "C received exactly what the future device wrote, via B");
    for n in [a, b, c] {
        n.shutdown().await;
    }
}
