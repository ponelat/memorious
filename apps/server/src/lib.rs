//! HTTP face of the always-on server peer. Browsers are thin clients of this API;
//! real peers sync with the embedded Node over iroh, not over HTTP.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use memorious_core::api_json::entry_json;
use memorious_core::event::{EventKind, MediaKind, Payload};
use memorious_core::media::{normalize_photo, sniff_audio, AudioContainer};
use memorious_core::Node;
use serde::{Deserialize, Serialize};

pub mod sweeper;
use serde_json::json;

pub struct AppState {
    pub node: Node,
    /// Directory of installable app builds served at /downloads (public — it
    /// holds software, never journal data).
    pub downloads_dir: Option<PathBuf>,
}

impl AppState {
    fn journal(&self) -> &memorious_core::Journal {
        self.node.journal()
    }
}

pub type SharedState = Arc<AppState>;

/// Build the full router: /api under bearer auth, static web UI for everything else.
pub fn app(state: SharedState, web_dist: Option<PathBuf>) -> Router {
    let api = Router::new()
        .route("/capture/text", post(capture_text))
        .route("/capture/photo", post(capture_photo))
        .route("/capture/audio", post(capture_audio))
        .route("/feed", get(feed))
        .route("/media/{hash}", get(media))
        .route("/redact", post(redact))
        .route("/trash", get(trash))
        .route("/search", get(search))
        .route("/status", get(status))
        .route("/downloads", get(downloads_list))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .route("/auth/check", post(auth_check))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(state.clone());

    let mut app = Router::new().nest("/api", api);
    if let Some(dir) = &state.downloads_dir {
        app = app.nest_service("/downloads", tower_http::services::ServeDir::new(dir));
    }
    if let Some(dist) = web_dist {
        let index = dist.join("index.html");
        app = app.fallback_service(
            tower_http::services::ServeDir::new(&dist)
                .fallback(tower_http::services::ServeFile::new(index)),
        );
    }
    app
}

/// Available app builds: name, size, and the public URL to fetch each one.
async fn downloads_list(State(state): State<SharedState>) -> Response {
    let Some(dir) = &state.downloads_dir else {
        return Json(json!({"files": []})).into_response();
    };
    let mut files = Vec::new();
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || !entry.path().is_file() {
                    continue;
                }
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                files.push(json!({
                    "name": name,
                    "size": size,
                    "url": format!("/downloads/{name}"),
                }));
            }
        }
        Err(e) => return internal(e.into()),
    }
    files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Json(json!({"files": files})).into_response()
}

// ---- auth ----

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(|s| s.trim().to_string())
}

async fn require_auth(
    State(state): State<SharedState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let ok = bearer(req.headers())
        .map(|token| state.journal().check_passcode(&token).unwrap_or(false))
        .unwrap_or(false);
    if !ok {
        return err(StatusCode::UNAUTHORIZED, "invalid or missing passcode");
    }
    next.run(req).await
}

#[derive(Deserialize)]
struct AuthCheck {
    passcode: String,
}

async fn auth_check(
    State(state): State<SharedState>,
    Json(body): Json<AuthCheck>,
) -> Response {
    match state.journal().check_passcode(&body.passcode) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(StatusCode::UNAUTHORIZED, "wrong passcode (or none set yet)"),
        Err(e) => internal(e),
    }
}

// ---- capture ----

#[derive(Deserialize)]
struct CaptureText {
    text: String,
}

async fn capture_text(
    State(state): State<SharedState>,
    Json(body): Json<CaptureText>,
) -> Response {
    if body.text.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty text");
    }
    match state.journal().capture_text(&body.text) {
        Ok(e) => Json(entry_json(&e)).into_response(),
        Err(e) => internal(e),
    }
}

async fn read_upload(mut multipart: Multipart) -> Result<Vec<u8>> {
    while let Some(field) = multipart.next_field().await? {
        if field.name() == Some("file") || field.file_name().is_some() {
            return Ok(field.bytes().await?.to_vec());
        }
    }
    anyhow::bail!("no file field in upload");
}

async fn capture_photo(State(state): State<SharedState>, multipart: Multipart) -> Response {
    let bytes = match read_upload(multipart).await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    };
    let jpeg = match tokio::task::spawn_blocking(move || normalize_photo(&bytes)).await {
        Ok(Ok(j)) => j,
        Ok(Err(e)) => return err(StatusCode::UNPROCESSABLE_ENTITY, &format!("{e:#}")),
        Err(e) => return internal(e.into()),
    };
    match state.node.capture_blob_with_intent(MediaKind::Photo, jpeg, true).await {
        Ok(e) => Json(entry_json(&e)).into_response(),
        Err(e) => internal(e),
    }
}

async fn capture_audio(State(state): State<SharedState>, multipart: Multipart) -> Response {
    let bytes = match read_upload(multipart).await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    };
    let m4a = match sniff_audio(&bytes) {
        AudioContainer::Mp4 => bytes,
        AudioContainer::Webm | AudioContainer::Ogg => {
            match transcode_to_m4a(bytes).await {
                Ok(b) => b,
                Err(e) => return err(StatusCode::UNPROCESSABLE_ENTITY, &format!("transcode: {e:#}")),
            }
        }
        AudioContainer::Unknown => {
            return err(StatusCode::UNPROCESSABLE_ENTITY, "unrecognized audio container")
        }
    };
    match state.node.capture_blob_with_intent(MediaKind::Audio, m4a, true).await {
        Ok(e) => Json(entry_json(&e)).into_response(),
        Err(e) => internal(e),
    }
}

