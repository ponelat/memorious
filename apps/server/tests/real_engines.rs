//! Real whisper + tesseract, exercised only when the tools are installed.
//! Fixtures are synthesized on the fly: macOS `say` for speech, ffmpeg drawtext
//! for a text image.

use memorious_server::sweeper::{Engines, SystemEngines};

fn have(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn whisper_transcribes_synthesized_speech() {
    let Some(engines) = SystemEngines::detect() else {
        eprintln!("engines not installed — skipping");
        return;
    };
    if !have("say") {
        eprintln!("no `say` (not macOS?) — skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let aiff = dir.path().join("s.aiff");
    let m4a = dir.path().join("s.m4a");
    assert!(std::process::Command::new("say")
        .args(["-o"])
        .arg(&aiff)
        .arg("the quick brown fox jumped over the lazy dog")
        .status()
        .unwrap()
        .success());
    assert!(std::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(&aiff)
        .args(["-c:a", "aac"])
        .arg(&m4a)
        .status()
        .unwrap()
        .success());

    let text = engines
        .transcribe(&std::fs::read(&m4a).unwrap())
        .unwrap()
        .to_lowercase();
    assert!(
        text.contains("quick brown fox"),
        "unexpected transcript: {text:?}"
    );
}

#[test]
fn tesseract_reads_rendered_text() {
    let Some(engines) = SystemEngines::detect() else {
        eprintln!("engines not installed — skipping");
        return;
    };
    // Committed fixture (rendered text); engines take JPEG in production, so
    // normalize exactly like the capture path does.
    let png = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hello.png"
    ))
    .unwrap();
    let jpeg = memorious_core::media::normalize_photo(&png).unwrap();
    let text = engines.ocr(&jpeg).unwrap().to_uppercase();
    assert!(
        text.contains("HELLO JOURNAL"),
        "unexpected OCR output: {text:?}"
    );
}
