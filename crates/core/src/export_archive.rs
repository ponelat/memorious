//! The **lossless archive**: every event as plain JSON, a JSON Schema describing them, and the media in
//! ordinary formats (jpg / m4a / mp4). Unlike [`crate::export_md`] — a one-way markdown *mirror* — this is meant to
//! be a backup a person or an AI could ingest without Memorious: nothing is summarised, redacted captures keep
//! their events (only their bytes are left out) and unknown event kinds are copied through untouched.
//!
//! Layout (the zip's root folder, or the output directory):
//!
//! ```text
//! README.md            how to read it
//! manifest.json        what is in here, checksums, what is deliberately not
//! events.jsonl         every event, one JSON object per line   (or events.json: one array)
//! schema/event.schema.json
//! schema/manifest.schema.json
//! media/<hash>.<ext>   decrypted media (jpg, m4a, mp4); <hash> is the `hash` in the event payload
//! ```
//!
//! Output goes through an [`ArchiveSink`], so the same code writes a folder ([`DirSink`]) or streams a zip
//! ([`ZipSink`]) without holding the media in memory.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use n0_future::StreamExt;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::event::{Event, EventKind, MediaKind};
use crate::node::{BlobOpenError, Node};

pub const FORMAT: &str = "memorious-archive";
pub const FORMAT_VERSION: u32 = 1;

pub const EVENT_SCHEMA: &str = include_str!("../schema/event.schema.json");
pub const MANIFEST_SCHEMA: &str = include_str!("../schema/manifest.schema.json");
const README: &str = include_str!("../schema/ARCHIVE-README.md");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventsEncoding {
    /// One JSON event per line (streamable, greppable). The default.
    Jsonl,
    /// A single JSON array.
    Json,
}

#[derive(Debug, Clone, Copy)]
pub struct ArchiveOpts {
    pub events: EventsEncoding,
    /// Include the media files. Off = events + schema + manifest only (small, fast).
    pub media: bool,
}

