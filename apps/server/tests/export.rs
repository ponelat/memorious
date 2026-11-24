//! `GET /api/export`: the lossless archive as a zip, behind the same passcode as everything else.

use std::io::{Cursor, Read};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use memorious_core::event::MediaKind;
use memorious_core::{Journal, Node};
use memorious_server::{app, AppState};
use tower::ServiceExt;

async fn router() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::init(&dir.path().join("j"), "pw").unwrap();
    journal.set_passcode("sesame").unwrap();
    let node = Node::spawn(journal).await.unwrap();
    node.journal().capture_text("in the archive").unwrap();
    node.capture_blob(MediaKind::Photo, vec![1, 2, 3, 4]).await.unwrap();
    let state = Arc::new(AppState::new(node, None, dir.path().join("j")));
    let state = memorious_server::RouterState(Arc::new(std::sync::RwLock::new(state)));
    (dir, app(state, None))
}

async fn get(r: &axum::Router, uri: &str, auth: bool) -> (StatusCode, Vec<(String, String)>, Vec<u8>) {
    let mut req = Request::builder().uri(uri);
    if auth {
        req = req.header(header::AUTHORIZATION, "Bearer sesame");
    }
    let resp = r.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string())).collect();
    (status, headers, resp.into_body().collect().await.unwrap().to_bytes().to_vec())
}

#[tokio::test]
async fn export_needs_the_passcode() {
    let (_d, r) = router().await;
    assert_eq!(get(&r, "/api/export", false).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn export_is_a_zip_with_events_schema_and_media() {
    let (_d, r) = router().await;
    let (status, headers, body) = get(&r, "/api/export", true).await;
    assert_eq!(status, StatusCode::OK);
    let h = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
    assert_eq!(h("content-type"), "application/zip");
    assert!(h("content-disposition").contains("memorious-archive-") && h("content-disposition").ends_with(".zip\""));

    let mut z = zip::ZipArchive::new(Cursor::new(body)).unwrap();
    let names: Vec<String> = z.file_names().map(|n| n.unwrap().to_string()).collect();
    let find = |suffix: &str| names.iter().find(|n| n.ends_with(suffix)).cloned().unwrap_or_else(|| panic!("{suffix} in {names:?}"));
    let mut events = String::new();
    z.by_name(&find("/events.jsonl")).unwrap().read_to_string(&mut events).unwrap();
    assert!(events.contains("in the archive"));
    find("/schema/event.schema.json");
    find("/manifest.json");
    assert!(names.iter().any(|n| n.contains("/media/") && n.ends_with(".jpg")));
}

#[tokio::test]
async fn export_options_and_bad_format() {
    let (_d, r) = router().await;
    let (status, _, body) = get(&r, "/api/export?format=json&media=0", true).await;
    assert_eq!(status, StatusCode::OK);
    let z = zip::ZipArchive::new(Cursor::new(body)).unwrap();
    let names: Vec<String> = z.file_names().map(|n| n.unwrap().to_string()).collect();
    assert!(names.iter().any(|n| n.ends_with("/events.json")));
    assert!(!names.iter().any(|n| n.contains("/media/")));
    assert_eq!(get(&r, "/api/export?format=xml", true).await.0, StatusCode::BAD_REQUEST);
}
