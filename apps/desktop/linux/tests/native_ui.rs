#![cfg(feature = "gui")]
// Run with: dbus-run-session -- xvfb-run cargo test -p memorious-linux --test native_ui
#[test]
#[ignore = "requires a graphical display"]
fn native_application_opens_setup_without_a_webview() {
    let dir = tempfile::tempdir().unwrap();
    let result = std::process::Command::new("dbus-run-session")
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_memorious-desktop"))
        .env("MEMORIOUS_DATA_DIR", dir.path())
        .env("MEMORIOUS_NO_KEYRING", "1")
        .env("MEMORIOUS_UI_SMOKE", "1")
        .status()
        .unwrap();
    assert!(result.success());
    assert!(!dir.path().join("db.sqlite").exists());
}

#[test]
#[ignore = "requires a graphical display"]
fn native_setup_and_capture_button_persist_an_encrypted_entry() {
    let dir = tempfile::tempdir().unwrap();
    let result = std::process::Command::new("dbus-run-session")
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_memorious-desktop"))
        .env("MEMORIOUS_DATA_DIR", dir.path())
        .env("MEMORIOUS_NO_KEYRING", "1")
        .env("MEMORIOUS_UI_SMOKE", "capture")
        .status()
        .unwrap();
    assert!(result.success());
    let result = std::process::Command::new("dbus-run-session")
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_memorious-desktop"))
        .env("MEMORIOUS_DATA_DIR", dir.path())
        .env("MEMORIOUS_NO_KEYRING", "1")
        .env("MEMORIOUS_UI_SMOKE", "verify")
        .status()
        .unwrap();
    assert!(result.success());
    assert_ne!(
        &std::fs::read(dir.path().join("db.sqlite")).unwrap()[..16],
        b"SQLite format 3\0"
    );
}

#[test]
#[ignore = "requires a graphical display"]
fn native_settings_offer_local_and_remote_transcription_without_requiring_a_local_api_key() {
    let dir = tempfile::tempdir().unwrap();
    let result = std::process::Command::new("dbus-run-session")
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_memorious-desktop"))
        .env("MEMORIOUS_DATA_DIR", dir.path())
        .env("MEMORIOUS_NO_KEYRING", "1")
        .env("MEMORIOUS_UI_SMOKE", "settings")
        .status()
        .unwrap();
    assert!(result.success());
    assert!(dir.path().join(".native-settings-tested").exists());
}

#[test]
#[ignore = "requires a graphical display"]
fn native_peers_exposes_network_settings_and_connection_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let result = std::process::Command::new("dbus-run-session").arg("--")
        .arg(env!("CARGO_BIN_EXE_memorious-desktop"))
        .env("MEMORIOUS_DATA_DIR", dir.path()).env("MEMORIOUS_NO_KEYRING", "1")
        .env("MEMORIOUS_UI_SMOKE", "peers").status().unwrap();
    assert!(result.success());
    assert!(dir.path().join(".native-peers-tested").exists());
}
