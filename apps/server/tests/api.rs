//! M2 server acceptance: capture all three types over HTTP, auth end to end,
//! and convergence with a plain core peer over iroh.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use memorious_core::{Journal, Node};
use memorious_server::{app, AppState};
use tower::ServiceExt;

async fn test_state() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::init(&dir.path().join("j"), "pw").unwrap();
    journal.set_passcode("sesame").unwrap();
    let node = Node::spawn(journal).await.unwrap();
    (dir, Arc::new(AppState { node, downloads_dir: None }))
}

fn authed(req: axum::http::request::Builder) -> axum::http::request::Builder {
    req.header(header::AUTHORIZATION, "Bearer sesame")
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn multipart_body(field_bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = "testboundary42";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"upload.bin\"\r\n\r\n",
    );
    body.extend_from_slice(field_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[tokio::test]
async fn auth_gates_everything() {
    let (_d, state) = test_state().await;
    let router = app(state, None);

    // No token → 401.
    let resp = router
        .clone()
        .oneshot(Request::get("/api/feed").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Wrong passcode check → 401; right one → 204.
    let resp = router
        .clone()
        .oneshot(
            Request::post("/api/auth/check")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"passcode":"nope"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = router
        .clone()
        .oneshot(
            Request::post("/api/auth/check")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"passcode":"sesame"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn status_carries_stats_names_and_net_config() {
    let (_d, state) = test_state().await;
    state.node.journal().ensure_device_name("web").unwrap();
    state.node.journal().capture_text("hello").unwrap();
    let router = app(state.clone(), None);

    let resp = router
        .clone()
        .oneshot(authed(Request::get("/api/status")).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let me = state.node.journal().device_id();
    assert_eq!(v["entries"], 1);
    assert_eq!(v["names"][me], "web");
    assert!(v["storage"]["db_bytes"].as_u64().unwrap() > 0);
    assert!(v["timeline"]["first_recorded_at"].is_i64());
    assert!(v["peers"].is_array());
    assert_eq!(v["net"]["relay_mode"], "default");
    assert_eq!(v["net"]["public_lookup"], true);

    // Rename this device through the API; the new name shows up in status.
    let resp = router
        .clone()
        .oneshot(
            authed(Request::post("/api/device-name"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"device_id":"{me}","name":"living room server"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = router
        .clone()
        .oneshot(authed(Request::get("/api/status")).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let v = body_json(resp).await;
    assert_eq!(v["names"][me], "living room server");

    // Bad names are refused.
    let resp = router
        .clone()
        .oneshot(
            authed(Request::post("/api/device-name"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"device_id":"{me}","name":"  "}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Net config: reject garbage, accept a real one, see it in status.
    let resp = router
        .clone()
        .oneshot(
            authed(Request::post("/api/net-config"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"relay_mode":"custom","relay_urls":[],"public_lookup":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = router
        .clone()
        .oneshot(
            authed(Request::post("/api/net-config"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"relay_mode":"disabled","relay_urls":[],"public_lookup":false}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = router
        .clone()
        .oneshot(authed(Request::get("/api/status")).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let v = body_json(resp).await;
    assert_eq!(v["net"]["relay_mode"], "disabled");
    assert_eq!(v["net"]["public_lookup"], false);
}

#[tokio::test]
async fn capture_all_three_types_and_read_back() {
    let (_d, state) = test_state().await;
    let router = app(state.clone(), None);

    // Text.
    let resp = router
        .clone()
        .oneshot(
            authed(Request::post("/api/capture/text"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"text":"typed in a browser"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Photo: a PNG goes in, JPEG comes out.
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        8,
        8,
        image::Rgb([0, 128, 255]),
    ));
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png).unwrap();
    let (ct, body) = multipart_body(png.get_ref());
    let resp = router
        .clone()
        .oneshot(
            authed(Request::post("/api/capture/photo"))
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let photo = body_json(resp).await;
    let photo_url = photo["media"]["url"].as_str().unwrap().to_string();

    // Audio: an m4a-shaped container passes straight through.
    let mut m4a = vec![0, 0, 0, 24];
    m4a.extend_from_slice(b"ftypM4A ");
    m4a.extend_from_slice(&[7; 64]);
    let (ct, body) = multipart_body(&m4a);
    let resp = router
        .clone()
        .oneshot(
            authed(Request::post("/api/capture/audio"))
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Feed shows all three, newest first.
    let resp = router
        .clone()
        .oneshot(authed(Request::get("/api/feed")).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let feed = body_json(resp).await;
    let kinds: Vec<_> = feed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, vec!["audio", "photo", "text"]);

    // Media fetch returns the normalized JPEG.
    let resp = router
        .clone()
        .oneshot(authed(Request::get(&photo_url)).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/jpeg"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
}

#[tokio::test]
async fn capture_video_mp4_roundtrip() {
    let (_d, state) = test_state().await;
    let router = app(state, None);

    // An mp4-shaped container passes straight through.
    let mut mp4 = vec![0, 0, 0, 24];
    mp4.extend_from_slice(b"ftypisom");
    mp4.extend_from_slice(&[7; 64]);
    let (ct, body) = multipart_body(&mp4);
    let resp = router
        .clone()
        .oneshot(
            authed(Request::post("/api/capture/video"))
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let entry = body_json(resp).await;
    assert_eq!(entry["kind"], "video");

    // Media fetch serves it back as video/mp4, bytes intact.
    let url = entry["media"]["url"].as_str().unwrap().to_string();
    let resp = router
        .clone()
        .oneshot(authed(Request::get(&url)).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "video/mp4"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(memorious_core::media::is_mp4_family(&bytes));

    // Garbage is refused.
    let (ct, body) = multipart_body(b"definitely not a video");
    let resp = router
        .clone()
        .oneshot(
            authed(Request::post("/api/capture/video"))
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn audio_transcodes_webm_when_ffmpeg_available() {
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("ffmpeg not installed — skipping transcode test");
        return;
    }
    // Make a tiny real webm/opus file with ffmpeg itself.
    let dir = tempfile::tempdir().unwrap();
    let webm_path = dir.path().join("t.webm");
    let ok = std::process::Command::new("ffmpeg")
        .args(["-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=0.3"])
        .args(["-c:a", "libopus"])
        .arg(&webm_path)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "ffmpeg fixture generation failed");
    let webm = std::fs::read(&webm_path).unwrap();

    let (_d, state) = test_state().await;
    let router = app(state, None);
    let (ct, body) = multipart_body(&webm);
    let resp = router
        .clone()
        .oneshot(
            authed(Request::post("/api/capture/audio"))
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let entry = body_json(resp).await;

    // Stored blob must be an mp4-family container now.
    let url = entry["media"]["url"].as_str().unwrap().to_string();
    let resp = router
        .clone()
        .oneshot(authed(Request::get(&url)).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(memorious_core::media::is_mp4_family(&bytes));
}

#[tokio::test]
async fn redact_hides_from_feed_shows_in_trash() {
    let (_d, state) = test_state().await;
    let router = app(state, None);

    let resp = router
        .clone()
        .oneshot(
            authed(Request::post("/api/capture/text"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"text":"mistake"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let id = body_json(resp).await["event_id"].as_str().unwrap().to_string();

    let resp = router
        .clone()
        .oneshot(
            authed(Request::post("/api/redact"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"event_id":"{id}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let feed = body_json(
        router
            .clone()
            .oneshot(authed(Request::get("/api/feed")).body(Body::empty()).unwrap())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(feed["entries"].as_array().unwrap().len(), 0);

    let trash = body_json(
        router
            .clone()
            .oneshot(authed(Request::get("/api/trash")).body(Body::empty()).unwrap())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(trash["entries"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn downloads_are_listed_and_publicly_fetchable() {
    let dir = tempfile::tempdir().unwrap();
    let dl = dir.path().join("downloads");
    std::fs::create_dir_all(&dl).unwrap();
    std::fs::write(dl.join("journal-cli-macos-arm64"), b"fake binary").unwrap();
    std::fs::write(dl.join(".hidden"), b"nope").unwrap();

    let journal = Journal::init(&dir.path().join("j"), "pw").unwrap();
    journal.set_passcode("sesame").unwrap();
    let state = Arc::new(AppState {
        node: Node::spawn(journal).await.unwrap(),
        downloads_dir: Some(dl),
    });
    let router = app(state, None);

    // Authed listing.
    let resp = router
        .clone()
        .oneshot(authed(Request::get("/api/downloads")).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let files = list["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "dotfiles are hidden");
    assert_eq!(files[0]["name"], "journal-cli-macos-arm64");
    assert_eq!(files[0]["url"], "/downloads/journal-cli-macos-arm64");

    // The file itself is public (software only, never journal data).
    let resp = router
        .clone()
        .oneshot(
            Request::get("/downloads/journal-cli-macos-arm64")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"fake binary");

    // But the listing endpoint stays behind auth like the rest of /api.
    let resp = router
        .clone()
        .oneshot(Request::get("/api/downloads").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn server_peer_converges_with_core_peer() {
    let (_d, state) = test_state().await;

    // Capture on the server over "HTTP" (direct handler call).
    let router = app(state.clone(), None);
    router
        .clone()
        .oneshot(
            authed(Request::post("/api/capture/text"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"text":"from the server"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // A plain core peer joins via the server's ticket and captures too.
    let dir = tempfile::tempdir().unwrap();
    let ticket = state.node.ticket().unwrap();
    let (peer, report) = Node::join_from_ticket(&dir.path().join("cli"), &ticket, "pw")
        .await
        .unwrap();
    assert!(report.received >= 1);
    peer.journal().capture_text("from the cli").unwrap();
    peer.sync_with(&state.node.addr()).await.unwrap();

    let server_ids: Vec<_> = state
        .node
        .journal()
        .list()
        .unwrap()
        .iter()
        .map(|e| e.event_id.clone())
        .collect();
    let peer_ids: Vec<_> = peer
        .journal()
        .list()
        .unwrap()
        .iter()
        .map(|e| e.event_id.clone())
        .collect();
    assert_eq!(server_ids, peer_ids);
    assert_eq!(server_ids.len(), 2);

    peer.shutdown().await;
}