impl Default for ArchiveOpts {
    fn default() -> Self {
        Self { events: EventsEncoding::Jsonl, media: true }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct ArchiveReport {
    pub events: usize,
    pub media_included: usize,
    pub media_redacted: usize,
    pub media_unavailable: usize,
    pub media_bytes: u64,
}

/// Where archive files go. `begin` starts a new file (the previous one is finished), `write` appends to it.
pub trait ArchiveSink: Send {
    fn begin(&mut self, path: &str) -> Result<()>;
    fn write(&mut self, bytes: &[u8]) -> Result<()>;
    fn put(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        self.begin(path)?;
        self.write(bytes)
    }
}

/// Writes the archive into a directory.
pub struct DirSink {
    root: PathBuf,
    cur: Option<File>,
}

impl DirSink {
    pub fn new(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        Ok(Self { root: root.to_path_buf(), cur: None })
    }
}

impl ArchiveSink for DirSink {
    fn begin(&mut self, path: &str) -> Result<()> {
        let p = self.root.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.cur = Some(File::create(&p).with_context(|| format!("create {}", p.display()))?);
        Ok(())
    }
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.cur.as_mut().context("write before begin")?.write_all(bytes)?;
        Ok(())
    }
}

/// Streams the archive into a zip. Every entry lives under `prefix/` (e.g. `memorious-archive-20261124-0700/`).
pub struct ZipSink<W: Write + Seek> {
    zip: zip::ZipWriter<W>,
    prefix: String,
}

impl<W: Write + Seek> ZipSink<W> {
    pub fn new(w: W, prefix: &str) -> Self {
        Self { zip: zip::ZipWriter::new(w), prefix: prefix.trim_matches('/').to_string() }
    }
    pub fn finish(self) -> Result<W> {
        Ok(self.zip.finish()?)
    }
}

impl<W: Write + Seek + Send> ArchiveSink for ZipSink<W> {
    fn begin(&mut self, path: &str) -> Result<()> {
        // Media is already compressed; text compresses well. zip64 (`large_file`) because sizes aren't known up front.
        let method = if path.starts_with("media/") { zip::CompressionMethod::Stored } else { zip::CompressionMethod::Deflated };
        let opts = zip::write::SimpleFileOptions::default().compression_method(method).large_file(true);
        self.zip.start_file(format!("{}/{path}", self.prefix), opts)?;
        Ok(())
    }
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.zip.write_all(bytes)?;
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(bytes)
}

fn iso(ms: i64) -> String {
    let (y, m, d) = civil(ms);
    let tod = ms.rem_euclid(86_400_000) / 1000;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", tod / 3600, (tod / 60) % 60, tod % 60)
}

fn civil(ms: i64) -> (i64, i64, i64) {
    let days = ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYYMMDD-HHMMSS` for naming an archive.
pub fn stamp(ms: i64) -> String {
    iso(ms).replace(['-', ':'], "").replace('T', "-").trim_end_matches('Z').to_string()
}

fn media_facts(kind: MediaKind) -> (&'static str, &'static str, &'static str) {
    match kind {
        MediaKind::Photo => ("photo", "jpg", "image/jpeg"),
        MediaKind::Audio => ("audio", "m4a", "audio/mp4"),
        MediaKind::Video => ("video", "mp4", "video/mp4"),
    }
}

/// Write the archive. `now_ms` is the export time recorded in the manifest.
pub async fn export_archive(node: &Node, sink: &mut dyn ArchiveSink, opts: ArchiveOpts, now_ms: i64) -> Result<ArchiveReport> {
    let journal = node.journal();
    let events: Vec<Event> = journal.store.all_events()?;
    let redacted = journal.store.redacted_ids()?;
    let mut report = ArchiveReport { events: events.len(), ..Default::default() };

    sink.put("README.md", README.as_bytes())?;
    sink.put("schema/event.schema.json", EVENT_SCHEMA.as_bytes())?;
    sink.put("schema/manifest.schema.json", MANIFEST_SCHEMA.as_bytes())?;

    // events, hashed as written
    let (events_file, encoding) = match opts.events {
        EventsEncoding::Jsonl => ("events.jsonl", "jsonl"),
        EventsEncoding::Json => ("events.json", "json"),
    };
    sink.begin(events_file)?;
    let mut hasher = Sha256::new();
    let mut emit = |sink: &mut dyn ArchiveSink, bytes: &[u8]| -> Result<()> {
        hasher.update(bytes);
        sink.write(bytes)
    };
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    if opts.events == EventsEncoding::Json {
        emit(sink, b"[\n")?;
    }
    for (i, e) in events.iter().enumerate() {
        let line = serde_json::to_string(e)?;
        match opts.events {
            EventsEncoding::Jsonl => {
                emit(sink, line.as_bytes())?;
                emit(sink, b"\n")?;
            }
            EventsEncoding::Json => {
                if i > 0 {
                    emit(sink, b",\n")?;
                }
                emit(sink, line.as_bytes())?;
            }
        }
        *by_kind.entry(kind_name(&e.kind)).or_default() += 1;
    }
    if opts.events == EventsEncoding::Json {
        emit(sink, b"\n]\n")?;
    }
    let events_sha = hex(&hasher.finalize());

    // media
    let mut media = Vec::new();
    let mut written: BTreeMap<String, (String, u64)> = BTreeMap::new(); // hash -> (sha256, bytes)
    for e in &events {
        if e.kind != EventKind::Capture {
            continue;
        }
        let Some(kind) = e.payload.media_kind() else { continue };
        let Some(hash) = e.blob_hash().map(str::to_string) else { continue };
        let (kname, ext, mime) = media_facts(kind);
        let path = format!("media/{hash}.{ext}");
        let mut entry = json!({ "event_id": e.event_id, "hash": hash, "kind": kname, "mime": mime });

        if redacted.contains(&e.event_id) {
            entry["status"] = "redacted".into();
            entry["reason"] = "the capture was redacted: its events are present, its bytes are left out".into();
            report.media_redacted += 1;
        } else if !opts.media {
            entry["status"] = "unavailable".into();
            entry["reason"] = "exported without media".into();
            report.media_unavailable += 1;
        } else if let Some((sha, bytes)) = written.get(&hash) {
            entry["status"] = "included".into();
            entry["path"] = path.into();
            entry["sha256"] = sha.clone().into();
            entry["bytes"] = (*bytes).into();
            report.media_included += 1;
        } else {
            match node.open_blob(&hash).await {
                Ok(mut opened) => {
                    sink.begin(&path)?;
                    let mut h = Sha256::new();
                    let mut n = 0u64;
                    while let Some(chunk) = opened.plaintext.next().await {
                        let chunk = chunk.with_context(|| format!("read media {hash}"))?;
                        h.update(&chunk);
                        n += chunk.len() as u64;
                        sink.write(&chunk)?;
                    }
                    let sha = hex(&h.finalize());
                    entry["status"] = "included".into();
                    entry["path"] = path.into();
                    entry["sha256"] = sha.clone().into();
                    entry["bytes"] = n.into();
                    written.insert(hash.clone(), (sha, n));
                    report.media_included += 1;
                    report.media_bytes += n;
                }
                Err(err) => {
                    entry["status"] = "unavailable".into();
                    entry["reason"] = match err {
                        BlobOpenError::Unreferenced => "no live capture references these bytes".to_string(),
                        BlobOpenError::Unavailable(e) => format!("this device does not hold the bytes: {e:#}"),
                    }
                    .into();
                    report.media_unavailable += 1;
                }
            }
        }
        media.push(entry);
    }

    let manifest = json!({
        "format": FORMAT,
        "format_version": FORMAT_VERSION,
        "exported_at_ms": now_ms,
        "exported_at": iso(now_ms),
        "engine": { "version": crate::VERSION, "git_sha": crate::GIT_SHA, "build_date": crate::BUILD_DATE },
        "device_id": journal.device_id(),
        "events": {
            "file": events_file,
            "encoding": encoding,
            "count": events.len(),
            "sha256": events_sha,
            "first_recorded_at": events.first().map(|e| e.recorded_at),
            "last_recorded_at": events.last().map(|e| e.recorded_at),
            "order": "recorded_at, then event_id",
            "by_kind": by_kind,
        },
        "schemas": { "event": "schema/event.schema.json", "manifest": "schema/manifest.schema.json" },
        "media": media,
        "media_summary": {
            "included": report.media_included,
            "redacted": report.media_redacted,
            "unavailable": report.media_unavailable,
        },
        "not_included": [
            "the master password and every key (media files here are decrypted; this archive is NOT encrypted)",
            "device-local settings: peer list and addresses, required/optional peer flags, retention policy, network config",
            "derived data: search index, embeddings, the markdown mirror",
        ],
    });
    sink.put("manifest.json", serde_json::to_string_pretty(&manifest)?.as_bytes())?;
    Ok(report)
}

fn kind_name(k: &EventKind) -> String {
    serde_json::to_value(k).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "unknown".into())
}

/// Convenience: write the archive into a directory.
pub async fn export_archive_dir(node: &Node, out: &Path, opts: ArchiveOpts, now_ms: i64) -> Result<ArchiveReport> {
    let mut sink = DirSink::new(out)?;
    export_archive(node, &mut sink, opts, now_ms).await
}
