//! M3 acceptance, command-layer half: drive the real Tauri IPC (mock runtime) —
//! setup, capture, feed, sync with an external core peer — exactly the calls the
//! shared web UI makes through its Tauri adapter.

use memorious_core::{Journal, Node};
use serde_json::{json, Value};
use tauri::ipc::InvokeBody;
use tauri::test::{mock_builder, mock_context, noop_assets, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::WebviewWindow;

fn invoke_request(cmd: &str, args: Value) -> InvokeRequest {
    InvokeRequest {
        cmd: cmd.into(),
        callback: tauri::ipc::CallbackFn(0),
        error: tauri::ipc::CallbackFn(1),
        url: if cfg!(any(windows, target_os = "android")) {
            "http://tauri.localhost"
        } else {
            "tauri://localhost"
        }
        .parse()
        .unwrap(),
        body: InvokeBody::Json(args),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.into(),
    }
}

async fn invoke(webview: &WebviewWindow<tauri::test::MockRuntime>, cmd: &str, args: Value) -> Result<Value, Value> {
    let (tx, rx) = std::sync::mpsc::channel();
    webview.clone().on_message(
        invoke_request(cmd, args),
        Box::new(move |_webview, _cmd, response, _callback, _error| {
            let _ = tx.send(response);
        }),
    );
    let response = tokio::task::spawn_blocking(move || {
        rx.recv_timeout(std::time::Duration::from_secs(60))
    })
    .await
    .unwrap()
    .expect("command timed out");
    match response {
        tauri::ipc::InvokeResponse::Ok(body) => Ok(body_to_json(body)),
        tauri::ipc::InvokeResponse::Err(e) => Err(e.0),
    }
}

fn body_to_json(body: tauri::ipc::InvokeResponseBody) -> Value {
    match body {
        tauri::ipc::InvokeResponseBody::Json(s) => serde_json::from_str(&s).unwrap(),
        tauri::ipc::InvokeResponseBody::Raw(bytes) => json!({"__raw_len": bytes.len()}),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn desktop_command_layer_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let desktop_dir = dir.path().join("desktop-journal");
    std::env::set_var("MEMORIOUS_DATA_DIR", &desktop_dir);

    let app = mock_builder()
        .manage(memorious_desktop_lib::NodeState::default())
        .invoke_handler(memorious_desktop_lib::handlers())
        .build(mock_context(noop_assets()))
        .unwrap();
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();

    // Fresh install → empty → init.
    let state = invoke(&webview, "setup_state", json!({})).await.unwrap();
    assert_eq!(state, json!("empty"));
    invoke(&webview, "setup_init", json!({})).await.unwrap();
    let state = invoke(&webview, "setup_state", json!({})).await.unwrap();
    assert_eq!(state, json!("ready"));

    // Capture text + photo (PNG in → JPEG stored).
    let entry = invoke(&webview, "capture_text", json!({"text": "typed on the desktop"}))
        .await
        .unwrap();
    assert_eq!(entry["kind"], "text");

    let img = image_png_bytes();
    let photo = invoke(
        &webview,
        "capture_media",
        json!({"kind": "photo", "bytes": img}),
    )
    .await
    .unwrap();
    assert_eq!(photo["kind"], "photo");
    let hash = photo["media"]["hash"].as_str().unwrap().to_string();

    // Feed shows both; media_bytes returns raw bytes.
    let feed = invoke(&webview, "feed", json!({})).await.unwrap();
    assert_eq!(feed["entries"].as_array().unwrap().len(), 2);
    let media = invoke(&webview, "media_bytes", json!({"hash": hash})).await.unwrap();
    assert!(media["__raw_len"].as_u64().unwrap() > 100);

    // An external core peer (stands in for the server) with entries of its own.
    let peer_journal = Journal::init(&dir.path().join("peer")).unwrap();
    // Same journal secret — as if the desktop had been paired to it.
    let desktop_secret = *Journal::open(&desktop_dir).unwrap().secret();
    drop(peer_journal);
    std::fs::remove_dir_all(dir.path().join("peer")).unwrap();
    let peer_journal =
        Journal::init_with_secret(&dir.path().join("peer"), desktop_secret).unwrap();
    peer_journal.capture_text("from the other peer").unwrap();
    let peer = Node::spawn(peer_journal).await.unwrap();
    let ticket = peer.ticket().unwrap();

    // Desktop dials out via the sync_now command; both sides converge.
    let report = invoke(&webview, "sync_now", json!({"ticket": ticket})).await.unwrap();
    assert_eq!(report["received"], 1);
    assert_eq!(report["sent"], 2);
    let feed = invoke(&webview, "feed", json!({})).await.unwrap();
    assert_eq!(feed["entries"].as_array().unwrap().len(), 3);
    assert_eq!(peer.journal().list().unwrap().len(), 3);

    // Third peer (the "CLI peer"): joins from the first peer's ticket, captures,
    // then the desktop syncs with it — all three converge.
    let (cli_peer, _) = Node::join_from_ticket(&dir.path().join("cli"), &ticket)
        .await
        .unwrap();
    cli_peer.journal().capture_text("from the cli peer").unwrap();
    invoke(
        &webview,
        "sync_now",
        json!({"ticket": cli_peer.ticket().unwrap()}),
    )
    .await
    .unwrap();
    peer.sync_with(&cli_peer.addr()).await.unwrap();
    let desktop_ids: Vec<String> = invoke(&webview, "feed", json!({})).await.unwrap()["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_id"].as_str().unwrap().to_string())
        .collect();
    let ids = |j: &Journal| -> Vec<String> {
        let mut v: Vec<String> = j.list().unwrap().iter().map(|e| e.event_id.clone()).collect();
        v.reverse(); // feed is newest-first
        v
    };
    assert_eq!(desktop_ids, ids(peer.journal()));
    assert_eq!(desktop_ids, ids(cli_peer.journal()));
    assert_eq!(desktop_ids.len(), 4);

    // Wrong-journal ticket is refused.
    let stranger = Node::spawn(Journal::init(&dir.path().join("stranger")).unwrap())
        .await
        .unwrap();
    let bad = invoke(
        &webview,
        "sync_now",
        json!({"ticket": stranger.ticket().unwrap()}),
    )
    .await;
    assert!(bad.is_err());

    // Redact propagates on next sync.
    let id = feed["entries"][0]["event_id"].as_str().unwrap();
    invoke(&webview, "redact", json!({"eventId": id})).await.unwrap();
    let trash = invoke(&webview, "trash_list", json!({})).await.unwrap();
    assert_eq!(trash["entries"].as_array().unwrap().len(), 1);

    // Search works through IPC.
    let hits = invoke(&webview, "search", json!({"q": "desktop"})).await.unwrap();
    assert_eq!(hits["entries"].as_array().unwrap().len(), 1);

    // Status carries a ticket other devices can join from.
    let status = invoke(&webview, "status", json!({})).await.unwrap();
    assert!(status["ticket"].as_str().unwrap().starts_with("memorious"));

    peer.shutdown().await;
    cli_peer.shutdown().await;
    stranger.shutdown().await;
}

fn image_png_bytes() -> Vec<u8> {
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        6,
        6,
        image::Rgb([10, 200, 30]),
    ));
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png).unwrap();
    png.into_inner()
}
