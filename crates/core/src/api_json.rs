//! The JSON shape of an entry as the UIs consume it — shared by the HTTP server
//! and the Tauri command layer so the web adapter sees one format everywhere.

use serde_json::json;

use crate::event::{Event, Payload};

pub fn entry_json(e: &Event) -> serde_json::Value {
    let mut v = json!({
        "event_id": e.event_id,
        "device_id": e.device_id,
        "recorded_at": e.recorded_at,
    });
    let obj = v.as_object_mut().unwrap();
    match &e.payload {
        Payload::Text { text } => {
            obj.insert("kind".into(), "text".into());
            obj.insert("text".into(), text.clone().into());
        }
        Payload::Photo { hash, size } => {
            obj.insert("kind".into(), "photo".into());
            obj.insert(
                "media".into(),
                json!({"hash": hash, "size": size, "url": format!("/api/media/{hash}")}),
            );
        }
        Payload::Audio { hash, size } => {
            obj.insert("kind".into(), "audio".into());
            obj.insert(
                "media".into(),
                json!({"hash": hash, "size": size, "url": format!("/api/media/{hash}")}),
            );
        }
        other => {
            obj.insert("kind".into(), "other".into());
            obj.insert("detail".into(), format!("{other:?}").into());
        }
    }
    v
}
