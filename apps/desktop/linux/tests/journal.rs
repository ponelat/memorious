use memorious_linux::DesktopJournal;
use tempfile::tempdir;

#[tokio::test]
async fn native_capture_search_redaction_and_reopen_use_the_same_encrypted_journal() {
    let dir = tempdir().unwrap();
    let app = DesktopJournal::create(dir.path(), "password")
        .await
        .unwrap();
    let first = app.capture_text("native penguin").unwrap();
    let second = app.capture_text("second capture").unwrap();
    let feed = app.entries("", false, 50).unwrap();
    assert_eq!(feed[0]["event_id"], second["event_id"]);
    assert_eq!(app.entries("penguin", false, 50).unwrap().len(), 1);
    app.redact(first["event_id"].as_str().unwrap()).unwrap();
    assert!(app.entries("penguin", false, 50).unwrap().is_empty());
    assert_eq!(app.entries("", true, 50).unwrap().len(), 1);
    assert!(DesktopJournal::open(dir.path(), "password").await.is_err());
    app.close().await;
    assert!(DesktopJournal::open(dir.path(), "wrong").await.is_err());
    let app = DesktopJournal::open(dir.path(), "password").await.unwrap();
    assert_eq!(app.entries("", false, 50).unwrap().len(), 1);
    app.close().await;
}

#[tokio::test]
async fn native_pairing_keeps_identity_and_syncs_captures() {
    let a = tempdir().unwrap();
    let b = tempdir().unwrap();
    let first = DesktopJournal::create(a.path(), "password").await.unwrap();
    first.capture_text("before pairing").unwrap();
    let second = DesktopJournal::join(b.path(), &first.ticket().unwrap(), "password")
        .await
        .unwrap();
    assert_eq!(second.entries("", false, 50).unwrap().len(), 1);
    second.capture_text("from Linux").unwrap();
    second.sync("").await.unwrap();
    assert_eq!(first.entries("", false, 50).unwrap().len(), 2);
    first.capture_text("from the inbound peer").unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while second.entries("", false, 50).unwrap().len() != 3 {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("capture must sync to known inbound peers without a pasted ticket");
    second.close().await;
    first.close().await;
}

#[tokio::test]
async fn native_media_normalizes_photos_and_rejects_non_m4a_audio() {
    let dir = tempdir().unwrap();
    let app = DesktopJournal::create(dir.path(), "password")
        .await
        .unwrap();
    assert!(app
        .capture_media("voice", b"not audio".to_vec())
        .await
        .is_err());
    assert!(app
        .capture_media("photo", b"not an image".to_vec())
        .await
        .is_err());
    assert!(app.entries("", false, 50).unwrap().is_empty());
    let mut png = std::io::Cursor::new(Vec::new());
    image::RgbImage::new(2, 2)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let entry = app.capture_media("photo", png.into_inner()).await.unwrap();
    let bytes = app
        .media(entry["media"]["hash"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        image::guess_format(&bytes).unwrap(),
        image::ImageFormat::Jpeg
    );
    app.close().await;
}
