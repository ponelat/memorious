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
    let result = std::process::Command::new("dbus-run-session")
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_memorious-desktop"))
        .env("MEMORIOUS_DATA_DIR", dir.path())
        .env("MEMORIOUS_NO_KEYRING", "1")
        .env("MEMORIOUS_UI_SMOKE", "peers")
        .status()
        .unwrap();
    assert!(result.success());
    assert!(dir.path().join(".native-peers-tested").exists());
}

#[test]
#[ignore = "requires a graphical display"]
fn native_stream_uses_mobile_typography_and_scroll_only_rules() {
    let dir = tempfile::tempdir().unwrap();
    let result = std::process::Command::new("dbus-run-session")
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_memorious-desktop"))
        .env("MEMORIOUS_DATA_DIR", dir.path())
        .env("MEMORIOUS_NO_KEYRING", "1")
        .env("MEMORIOUS_UI_SMOKE", "stream")
        .env("GDK_DEBUG", "no-portals")
        .env("GTK_A11Y", "none")
        .status()
        .unwrap();
    assert!(result.success());
    assert!(!dir.path().join("db.sqlite").exists());
}

#[test]
#[ignore = "requires a graphical display"]
fn composer_has_recording_controls_and_remembers_appearance() {
    let dir = tempfile::tempdir().unwrap();
    for mode in ["composer", "appearance"] {
        let result = std::process::Command::new("dbus-run-session")
            .arg("--")
            .arg(env!("CARGO_BIN_EXE_memorious-desktop"))
            .env("MEMORIOUS_DATA_DIR", dir.path().join("journal"))
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("MEMORIOUS_NO_KEYRING", "1")
            .env("MEMORIOUS_UI_SMOKE", mode)
            .env("GDK_DEBUG", "no-portals")
            .env("GTK_A11Y", "none")
            .status()
            .unwrap();
        assert!(result.success());
    }
    assert!(!dir.path().join("journal/db.sqlite").exists());
}

#[test]
#[ignore = "requires a graphical display"]
fn microphone_button_saves_audio_and_cancel_discards_it_without_ptt() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let recorder = dir.path().join("pw-record");
    std::fs::write(&recorder, "#!/usr/bin/env python3\nimport os, signal, time\nsignal.signal(signal.SIGINT, lambda *_: exit(0))\nos.write(1, b'\\x01\\x00' * 16000)\nwhile True: time.sleep(.01)\n").unwrap();
    std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap()
    );
    for mode in ["recording", "audio-verify"] {
        let result = std::process::Command::new("dbus-run-session")
            .arg("--")
            .arg(env!("CARGO_BIN_EXE_memorious-desktop"))
            .env("MEMORIOUS_DATA_DIR", dir.path().join("journal"))
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("PATH", &path)
            .env("MEMORIOUS_NO_KEYRING", "1")
            .env("MEMORIOUS_UI_SMOKE", mode)
            .env("GDK_DEBUG", "no-portals")
            .env("GTK_A11Y", "none")
            .status()
            .unwrap();
        assert!(result.success());
    }
    assert_ne!(
        &std::fs::read(dir.path().join("journal/db.sqlite")).unwrap()[..16],
        b"SQLite format 3\0"
    );
}

#[test]
#[ignore = "requires a graphical display"]
fn native_canvas_draws_selects_undoes_and_reopens_from_the_encrypted_journal() {
    let dir = tempfile::tempdir().unwrap();
    for mode in ["canvas", "canvas-verify"] {
        let result = std::process::Command::new("dbus-run-session")
            .arg("--")
            .arg(env!("CARGO_BIN_EXE_memorious-desktop"))
            .env("MEMORIOUS_DATA_DIR", dir.path().join("journal"))
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("MEMORIOUS_NO_KEYRING", "1")
            .env("MEMORIOUS_UI_SMOKE", mode)
            .env("GDK_DEBUG", "no-portals")
            .env("GTK_A11Y", "none")
            .status()
            .unwrap();
        assert!(result.success());
    }
    assert!(dir.path().join("journal/.native-canvas-tested").exists());
    assert!(dir.path().join("journal/.native-canvas-verified").exists());
}
