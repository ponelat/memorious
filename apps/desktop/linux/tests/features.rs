use memorious_linux::DesktopJournal;
use serde_json::json;

#[tokio::test]
async fn native_ink_is_durable_cursor_based_and_outside_the_feed() {
    let dir = tempfile::tempdir().unwrap();
    let app = DesktopJournal::create(dir.path(), "password")
        .await
        .unwrap();
    let op = json!({"op":"stroke","cid":"gtk-stroke","tool":"pen","seed":1,"size":5,"pts":[[10,20,0.5,0],[40,50,0.7,0]]});
    let appended = app
        .request("ink_append", json!({"period":"2026-W48","ops":[op]}))
        .await
        .unwrap();
    let page = app
        .request("ink_list", json!({"period":"2026-W48"}))
        .await
        .unwrap();
    assert_eq!(
        page["events"][0]["event_id"],
        appended["events"][0]["event_id"]
    );
    assert!(page["events"][0]["seq"].as_u64().unwrap() > 0);
    let empty = app
        .request(
            "ink_list",
            json!({"period":"2026-W48","after":page["cursor"]}),
        )
        .await
        .unwrap();
    assert_eq!(empty["events"], json!([]));
    assert_eq!(empty["cursor"], page["cursor"]);
    assert_eq!(
        app.request("ink_months", json!({})).await.unwrap()["months"],
        json!(["2026-W48"])
    );
    assert!(app.entries("", false, 50).unwrap().is_empty());
    assert!(app
        .request(
            "ink_append",
            json!({"period":"invalid","ops":[{"op":"stroke"}]})
        )
        .await
        .is_err());
    app.close().await;
    let app = DesktopJournal::open(dir.path(), "password").await.unwrap();
    assert_eq!(
        app.request("ink_list", json!({"period":"2026-W48"}))
            .await
            .unwrap()["events"],
        page["events"]
    );
    app.close().await;
}

#[tokio::test]
async fn native_retention_and_isolation_preserve_captures() {
    let dir = tempfile::tempdir().unwrap();
    let app = DesktopJournal::create(dir.path(), "password")
        .await
        .unwrap();
    app.capture_text("keep this entry").unwrap();
    app.request(
        "set_peer_required",
        json!({"endpoint_id":"example-peer","required":false}),
    )
    .await
    .unwrap();
    app.request("set_retention", json!({"preset":"phone"}))
        .await
        .unwrap();
    assert_eq!(app.status().await.unwrap()["retention"]["voice_audio"]["days"], 7);
    assert!(app
        .request("set_retention", json!({"preset":"typo"}))
        .await
        .is_err());
    app.request("isolate", json!({})).await.unwrap();
    assert_eq!(app.status().await.unwrap()["isolated"], true);
    assert!(app.sync("").await.is_err());
    app.request("end_isolation", json!({})).await.unwrap();
    assert_eq!(app.status().await.unwrap()["isolated"], false);
    assert_eq!(app.entries("", false, 50).unwrap().len(), 1);
    app.close().await;
}

#[tokio::test]
async fn native_canvas_captures_push_to_the_other_peer_immediately() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let first = DesktopJournal::create(a.path(), "password").await.unwrap();
    let second = DesktopJournal::join(b.path(), &first.ticket().unwrap(), "password")
        .await
        .unwrap();
    first.request("ink_append",json!({"period":"2026-W48","ops":[{"op":"stroke","cid":"sync-test","tool":"pen","seed":1,"size":5,"pts":[[1,1,0.5,0]]}]})).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            let page = second
                .request("ink_list", json!({"period":"2026-W48"}))
                .await
                .unwrap();
            if page["events"].as_array().unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("ink captures must push without a manual sync");
    second.close().await;
    first.close().await;
}

#[tokio::test]
async fn native_archive_includes_the_new_schema_and_ink() {
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let app = DesktopJournal::create(dir.path(), "password")
        .await
        .unwrap();
    app.request(
        "ink_append",
        json!({"period":"2026-W48","ops":[{"op":"future-tool","cid":"archive-test"}]}),
    )
    .await
    .unwrap();
    let destination = out.path().join("archive.zip");
    let report = app
        .request("export_archive", json!({"out":destination,"media":false}))
        .await
        .unwrap();
    assert!(report["events"].as_u64().unwrap() > 0);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&destination).unwrap().permissions().mode() & 0o777,0o600);
    let file = std::fs::read(destination).unwrap();
    assert!(file.starts_with(b"PK"));
    app.close().await;
}

#[tokio::test]
async fn native_write_only_join_does_not_pull_history_and_pushes_captures() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let source = DesktopJournal::create(a.path(), "password").await.unwrap();
    source.capture_text("existing history").unwrap();
    let capture = DesktopJournal::join_write_only(b.path(), &source.ticket().unwrap(), "password")
        .await
        .unwrap();
    assert_eq!(capture.status().await.unwrap()["write_only"], true);
    capture.capture_text("new capture").unwrap();
    capture.sync("").await.unwrap();
    assert_eq!(capture.entries("", false, 50).unwrap().len(), 1);
    assert_eq!(source.entries("", false, 50).unwrap().len(), 2);
    capture.close().await;
    source.close().await;
}
