//! M5 acceptance: media arrives → sweeper annotates → searchable, no user action.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use memorious_core::event::MediaKind;
use memorious_core::{Journal, Node};
use memorious_server::sweeper::{sweep_once, Engines};
use memorious_server::{app, AppState};
use tower::ServiceExt;

struct MockEngines;

impl Engines for MockEngines {
    fn transcribe(&self, _m4a: &[u8]) -> anyhow::Result<String> {
        Ok("bought a kayak at the harbour".into())
    }
    fn ocr(&self, _jpeg: &[u8]) -> anyhow::Result<String> {
        Ok("SPECIALS: flat white 3.50".into())
    }
}

fn m4a_bytes() -> Vec<u8> {
    let mut b = vec![0, 0, 0, 24];
    b.extend_from_slice(b"ftypM4A ");
    b.extend_from_slice(&[3; 64]);
    b
}

#[tokio::test]
async fn media_from_a_peer_becomes_searchable_with_no_user_action() {
    // "Phone" peer captures audio offline (no will_enrich — it can't enrich).
    let dir = tempfile::tempdir().unwrap();
    let phone = Node::spawn(Journal::init(&dir.path().join("phone"), "pw").unwrap())
        .await
        .unwrap();
    phone
        .capture_blob(MediaKind::Audio, m4a_bytes())
        .await
        .unwrap();

    // Server peer joins the same journal and syncs the capture in.
    let server_journal =
        Journal::init_with_secret(&dir.path().join("server"), *phone.journal().secret(), "pw").unwrap();
    server_journal.set_passcode("sesame").unwrap();
    let state = Arc::new(AppState {
        node: Node::spawn(server_journal).await.unwrap(),
        downloads_dir: None,
    });
    state.node.sync_with(&phone.addr()).await.unwrap();

    // Sweeper runs (grace irrelevant: flag not set).
    let written = sweep_once(&state.node, &MockEngines, memorious_core::enrich::DEFAULT_GRACE_MS)
        .await
        .unwrap();
    assert_eq!(written, 1);

    // The transcript is searchable and attached to the entry in the feed.
    let router = app(state.clone(), None);
    let resp = router
        .clone()
        .oneshot(
            Request::get("/api/search?q=kayak")
                .header(header::AUTHORIZATION, "Bearer sesame")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let hits: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(hits["entries"].as_array().unwrap().len(), 1);
    assert_eq!(hits["entries"][0]["kind"], "audio");
    assert_eq!(hits["entries"][0]["annotation"], "bought a kayak at the harbour");

    let resp = router
        .clone()
        .oneshot(
            Request::get("/api/feed")
                .header(header::AUTHORIZATION, "Bearer sesame")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let feed: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(feed["entries"][0]["annotation"], "bought a kayak at the harbour");

    // Second sweep is a no-op (already annotated).
    let written = sweep_once(&state.node, &MockEngines, 0).await.unwrap();
    assert_eq!(written, 0);

    // The annotation syncs back to the phone; it converges on the same winner.
    phone.sync_with(&state.node.addr()).await.unwrap();
    let phone_annotations = phone.journal().annotations().unwrap();
    let server_annotations = state.node.journal().annotations().unwrap();
    assert_eq!(phone_annotations, server_annotations);
    assert_eq!(phone_annotations.len(), 1);

    phone.shutdown().await;
}

#[tokio::test]
async fn flagged_captures_wait_out_the_grace_period() {
    let dir = tempfile::tempdir().unwrap();
    let capturer = Node::spawn(Journal::init(&dir.path().join("capturer"), "pw").unwrap())
        .await
        .unwrap();
    // Captured with intent to enrich locally.
    capturer
        .capture_blob_with_intent(MediaKind::Audio, m4a_bytes(), true)
        .await
        .unwrap();

    let sweeper_journal =
        Journal::init_with_secret(&dir.path().join("sweeper"), *capturer.journal().secret(), "pw")
            .unwrap();
    let state = Arc::new(AppState {
        node: Node::spawn(sweeper_journal).await.unwrap(),
        downloads_dir: None,
    });
    state.node.sync_with(&capturer.addr()).await.unwrap();

    // Within the grace window: hands off.
    assert_eq!(
        sweep_once(&state.node, &MockEngines, 60_000).await.unwrap(),
        0
    );
    // Grace expired (zero grace): sweep it.
    assert_eq!(sweep_once(&state.node, &MockEngines, 0).await.unwrap(), 1);

    capturer.shutdown().await;
}
