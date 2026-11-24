//! Archive download links: build in the background, then a secret, expiring, resumable URL.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use memorious_core::event::MediaKind;
use memorious_core::{Journal, Node};
use memorious_server::{app, AppState};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn router() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::init(&dir.path().join("j"), "pw").unwrap();
    journal.set_passcode("sesame").unwrap();
    let node = Node::spawn(journal).await.unwrap();
    node.journal().capture_text("linked").unwrap();
    node.capture_blob(MediaKind::Photo, vec![3; 5000]).await.unwrap();
    let state = Arc::new(AppState::new(node, None, dir.path().join("j")));
    let state = memorious_server::RouterState(Arc::new(std::sync::RwLock::new(state)));
    (dir, app(state, None))
}

async fn call(r: &axum::Router, method: &str, uri: &str, body: Option<Value>, auth: bool, range: Option<&str>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(uri);
    if auth {
        req = req.header(header::AUTHORIZATION, "Bearer sesame");
    }
    if let Some(r) = range {
        req = req.header(header::RANGE, r);
    }
    let req = match body {
        Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = r.clone().oneshot(req).await.unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    (status, headers, resp.into_body().collect().await.unwrap().to_bytes().to_vec())
}

async fn make_ready(r: &axum::Router) -> Value {
    let (s, _, b) = call(r, "POST", "/api/exports", Some(json!({})), true, None).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let id = serde_json::from_slice::<Value>(&b).unwrap()["export"]["id"].as_str().unwrap().to_string();
    for _ in 0..200 {
        let (_, _, b) = call(r, "GET", "/api/exports", None, true, None).await;
        let list: Value = serde_json::from_slice(&b).unwrap();
        let e = list["exports"].as_array().unwrap().iter().find(|e| e["id"] == id.as_str()).unwrap().clone();
        match e["status"].as_str().unwrap() {
            "ready" => return e,
            "failed" => panic!("failed: {e}"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    panic!("never ready");
}

#[tokio::test]
async fn managing_exports_needs_the_passcode() {
    let (_d, r) = router().await;
    assert_eq!(call(&r, "GET", "/api/exports", None, false, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&r, "POST", "/api/exports", Some(json!({})), false, None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_link_works_without_a_passcode_and_resumes() {
    let (_d, r) = router().await;
    let e = make_ready(&r).await;
    let link = e["link"].as_str().unwrap().to_string();
    assert!(link.starts_with("/export/"), "{link}");

    // no Authorization header: the secret in the URL is the credential
    let (s, h, full) = call(&r, "GET", &link, None, false, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::CONTENT_TYPE], "application/zip");
    assert!(h[header::CONTENT_DISPOSITION].to_str().unwrap().contains("attachment"));
    assert_eq!(full.len() as u64, e["size"].as_u64().unwrap());
    assert!(zip::ZipArchive::new(std::io::Cursor::new(full.clone())).is_ok());

    // resumable: a Range request returns exactly that slice, as 206
    let (s, h, part) = call(&r, "GET", &link, None, false, Some("bytes=10-49")).await;
    assert_eq!(s, StatusCode::PARTIAL_CONTENT);
    assert_eq!(part, full[10..50]);
    assert!(h.contains_key(header::CONTENT_RANGE));
}

#[tokio::test]
async fn wrong_secret_unknown_id_and_revoked_are_all_404() {
    let (_d, r) = router().await;
    let e = make_ready(&r).await;
    let link = e["link"].as_str().unwrap().to_string();
    let id = e["id"].as_str().unwrap();
    let bad = link.replacen(link.split('/').nth(3).unwrap(), &"0".repeat(48), 1);
    assert_eq!(call(&r, "GET", &bad, None, false, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&r, "GET", "/export/ffff/ffff/x.zip", None, false, None).await.0, StatusCode::NOT_FOUND);

    assert_eq!(call(&r, "DELETE", &format!("/api/exports/{id}"), None, true, None).await.0, StatusCode::OK);
    assert_eq!(call(&r, "GET", &link, None, false, None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_expired_link_is_gone() {
    let (d, r) = router().await;
    let e = make_ready(&r).await;
    let link = e["link"].as_str().unwrap().to_string();
    let meta = d.path().join("j/exports").join(format!("{}.json", e["id"].as_str().unwrap()));
    let mut m: Value = serde_json::from_slice(&std::fs::read(&meta).unwrap()).unwrap();
    m["expires_ms"] = json!(1);
    std::fs::write(&meta, serde_json::to_vec(&m).unwrap()).unwrap();
    assert_eq!(call(&r, "GET", &link, None, false, None).await.0, StatusCode::NOT_FOUND);
    assert!(!d.path().join("j/exports").join(format!("{}.zip", e["id"].as_str().unwrap())).exists(), "expired files are swept");
}

#[tokio::test]
async fn only_the_newest_archives_are_kept() {
    let (_d, r) = router().await;
    let a = make_ready(&r).await;
    let b = make_ready(&r).await;
    let c = make_ready(&r).await;
    let (_, _, body) = call(&r, "GET", "/api/exports", None, true, None).await;
    let ids: Vec<String> = serde_json::from_slice::<Value>(&body).unwrap()["exports"].as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids.contains(&c["id"].as_str().unwrap().to_string()) && ids.contains(&b["id"].as_str().unwrap().to_string()));
    assert!(!ids.contains(&a["id"].as_str().unwrap().to_string()));
}
