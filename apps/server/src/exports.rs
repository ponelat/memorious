//! Archive downloads as **links**: `POST /api/exports` builds the lossless archive (memorious_core::export_archive) in the
//! background into `<data>/exports/<id>.zip`; the finished file is served from an unguessable, expiring URL
//! (`/export/<id>/<secret>/<name>.zip`) by tower-http's `ServeFile`, so downloads are **resumable** (HTTP Range), carry an
//! ETag, and don't depend on the app session or one long-lived request — `curl -C -`, wget and any browser's download
//! manager work. The link is a bearer secret for the whole journal in plain form: short expiry by default, revocable.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use memorious_core::export_archive::{export_archive, stamp, ArchiveOpts, ArchiveProgress, EventsEncoding, ZipSink};
use rand::TryRngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::{err, internal, SharedState};

/// At most this many finished archives are kept at once (each is the size of the journal): the oldest goes when a new one
/// is made.
const KEEP: usize = 2;
const DEFAULT_TTL_HOURS: u64 = 24;
const MAX_TTL_HOURS: u64 = 24 * 7;

#[derive(Default)]
pub struct Exports {
    /// Archives being built right now: id -> live progress.
    running: Mutex<HashMap<String, Arc<ArchiveProgress>>>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Meta {
    id: String,
    secret: String,
    /// File name offered to the downloader.
    name: String,
    /// "running" | "ready" | "failed"
    status: String,
    error: Option<String>,
    created_ms: i64,
    expires_ms: i64,
    size: u64,
    format: String,
    media: bool,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn dir(state: &SharedState) -> PathBuf {
    state.data_dir.join("exports")
}

fn meta_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn zip_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.zip"))
}

fn token(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::rngs::OsRng.try_fill_bytes(&mut b).expect("os randomness");
    data_encoding::HEXLOWER.encode(&b)
}

fn read_meta(dir: &Path, id: &str) -> Option<Meta> {
    serde_json::from_slice(&std::fs::read(meta_path(dir, id)).ok()?).ok()
}

fn write_meta(dir: &Path, m: &Meta) -> std::io::Result<()> {
    let tmp = dir.join(format!("{}.json.tmp", m.id));
    std::fs::File::create(&tmp)?.write_all(&serde_json::to_vec_pretty(m)?)?;
    std::fs::rename(tmp, meta_path(dir, &m.id))
}

fn all_meta(dir: &Path) -> Vec<Meta> {
    let mut out: Vec<Meta> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension()?.to_str()? == "json").then(|| serde_json::from_slice::<Meta>(&std::fs::read(p).ok()?).ok()).flatten()
        })
        .collect();
    out.sort_by_key(|m| std::cmp::Reverse(m.created_ms));
    out
}

fn remove(dir: &Path, id: &str) {
    let _ = std::fs::remove_file(zip_path(dir, id));
    let _ = std::fs::remove_file(dir.join(format!("{id}.zip.part")));
    let _ = std::fs::remove_file(meta_path(dir, id));
}

/// Delete what has expired, and what a restart interrupted (running on disk, nobody building it).
fn sweep(state: &SharedState) {
    let d = dir(state);
    let live = state.exports.running.lock().unwrap();
    for m in all_meta(&d) {
        let interrupted = m.status == "running" && !live.contains_key(&m.id);
        if m.expires_ms <= now_ms() || interrupted {
            remove(&d, &m.id);
        }
    }
}

fn view(state: &SharedState, m: &Meta) -> Value {
    let live = state.exports.running.lock().unwrap().get(&m.id).cloned();
    let progress = live.map(|p| {
        json!({
            "events": p.events_total.load(Ordering::Relaxed),
            "media_done": p.media_done.load(Ordering::Relaxed),
            "media_total": p.media_total.load(Ordering::Relaxed),
            "bytes": p.bytes_written.load(Ordering::Relaxed),
        })
    });
    json!({
        "id": m.id,
        "name": m.name,
        "status": m.status,
        "error": m.error,
        "created_ms": m.created_ms,
        "expires_ms": m.expires_ms,
        "size": m.size,
        "format": m.format,
        "media": m.media,
        // a path, not a URL: the browser knows its own origin
        "link": (m.status == "ready").then(|| format!("/export/{}/{}/{}", m.id, m.secret, m.name)),
        "progress": progress,
    })
}

#[derive(Deserialize)]
pub(crate) struct CreateBody {
    /// `jsonl` (default) or `json`.
    format: Option<String>,
    media: Option<bool>,
    /// How long the link lives, in hours (default 24, max 168).
    ttl_hours: Option<u64>,
}

