//! The canvas over HTTP: ink ops are appended per UTC month and read back in order, with an
//! after-cursor for cheap polling. Auth like everything under /api.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use memorious_core::{Journal, Node};
use memorious_server::{app, AppState};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn router() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::init(&dir.path().join("j"), "pw").unwrap();
    journal.set_passcode("sesame").unwrap();
    let node = Node::spawn(journal).await.unwrap();
    let state = Arc::new(AppState::new(node, None, dir.path().join("j")));
    let state = memorious_server::RouterState(Arc::new(std::sync::RwLock::new(state)));
    (dir, app(state, None))
}

async fn send(r: &axum::Router, method: &str, uri: &str, body: Option<Value>, auth: bool) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if auth {
        req = req.header(header::AUTHORIZATION, "Bearer sesame");
    }
    let req = match body {
        Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = r.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn stroke(n: u32) -> Value {
    json!({ "op": "stroke", "tool": "pencil", "seed": n, "size": 4.5, "pts": [[1, 2, 0.5, 0]] })
}

#[tokio::test]
async fn ink_needs_the_passcode() {
    let (_d, r) = router().await;
    assert_eq!(send(&r, "GET", "/api/ink/2026-11", None, false).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&r, "POST", "/api/ink/2026-11", Some(json!({"ops": [stroke(1)]})), false).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn append_a_batch_then_read_it_back_in_order() {
    let (_d, r) = router().await;
    let (st, body) = send(&r, "POST", "/api/ink/2026-11", Some(json!({"ops": [stroke(1), stroke(2), stroke(3)]})), true).await;
    assert_eq!(st, StatusCode::OK);
    let ids: Vec<String> = body["events"].as_array().unwrap().iter().map(|e| e["event_id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 3);

    let (st, body) = send(&r, "GET", "/api/ink/2026-11", None, true).await;
    assert_eq!(st, StatusCode::OK);
    let evs = body["events"].as_array().unwrap();
    assert_eq!(evs.iter().map(|e| e["event_id"].as_str().unwrap().to_string()).collect::<Vec<_>>(), ids);
    assert_eq!(evs[0]["data"]["op"], "stroke");
    assert_eq!(evs[1]["data"]["seed"], 2);
    assert!(body["device_id"].as_str().is_some_and(|d| !d.is_empty()), "the client needs its own device id to scope undo/redo");
    assert_eq!(evs[0]["device_id"], body["device_id"]);

    // cursor: ask again from where we were and get only what arrived since
    let cursor = body["cursor"].as_i64().unwrap();
    let (_, again) = send(&r, "GET", &format!("/api/ink/2026-11?after={cursor}"), None, true).await;
    assert!(again["events"].as_array().unwrap().is_empty());
    assert_eq!(again["cursor"], cursor, "nothing new: the cursor stays put");
    send(&r, "POST", "/api/ink/2026-11", Some(json!({"ops": [stroke(4)]})), true).await;
    let (_, more) = send(&r, "GET", &format!("/api/ink/2026-11?after={cursor}"), None, true).await;
    assert_eq!(more["events"].as_array().unwrap().len(), 1);
    assert_eq!(more["events"][0]["data"]["seed"], 4);
    assert!(more["cursor"].as_i64().unwrap() > cursor);
}

#[tokio::test]
async fn months_are_listed_and_bad_input_is_refused_whole() {
    let (_d, r) = router().await;
    send(&r, "POST", "/api/ink/2026-11", Some(json!({"ops": [stroke(1)]})), true).await;
    send(&r, "POST", "/api/ink/2026-09", Some(json!({"ops": [stroke(2)]})), true).await;
    let (_, body) = send(&r, "GET", "/api/ink", None, true).await;
    assert_eq!(body["months"], json!(["2026-09", "2026-11"]));

    assert_eq!(send(&r, "POST", "/api/ink/november", Some(json!({"ops": [stroke(1)]})), true).await.0, StatusCode::BAD_REQUEST);
    // one bad op in a batch: nothing is appended
    let (st, _) = send(&r, "POST", "/api/ink/2026-11", Some(json!({"ops": [stroke(9), {"no": "op"}]})), true).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (_, body) = send(&r, "GET", "/api/ink/2026-11", None, true).await;
    assert_eq!(body["events"].as_array().unwrap().len(), 1, "the half-good batch left no trace");
}
