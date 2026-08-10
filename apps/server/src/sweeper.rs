//! The enrichment sweeper: the always-on peer's background pass that turns
//! audio into transcripts (whisper) and photos into OCR text, as annotation
//! events. Engines are behind a trait so tests run without models.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use journal_core::enrich::DEFAULT_GRACE_MS;
use journal_core::event::Payload;
use journal_core::Node;

pub trait Engines: Send + Sync + 'static {
    fn transcribe(&self, m4a: &[u8]) -> Result<String>;
    fn ocr(&self, jpeg: &[u8]) -> Result<String>;
}

/// One sweep: enrich everything due right now. Returns annotations written.
pub async fn sweep_once(node: &Node, engines: &dyn Engines, grace_ms: i64) -> Result<usize> {
    let due = node.journal().pending_enrichment(grace_ms)?;
    let mut written = 0;
    for event in due {
        let Some(hash) = event.blob_hash() else { continue };
        let bytes = match node.blob_bytes(hash).await {
            Ok(b) => b,
            Err(err) => {
                // Blob not local yet (sync in flight) — a later sweep gets it.
                tracing::debug!("blob {hash} not available yet: {err:#}");
                continue;
            }
        };
        let result = match &event.payload {
            Payload::Audio { .. } => engines.transcribe(&bytes),
            Payload::Photo { .. } => engines.ocr(&bytes),
            _ => continue,
        };
        match result {
            Ok(text) => {
                node.journal().annotate(&event.event_id, text.trim())?;
                written += 1;
                tracing::info!("enriched {} ({} chars)", event.event_id, text.trim().len());
            }
            Err(err) => {
                // Leave it pending; the next sweep retries.
                tracing::warn!("enrichment failed for {}: {err:#}", event.event_id);
            }
        }
    }
    Ok(written)
}

/// Run forever. `interval` between sweeps; grace from UNDERSTANDING.md unless overridden.
pub fn spawn(node: Arc<crate::AppState>, engines: Arc<dyn Engines>) {
    let interval = std::env::var("ENRICH_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60_000u64);
    let grace = std::env::var("ENRICH_GRACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_GRACE_MS);
    tokio::spawn(async move {
        loop {
            if let Err(err) = sweep_once(&node.node, engines.as_ref(), grace).await {
                tracing::warn!("sweep failed: {err:#}");
            }
            tokio::time::sleep(Duration::from_millis(interval)).await;
        }
    });
}

/// Real engines: whisper-cli (whisper.cpp) + tesseract, both via subprocess.
pub struct SystemEngines {
    pub whisper_model: PathBuf,
}

impl SystemEngines {
    /// Probe the system; None (with a log line) if tools or model are missing.
    pub fn detect() -> Option<Self> {
        let model = std::env::var_os("WHISPER_MODEL")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs_home().join(".cache/whisper/ggml-base.bin")
            });
        let whisper = which("whisper-cli");
        let tesseract = which("tesseract");
        let ffmpeg = which("ffmpeg");
        if whisper && tesseract && ffmpeg && model.exists() {
            Some(Self { whisper_model: model })
        } else {
            tracing::warn!(
                "enrichment disabled: whisper-cli={whisper} tesseract={tesseract} ffmpeg={ffmpeg} model={}",
                model.display()
            );
            None
        }
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn which(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run(cmd: &mut std::process::Command) -> Result<std::process::Output> {
    let out = cmd.output().with_context(|| format!("spawn {cmd:?}"))?;
    if !out.status.success() {
        bail!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).chars().take(400).collect::<String>()
        );
    }
    Ok(out)
}

impl Engines for SystemEngines {
    fn transcribe(&self, m4a: &[u8]) -> Result<String> {
        let dir = tempfile::tempdir()?;
        let in_path = dir.path().join("in.m4a");
        let wav = dir.path().join("in.wav");
        std::fs::write(&in_path, m4a)?;
        // whisper.cpp wants 16 kHz mono wav.
        run(std::process::Command::new("ffmpeg")
            .args(["-y", "-i"])
            .arg(&in_path)
            .args(["-ar", "16000", "-ac", "1"])
            .arg(&wav))?;
        let out_base = dir.path().join("out");
        run(std::process::Command::new("whisper-cli")
            .arg("-m")
            .arg(&self.whisper_model)
            .args(["-otxt", "-of"])
            .arg(&out_base)
            .args(["-np"])
            .arg(&wav))?;
        let text = std::fs::read_to_string(out_base.with_extension("txt"))
            .context("whisper txt output")?;
        Ok(text.trim().to_string())
    }

    fn ocr(&self, jpeg: &[u8]) -> Result<String> {
        let dir = tempfile::tempdir()?;
        let in_path = dir.path().join("in.jpg");
        std::fs::write(&in_path, jpeg)?;
        let out = run(std::process::Command::new("tesseract")
            .arg(&in_path)
            .arg("stdout")
            .args(["--psm", "3"]))?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}