/// `POST /api/exports` → 202 `{export: {...}}`. One build at a time (409 otherwise).
pub(crate) async fn create(State(state): State<SharedState>, Json(body): Json<CreateBody>) -> Response {
    let encoding = match body.format.as_deref() {
        None | Some("jsonl") => EventsEncoding::Jsonl,
        Some("json") => EventsEncoding::Json,
        Some(other) => return err(StatusCode::BAD_REQUEST, &format!("format must be jsonl or json, got {other:?}")),
    };
    let media = body.media.unwrap_or(true);
    let ttl = body.ttl_hours.unwrap_or(DEFAULT_TTL_HOURS).clamp(1, MAX_TTL_HOURS);
    sweep(&state);
    let d = dir(&state);
    if let Err(e) = std::fs::create_dir_all(&d) {
        return internal(e.into());
    }
    if !state.exports.running.lock().unwrap().is_empty() {
        return err(StatusCode::CONFLICT, "an archive is already being built");
    }
    // keep disk bounded: drop the oldest finished archives beyond KEEP-1 before adding one
    let ready: Vec<Meta> = all_meta(&d).into_iter().filter(|m| m.status == "ready").collect();
    for old in ready.iter().skip(KEEP - 1) {
        remove(&d, &old.id);
    }

    let now = now_ms();
    let id = token(8);
    let meta = Meta {
        id: id.clone(),
        secret: token(24),
        name: format!("memorious-archive-{}.zip", stamp(now)),
        status: "running".into(),
        error: None,
        created_ms: now,
        expires_ms: now + (ttl as i64) * 3_600_000,
        size: 0,
        format: if encoding == EventsEncoding::Json { "json" } else { "jsonl" }.into(),
        media,
    };
    if let Err(e) = write_meta(&d, &meta) {
        return internal(e.into());
    }
    let progress = Arc::new(ArchiveProgress::default());
    state.exports.running.lock().unwrap().insert(id.clone(), progress.clone());
    let out = Json(json!({ "export": view(&state, &meta) }));

    let st = state.clone();
    tokio::spawn(async move {
        let part = d.join(format!("{id}.zip.part"));
        let result: anyhow::Result<u64> = async {
            let file = std::fs::File::create(&part)?;
            let prefix = meta.name.trim_end_matches(".zip").to_string();
            let mut sink = ZipSink::new(file, &prefix);
            export_archive(&st.node, &mut sink, ArchiveOpts { events: encoding, media, progress: Some(progress) }, now).await?;
            let file = sink.finish()?;
            file.sync_all()?;
            let size = file.metadata()?.len();
            std::fs::rename(&part, zip_path(&d, &id))?;
            Ok(size)
        }
        .await;
        let mut m = read_meta(&d, &id).unwrap_or(meta);
        match result {
            Ok(size) => {
                m.status = "ready".into();
                m.size = size;
            }
            Err(e) => {
                tracing::warn!("archive export failed: {e:#}");
                let _ = std::fs::remove_file(&part);
                m.status = "failed".into();
                m.error = Some(format!("{e:#}"));
            }
        }
        let _ = write_meta(&d, &m);
        st.exports.running.lock().unwrap().remove(&id);
    });
    (StatusCode::ACCEPTED, out).into_response()
}

/// `GET /api/exports` → `{exports: [...]}`, newest first.
pub(crate) async fn list(State(state): State<SharedState>) -> Response {
    sweep(&state);
    let d = dir(&state);
    Json(json!({ "exports": all_meta(&d).iter().map(|m| view(&state, m)).collect::<Vec<_>>() })).into_response()
}

/// `DELETE /api/exports/{id}` — revokes the link and deletes the file.
pub(crate) async fn delete(State(state): State<SharedState>, UrlPath(id): UrlPath<String>) -> Response {
    if !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return err(StatusCode::BAD_REQUEST, "bad id");
    }
    if state.exports.running.lock().unwrap().contains_key(&id) {
        return err(StatusCode::CONFLICT, "still being built");
    }
    remove(&dir(&state), &id);
    Json(json!({ "ok": true })).into_response()
}

fn same(a: &str, b: &str) -> bool {
    // constant-time-ish compare; the secret is random so this is belt and braces
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `GET /export/{id}/{secret}/{name}` — public, secret-gated, resumable (Range) file download. Anything wrong is a plain 404
/// so the URL space can't be probed.
pub async fn download(
    State(state): State<SharedState>,
    UrlPath((id, secret, _name)): UrlPath<(String, String, String)>,
    req: Request<Body>,
) -> Response {
    let nf = || err(StatusCode::NOT_FOUND, "not found");
    if !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return nf();
    }
    sweep(&state);
    let d = dir(&state);
    let Some(m) = read_meta(&d, &id) else { return nf() };
    if m.status != "ready" || !same(&m.secret, &secret) || m.expires_ms <= now_ms() {
        return nf();
    }
    let Ok(resp) = tower_http::services::ServeFile::new(zip_path(&d, &id)).oneshot(req).await;
    let mut resp = resp.map(Body::new);
    let h = resp.headers_mut();
    h.insert(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", m.name).parse().unwrap());
    h.insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    h.insert("x-robots-tag", "noindex, nofollow".parse().unwrap());
    resp.into_response()
}
