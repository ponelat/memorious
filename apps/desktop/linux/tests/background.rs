#![cfg(feature = "gui")]
#[tokio::test]
async fn background_handoff_preserves_identity_and_releases_ownership_on_stop() {
    use memorious_linux::background::Session;
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create(dir.path(), "password").await.unwrap();
    let identity = session
        .request("status", serde_json::json!({}))
        .await
        .unwrap()["device_id"]
        .clone();
    session
        .enable_background(std::path::Path::new(env!(
            "CARGO_BIN_EXE_memorious-desktop"
        )))
        .await
        .unwrap();
    assert!(session.is_background());
    session
        .request(
            "capture_text",
            serde_json::json!({"text":"from native toolbar"}),
        )
        .await
        .unwrap();
    session.close().await;
    let mut session = Session::attach(dir.path()).await.unwrap();
    assert_eq!(
        session
            .request("status", serde_json::json!({}))
            .await
            .unwrap()["device_id"],
        identity
    );
    assert!(session.disable_background("wrong").await.is_err());
    assert!(session
        .request("health", serde_json::json!({}))
        .await
        .is_ok());
    session.disable_background("password").await.unwrap();
    assert!(!session.is_background());
    assert_eq!(
        session
            .request("feed", serde_json::json!({}))
            .await
            .unwrap()["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    session.close().await;
}

#[test]
fn ptt_settings_preserve_saved_key_without_exposing_it_in_status() {
    use memorious_linux::background::{load_ptt, ptt_status, save_ptt};
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    save_ptt(
        dir.path(),
        true,
        "KEY_RIGHTCTRL",
        Some("test-transcription-key"),
    )
    .unwrap();
    save_ptt(dir.path(), true, "KEY_F8", None).unwrap();
    let config = load_ptt(dir.path()).unwrap();
    assert_eq!(config.api_key, "test-transcription-key");
    assert_eq!(config.key, "KEY_F8");
    assert_eq!(
        std::fs::metadata(dir.path().join(".memorious-ptt.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!ptt_status(dir.path())
        .to_string()
        .contains("test-transcription-key"));
}

#[test]
fn local_transcription_needs_no_api_key_and_old_configs_stay_remote() {
    use memorious_linux::background::{load_ptt, save_ptt_with_provider};
    let dir = tempfile::tempdir().unwrap();
    save_ptt_with_provider(dir.path(), true, "KEY_RIGHTCTRL", None, "local").unwrap();
    assert_eq!(load_ptt(dir.path()).unwrap().provider, "local");
    assert!(save_ptt_with_provider(dir.path(), true, "KEY_RIGHTCTRL", None, "remote").is_err());
    std::fs::write(
        dir.path().join(".memorious-ptt.json"),
        r#"{"enabled":true,"key":"KEY_RIGHTCTRL","api_key":"saved-key"}"#,
    )
    .unwrap();
    assert_eq!(load_ptt(dir.path()).unwrap().provider, "remote");
}

#[test]
fn transcription_language_is_persisted_and_invalid_values_do_not_change_settings() {
    use memorious_linux::background::{load_ptt, save_ptt_options};
    let dir = tempfile::tempdir().unwrap();
    save_ptt_options(dir.path(), true, "KEY_RIGHTCTRL", None, "local", "af").unwrap();
    assert_eq!(load_ptt(dir.path()).unwrap().language, "af");
    assert!(save_ptt_options(
        dir.path(),
        true,
        "KEY_RIGHTCTRL",
        None,
        "local",
        "bad language"
    )
    .is_err());
    assert_eq!(load_ptt(dir.path()).unwrap().language, "af");
}

#[tokio::test]
async fn background_sync_uses_an_inbound_peer_without_a_saved_ticket() {
    use memorious_linux::background::Session;
    use serde_json::json;
    let directory = tempfile::tempdir().unwrap();
    let peer_directory = tempfile::tempdir().unwrap();
    let mut session = Session::create(directory.path(), "password").await.unwrap();
    let status = session.status().await.unwrap();
    let (peer, _) = memorious_core::Node::pair_from_ticket(
        peer_directory.path(),
        status["ticket"].as_str().unwrap(),
        "password",
    )
    .await
    .unwrap();
    let executable = std::env::var_os("MEMORIOUS_TEST_DESKTOP_EXE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_memorious-desktop").into());
    session.enable_background(&executable).await.unwrap();
    peer.journal()
        .capture_text("from a known inbound peer")
        .unwrap();
    let result = session.sync("").await.unwrap();
    assert_eq!(result["received"], 1);
    assert_eq!(result["peers"][0]["ok"], true);
    assert_eq!(
        session.request("feed", json!({})).await.unwrap()["entries"][0]["text"],
        "from a known inbound peer"
    );
    session.disable_background("password").await.unwrap();
    session.close().await;
    peer.shutdown().await;
}