/// Browser MediaRecorder often yields webm/opus (Chrome) — one stored format means
/// transcoding to AAC/m4a here, via the system ffmpeg.
async fn transcode_to_m4a(input: Vec<u8>) -> Result<Vec<u8>> {
    let dir = tempfile::tempdir()?;
    let in_path = dir.path().join("in");
    let out_path = dir.path().join("out.m4a");
    tokio::fs::write(&in_path, &input).await?;
    let output = tokio::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(&in_path)
        .args(["-vn", "-c:a", "aac", "-b:a", "96k"])
        .arg(&out_path)
        .output()
        .await
        .context("run ffmpeg (is it installed?)")?;
    if !output.status.success() {
        anyhow::bail!("ffmpeg failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(tokio::fs::read(&out_path).await?)
}

// ---- reading ----

#[derive(Deserialize)]
struct FeedParams {
    /// Return entries strictly older than this recorded_at (ms).
    before: Option<i64>,
    limit: Option<usize>,
}


fn annotated_entry(journal: &memorious_core::Journal, e: &memorious_core::Event) -> serde_json::Value {
    let mut v = entry_json(e);
    if let Ok(map) = journal.annotations() {
        if let Some(text) = map.get(&e.event_id) {
            if !text.is_empty() {
                v["annotation"] = text.clone().into();
            }
        }
    }
    v
}

async fn feed(State(state): State<SharedState>, Query(p): Query<FeedParams>) -> Response {
    let limit = p.limit.unwrap_or(50).min(500);
    let annotations = state.journal().annotations().unwrap_or_default();
    match state.journal().list() {
        Ok(mut entries) => {
            entries.reverse(); // list() is oldest-first
            let page: Vec<_> = entries
                .iter()
                .filter(|e| p.before.map(|b| e.recorded_at < b).unwrap_or(true))
                .take(limit)
                .map(|e| {
                    let mut v = entry_json(e);
                    if let Some(text) = annotations.get(&e.event_id) {
                        if !text.is_empty() {
                            v["annotation"] = text.clone().into();
                        }
                    }
                    v
                })
                .collect();
            let next_before = page.last().and_then(|e| e["recorded_at"].as_i64());
            Json(json!({"entries": page, "next_before": next_before})).into_response()
        }
        Err(e) => internal(e),
    }
}

async fn media(State(state): State<SharedState>, UrlPath(hash): UrlPath<String>) -> Response {
    // Content type comes from which capture references the hash.
    let kind = match media_kind_for_hash(&state, &hash) {
        Ok(Some(k)) => k,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no entry references this media"),
        Err(e) => return internal(e),
    };
    match state.node.blob_bytes(&hash).await {
        Ok(bytes) => {
            let content_type = match kind {
                MediaKind::Photo => "image/jpeg",
                MediaKind::Audio => "audio/mp4",
            };
            (
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::CACHE_CONTROL, "private, max-age=31536000, immutable"),
                ],
                bytes,
            )
                .into_response()
        }
        Err(e) => err(StatusCode::NOT_FOUND, &format!("blob unavailable: {e:#}")),
    }
}

fn media_kind_for_hash(state: &AppState, hash: &str) -> Result<Option<MediaKind>> {
    for e in state.journal().store.all_events()? {
        match &e.payload {
            Payload::Photo { hash: h, .. } if h == hash => return Ok(Some(MediaKind::Photo)),
            Payload::Audio { hash: h, .. } if h == hash => return Ok(Some(MediaKind::Audio)),
            _ => {}
        }
    }
    Ok(None)
}

#[derive(Deserialize)]
struct RedactBody {
    event_id: String,
}

async fn redact(State(state): State<SharedState>, Json(body): Json<RedactBody>) -> Response {
    match state.journal().redact(&body.event_id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    }
}

async fn trash(State(state): State<SharedState>) -> Response {
    match state.journal().trash() {
        Ok(entries) => {
            let mut entries = entries;
            entries.reverse();
            Json(json!({"entries": entries.iter().map(entry_json).collect::<Vec<_>>()}))
                .into_response()
        }
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
}

async fn search(State(state): State<SharedState>, Query(p): Query<SearchParams>) -> Response {
    let journal = state.journal();
    let run = || -> Result<Vec<serde_json::Value>> {
        let redacted = journal.store.redacted_ids()?;
        let mut out = Vec::new();
        for id in journal.store.search(&p.q)? {
            if let Some(e) = journal.store.get_event(&id)? {
                // Annotations surface as their target entry (M5 wires this fully).
                let display = match &e.payload {
                    Payload::Annotation { target, .. } => journal.store.get_event(target)?,
                    _ => Some(e),
                };
                if let Some(e) = display {
                    if e.kind == EventKind::Capture && !redacted.contains(&e.event_id) {
                        out.push(annotated_entry(journal, &e));
                    }
                }
            }
        }
        Ok(out)
    };
    match run() {
        Ok(entries) => Json(json!({"entries": entries})).into_response(),
        Err(e) => internal(e),
    }
}

async fn status(State(state): State<SharedState>) -> Response {
    let journal = state.journal();
    let run = || -> Result<serde_json::Value> {
        Ok(json!({
            "device_id": journal.device_id(),
            "entries": journal.list()?.len(),
            "trash": journal.trash()?.len(),
            "heads": journal.store.heads()?,
        }))
    };
    match run() {
        Ok(mut v) => {
            if let Ok(t) = state.node.ticket() {
                v["ticket"] = t.into();
            }
            Json(v).into_response()
        }
        Err(e) => internal(e),
    }
}

// ---- helpers ----

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({"error": msg}))).into_response()
}

fn internal(e: anyhow::Error) -> Response {
    tracing::error!("internal error: {e:#}");
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

#[derive(Serialize)]
pub struct Never {}
