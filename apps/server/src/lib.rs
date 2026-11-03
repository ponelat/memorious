//! HTTP face of the always-on server peer. Browsers are thin clients of this API;
//! real peers sync with the embedded Node over iroh, not over HTTP.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, Multipart, Path as UrlPath, Query, State};
use axum::http::HeaderValue;
use tower_http::set_header::SetResponseHeaderLayer;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use memorious_core::api_json::entry_json;
use memorious_core::event::{AudioKind, MediaKind};
use memorious_core::media::{is_mp4_family, normalize_photo, sniff_audio, AudioContainer};
use memorious_core::node::BlobOpenError;
use memorious_core::{Journal, Node};
use serde::{Deserialize, Serialize};

pub mod mcp;
pub mod oauth;
pub mod embed_sweeper;
pub mod peer_ping;
pub mod sweeper;
use serde_json::json;

/// The router's actual axum state: a swappable pointer to the current
/// `AppState`, so a reset or a browser-driven re-pair can atomically replace
/// the running server's `Node` — new data dir, new identity, everything —
/// without restarting the process (the old exit-and-wait-for-an-operator
/// design was the reason a reset server couldn't recover itself; see
/// `docs/DEPLOY.md`). Only the handlers that swap it (`reset_device`,
/// `setup_join`) ask for `State<RouterState>` directly; every other handler
/// keeps asking for `State<SharedState>` — an `Arc<AppState>` resolved fresh
/// per request via the `FromRef` impl below — completely unchanged from
/// before this existed.
/// A newtype, not a plain type alias, so `impl FromRef` below is legal
/// (axum's trait + `Arc<...>` are both foreign to this crate — the orphan
/// rules need at least one local type in the `impl`).
#[derive(Clone)]
pub struct RouterState(pub Arc<std::sync::RwLock<Arc<AppState>>>);

/// What an ordinary handler extracts: today's `AppState`, snapshotted at the
/// start of the request. A swap mid-request finishes that request against
/// the old snapshot and the next request sees the new one — never a torn
/// read across the two.
pub type SharedState = Arc<AppState>;

impl axum::extract::FromRef<RouterState> for SharedState {
    fn from_ref(input: &RouterState) -> Self {
        input.0.read().unwrap().clone()
    }
}

pub struct AppState {
    pub node: Node,
    /// Directory of installable app builds served at /downloads (public — it
    /// holds software, never journal data).
    pub downloads_dir: Option<PathBuf>,
    /// Failed-passcode accounting; the default limits unless a test says otherwise.
    pub auth: AuthGuard,
    /// This peer's data directory (`MEMORIOUS_DATA`) — needed only by
    /// `reset_device`, to delete it after shutting the node down.
    pub data_dir: PathBuf,
    /// Pending OAuth authorization codes for the MCP endpoint (in memory, 60s).
    pub oauth_codes: oauth::Codes,
    /// Semantic-search model, on peers configured to run one. Set once at
    /// startup (`with_embedder`); `None` means keyword search only.
    pub embedder: Option<Arc<dyn memorious_core::search::Embedder>>,
}

impl AppState {
    pub fn new(node: Node, downloads_dir: Option<PathBuf>, data_dir: PathBuf) -> Self {
        Self {
            node,
            downloads_dir,
            auth: AuthGuard::new(AuthLimits::default()),
            data_dir,
            oauth_codes: Default::default(),
            embedder: None,
        }
    }

    pub fn with_embedder(mut self, embedder: Arc<dyn memorious_core::search::Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    fn journal(&self) -> &memorious_core::Journal {
        self.node.journal()
    }
}

/// How many wrong passcodes a client gets before it waits out the window, and
/// the pause every miss costs. The pause is serialized across all clients, so
/// rotating addresses buys nothing: at most one guess per `failure_delay`,
/// from anywhere.
#[derive(Clone, Copy, Debug)]
pub struct AuthLimits {
    pub max_failures: u32,
    pub window: Duration,
    pub failure_delay: Duration,
}

impl Default for AuthLimits {
    fn default() -> Self {
        Self {
            max_failures: 10,
            window: Duration::from_secs(15 * 60),
            failure_delay: Duration::from_millis(250),
        }
    }
}

struct Failures {
    count: u32,
    since: Instant,
}

/// Per-client failed-attempt accounting for the browser passcode. The passcode
/// is the only thing between the internet and the journal, and it may be
/// short, so guessing has to be slow: a client that misses `max_failures`
/// times inside `window` is refused (429, Retry-After) until the window
/// passes; a correct passcode clears its count.
pub struct AuthGuard {
    limits: AuthLimits,
    failures: StdMutex<HashMap<String, Failures>>,
    delay: tokio::sync::Mutex<()>,
}

impl AuthGuard {
    pub fn new(limits: AuthLimits) -> Self {
        Self {
            limits,
            failures: StdMutex::new(HashMap::new()),
            delay: tokio::sync::Mutex::new(()),
        }
    }

    /// Seconds the client still has to wait, if it is locked out.
    pub(crate) fn locked_for(&self, client: &str) -> Option<u64> {
        let mut map = self.failures.lock().unwrap();
        let expired = matches!(map.get(client), Some(f) if f.since.elapsed() >= self.limits.window);
        if expired {
            map.remove(client);
            return None;
        }
        match map.get(client) {
            Some(f) if f.count >= self.limits.max_failures => {
                Some((self.limits.window - f.since.elapsed()).as_secs().max(1))
            }
            _ => None,
        }
    }

    /// Count a miss, then hold the global gate for the delay.
    pub(crate) async fn failed(&self, client: &str) {
        {
            let mut map = self.failures.lock().unwrap();
            let now = Instant::now();
            let window = self.limits.window;
            let f = map
                .entry(client.to_string())
                .or_insert(Failures { count: 0, since: now });
            if now.duration_since(f.since) >= window {
                *f = Failures { count: 0, since: now };
            }
            f.count += 1;
            if map.len() > 10_000 {
                map.retain(|_, f| f.since.elapsed() < window);
            }
        }
        if !self.limits.failure_delay.is_zero() {
            let _gate = self.delay.lock().await;
            tokio::time::sleep(self.limits.failure_delay).await;
        }
    }

    pub(crate) fn succeeded(&self, client: &str) {
        self.failures.lock().unwrap().remove(client);
    }
}

/// The client behind Caddy. The listener is loopback-only, so X-Forwarded-For
/// is always Caddy's own header and can be trusted; without it (tests, a
/// direct local curl) everything is one client.
pub(crate) fn client_key(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local".into())
}

/// Browser hardening on every response: only our own bundle runs, media and
/// photos come as blobs we fetched ourselves, nothing frames us.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
img-src 'self' blob: data:; media-src 'self' blob:; connect-src 'self'; font-src 'self'; \
object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'";

/// Build the full router: /api under bearer auth, static web UI for everything else.
pub fn app(state: RouterState, web_dist: Option<PathBuf>) -> Router {
    let authed = Router::new()
        .route("/capture/text", post(capture_text))
        .route("/capture/photo", post(capture_photo))
        .route("/capture/audio", post(capture_audio))
        .route("/capture/video", post(capture_video))
        .route("/feed", get(feed))
        .route("/media/{hash}", get(media))
        .route("/redact", post(redact))
        .route("/trash", get(trash))
        .route("/search", get(search))
        .route("/search/related", get(search_related))
        .route("/status", get(status))
        .route("/peers/ping", post(ping_peers))
        .route("/device-name", post(set_device_name))
        .route("/agents", get(list_agents).post(mint_agent))
        .route("/agents/{id}", axum::routing::delete(revoke_agent))
        .route("/net-config", post(set_net_config))
        .route("/passcode", post(change_passcode))
        .route("/master-password/rotate", post(rotate_master_password))
        .route("/master-password/adopt", post(adopt_master_password))
        .route("/reset", post(reset_device))
        .route("/downloads", get(downloads_list))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024));
    // Unauthenticated: checking a passcode obviously can't require one, and
    // setup/state|passcode|join have their own guard (`ensure_unprovisioned`)
    // that refuses once a passcode exists — see their doc comments.
    let open = Router::new()
        .route("/auth/check", post(auth_check))
        .route("/setup/state", get(setup_state))
        .route("/setup/passcode", post(setup_passcode))
        .route("/setup/join", post(setup_join))
        .layer(DefaultBodyLimit::max(4 * 1024));
    let api = authed.merge(open).with_state(state.clone());

    let mut app = Router::new()
        .nest("/api", api)
        .route("/mcp", post(mcp::endpoint).get(mcp::not_allowed))
        .route("/.well-known/oauth-protected-resource", get(oauth::protected_resource))
        .route("/.well-known/oauth-protected-resource/mcp", get(oauth::protected_resource))
        .route("/.well-known/oauth-authorization-server", get(oauth::server_metadata))
        .route("/oauth/register", post(oauth::register).layer(DefaultBodyLimit::max(8 * 1024)))
        .route("/oauth/authorize", get(oauth::authorize_page).post(oauth::authorize_submit))
        .route("/oauth/token", post(oauth::token))
        .with_state(state.clone());
    if let Some(dir) = &state.0.read().unwrap().downloads_dir {
        app = app.nest_service("/downloads", tower_http::services::ServeDir::new(dir));
    }
    if let Some(dist) = web_dist {
        let index = dist.join("index.html");
        app = app.fallback_service(
            tower_http::services::ServeDir::new(&dist)
                .fallback(tower_http::services::ServeFile::new(index)),
        );
    }
    app.layer(SetResponseHeaderLayer::if_not_present(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    ))
    .layer(SetResponseHeaderLayer::if_not_present(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    ))
    .layer(SetResponseHeaderLayer::if_not_present(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("DENY"),
    ))
    .layer(SetResponseHeaderLayer::if_not_present(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    ))
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
    let client = client_key(req.headers());
    if let Some(secs) = state.auth.locked_for(&client) {
        return too_many(secs);
    }
    match bearer(req.headers()) {
        None => return err(StatusCode::UNAUTHORIZED, "invalid or missing passcode"),
        Some(token) if state.journal().check_passcode(&token).unwrap_or(false) => {
            state.auth.succeeded(&client);
        }
        Some(_) => {
            state.auth.failed(&client).await;
            return err(StatusCode::UNAUTHORIZED, "invalid or missing passcode");
        }
    }
    next.run(req).await
}

pub(crate) fn too_many(retry_after_secs: u64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, retry_after_secs.to_string())],
        Json(json!({"error": "too many wrong passcodes — try again later"})),
    )
        .into_response()
}

#[derive(Deserialize)]
struct AuthCheck {
    passcode: String,
}

async fn auth_check(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<AuthCheck>,
) -> Response {
    let client = client_key(&headers);
    if let Some(secs) = state.auth.locked_for(&client) {
        return too_many(secs);
    }
    match state.journal().check_passcode(&body.passcode) {
        Ok(true) => {
            state.auth.succeeded(&client);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => {
            state.auth.failed(&client).await;
            err(StatusCode::UNAUTHORIZED, "wrong passcode (or none set yet)")
        }
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

#[derive(Deserialize)]
struct AudioParams {
    /// "voice" (default) | "music" — decided at capture, see
    /// crates/core/src/retention.rs.
    kind: Option<String>,
}

async fn capture_audio(
    State(state): State<SharedState>,
    Query(p): Query<AudioParams>,
    multipart: Multipart,
) -> Response {
    let audio_kind = match p.kind.as_deref() {
        None => AudioKind::Voice,
        Some(k) => match AudioKind::parse(k) {
            Some(k) => k,
            None => return err(StatusCode::BAD_REQUEST, "kind must be voice or music"),
        },
    };
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
    match state.node.capture_audio_with_intent(m4a, audio_kind, true).await {
        Ok(e) => Json(entry_json(&e)).into_response(),
        Err(e) => internal(e),
    }
}

/// Probe every known peer: reachable = the ordinary sync handshake answered
/// (crates/core: Node::ping_peers). Cheap when converged; heals when behind.
async fn ping_peers(State(state): State<SharedState>) -> Response {
    match state.node.ping_peers(std::time::Duration::from_secs(4)).await {
        Ok(pings) => Json(json!({ "pings": pings })).into_response(),
        Err(e) => internal(e),
    }
}

async fn capture_video(State(state): State<SharedState>, multipart: Multipart) -> Response {
    let bytes = match read_upload(multipart).await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    };
    // Canonical video is H.264/AAC MP4. Browsers paste/drag mp4-family files
    // (mp4/mov) straight through; webm screen-recordings get transcoded.
    let mp4 = if is_mp4_family(&bytes) {
        bytes
    } else if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        match transcode_to_mp4_video(bytes).await {
            Ok(b) => b,
            Err(e) => return err(StatusCode::UNPROCESSABLE_ENTITY, &format!("transcode: {e:#}")),
        }
    } else {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "unrecognized video container");
    };
    // No enrichment engine for video: capture without the will-enrich flag so
    // peers don't hold a grace period for an annotation that never comes.
    match state.node.capture_blob(MediaKind::Video, mp4).await {
        Ok(e) => Json(entry_json(&e)).into_response(),
        Err(e) => internal(e),
    }
}

/// A tempdir for plaintext media passing through subprocess pipelines
/// (ffmpeg/whisper/tesseract): 0700, and every file is zero-overwritten
/// before removal so plaintext doesn't linger on disk. Best-effort — the
/// journal's own stores are encrypted; this covers the scratch space.
pub(crate) struct ScrubDir(tempfile::TempDir);

impl ScrubDir {
    pub(crate) fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self(dir))
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        self.0.path()
    }
}

impl Drop for ScrubDir {
    fn drop(&mut self) {
        let Ok(entries) = std::fs::read_dir(self.0.path()) else { return };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(entry.path()) {
                use std::io::Write;
                let zeros = [0u8; 8192];
                let mut remaining = meta.len();
                while remaining > 0 {
                    let n = remaining.min(zeros.len() as u64) as usize;
                    if f.write_all(&zeros[..n]).is_err() {
                        break;
                    }
                    remaining -= n as u64;
                }
                let _ = f.sync_all();
            }
        }
    }
}

/// Browser MediaRecorder often yields webm/opus (Chrome) — one stored format means
/// transcoding to AAC/m4a here, via the system ffmpeg.
async fn transcode_to_m4a(input: Vec<u8>) -> Result<Vec<u8>> {
    let dir = ScrubDir::new()?;
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

/// Same idea for video: webm (Chrome screen/tab recordings) → H.264/AAC MP4.
async fn transcode_to_mp4_video(input: Vec<u8>) -> Result<Vec<u8>> {
    let dir = ScrubDir::new()?;
    let in_path = dir.path().join("in");
    let out_path = dir.path().join("out.mp4");
    tokio::fs::write(&in_path, &input).await?;
    let output = tokio::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(&in_path)
        .args(["-c:v", "libx264", "-preset", "fast", "-c:a", "aac", "-b:a", "96k", "-movflags", "+faststart"])
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
    // One decrypted chunk in flight, not the whole file. The feed fires a
    // GET per mounted photo/audio/video; buffering each one is what crossed
    // the 192MB cgroup.
    let opened = match state.node.open_blob(&hash).await {
        Ok(opened) => opened,
        Err(BlobOpenError::Unreferenced) => {
            return err(StatusCode::NOT_FOUND, "no entry references this media");
        }
        Err(BlobOpenError::Unavailable(e)) => {
            return err(StatusCode::NOT_FOUND, &format!("blob unavailable: {e:#}"));
        }
    };
    let content_type = match opened.kind {
        MediaKind::Photo => "image/jpeg",
        MediaKind::Audio => "audio/mp4",
        MediaKind::Video => "video/mp4",
    };
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, opened.len.to_string())
        .header(header::CACHE_CONTROL, "private, max-age=31536000, immutable")
        .body(Body::from_stream(opened.plaintext))
        .expect("media response")
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
    let st = state.clone();
    let q = p.q.clone();
    let res = tokio::task::spawn_blocking(move || {
        search_entries(st.journal(), st.embedder.as_deref(), &q, &q, usize::MAX)
    })
    .await;
    match res.unwrap_or_else(|e| Err(e.into())) {
        Ok(entries) => Json(json!({"entries": entries})).into_response(),
        Err(e) => internal(e),
    }
}

/// Words from the journal near `q` in meaning. Empty on peers with no embedder.
async fn search_related(State(state): State<SharedState>, Query(p): Query<SearchParams>) -> Response {
    let st = state.clone();
    let res = tokio::task::spawn_blocking(move || match st.embedder.as_deref() {
        Some(e) => memorious_core::search::related_terms(&st.journal().store, e, &p.q, 8),
        None => Ok(Vec::new()),
    })
    .await;
    match res.unwrap_or_else(|e| Err(e.into())) {
        Ok(w) => Json(json!({"related": w.into_iter().map(|(word, similarity)| json!({"word": word, "similarity": similarity})).collect::<Vec<_>>()})).into_response(),
        Err(e) => internal(e),
    }
}

use memorious_core::mcp::search_entries;

async fn status(State(state): State<SharedState>) -> Response {
    match state.node.status_json().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct DeviceNameBody {
    device_id: String,
    name: String,
}

#[derive(Deserialize)]
struct AgentBody {
    name: String,
}

/// Mint an MCP-over-QUIC agent ticket. The only place the key is ever shown.
async fn mint_agent(State(state): State<SharedState>, Json(body): Json<AgentBody>) -> Response {
    let name = body.name.trim();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, "name must not be empty");
    }
    match state.node.mint_agent_ticket(name).await {
        Ok(ticket) => Json(json!({"ticket": ticket})).into_response(),
        Err(e) => internal(e),
    }
}

async fn list_agents(State(state): State<SharedState>) -> Response {
    match state.node.agent_keys() {
        Ok(keys) => Json(keys).into_response(),
        Err(e) => internal(e),
    }
}

async fn revoke_agent(State(state): State<SharedState>, UrlPath(id): UrlPath<String>) -> Response {
    match state.node.revoke_agent_key(&id) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => internal(e),
    }
}

async fn set_device_name(
    State(state): State<SharedState>,
    Json(body): Json<DeviceNameBody>,
) -> Response {
    match state.journal().set_device_name(&body.device_id, &body.name) {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    }
}

/// Store the (per-device) network config. Applied on the next restart —
/// the running endpoint keeps its current relays.
async fn set_net_config(
    State(state): State<SharedState>,
    Json(cfg): Json<memorious_core::node::NetConfig>,
) -> Response {
    match state.journal().set_net_config(&cfg) {
        Ok(()) => Json(json!({"ok": true, "applies": "on restart"})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct PasscodeBody {
    passcode: String,
}

/// Change the browser passcode — a token-set event, "latest wins" across
/// every synced peer (`Journal::set_passcode`), same as when it's set for
/// the first time by `setup_passcode`. Deliberately not the same mechanism
/// as `rotate_master_password`: the passcode doesn't wrap or encrypt
/// anything, so there's nothing for other devices to "adopt" — they just
/// pick up the new value on their next sync, same as a device rename.
async fn change_passcode(State(state): State<SharedState>, Json(body): Json<PasscodeBody>) -> Response {
    // MCP tokens are bound to the passcode hash in force when issued, so this
    // (and a passcode set on any synced peer) signs every MCP client out.
    match state.journal().set_passcode(&body.passcode) {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct MasterPasswordBody {
    password: String,
}

/// Originate a master-password change on this server. Re-keys its local
/// database immediately and moves the password proof forward for every
/// other peer to verify against — but the *next restart* still reads
/// `MEMORIOUS_PASSWORD` from the environment, so the deployed secret must
/// be updated too, or startup will fail to open the now-rekeyed database.
async fn rotate_master_password(
    State(state): State<SharedState>,
    Json(body): Json<MasterPasswordBody>,
) -> Response {
    match state.journal().rotate_master_password(&body.password) {
        Ok(()) => Json(json!({
            "ok": true,
            "note": "restart requires updating MEMORIOUS_PASSWORD to the new password",
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    }
}

/// Catch up to a rotation another device already published — verified
/// against that device's password proof, so a wrong guess is rejected. Same
/// restart caveat as `rotate_master_password`.
async fn adopt_master_password(
    State(state): State<SharedState>,
    Json(body): Json<MasterPasswordBody>,
) -> Response {
    match state.journal().adopt_master_password(&body.password) {
        Ok(()) => Json(json!({
            "ok": true,
            "note": "restart requires updating MEMORIOUS_PASSWORD to the new password",
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    }
}

/// Shut the node down, delete this peer's data directory, and open a fresh
/// empty journal in its place under the `MEMORIOUS_PASSWORD` this process
/// was started with. `pub` and returning the new `Node` (rather than just
/// wiping) is what lets `reset_device` hot-swap instead of exiting — see
/// its doc comment for why that matters. Unit-tested directly in
/// `tests/api.rs`.
pub async fn reset_journal(state: &AppState) -> Result<Node> {
    state.node.shutdown_ref().await;
    std::fs::remove_dir_all(&state.data_dir).context("delete data dir")?;
    let password = std::env::var("MEMORIOUS_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty())
        .context("MEMORIOUS_PASSWORD not set in this process's environment")?;
    let journal = Journal::init(&state.data_dir, &password)?;
    journal.ensure_device_name("web")?;
    journal.ensure_peer_join()?;
    journal.ensure_version_seen()?;
    Node::spawn(journal).await
}

/// Delete this peer's own copy of the journal and swap in a fresh empty one
/// — every other peer keeps its copy, nothing else on this box restarts.
/// The first version of this exited the process instead (LOG.md
/// 2026-09-18) on the theory that reusing `main.rs`'s startup path beat
/// teaching every handler and background task to tolerate a swapped-out
/// `Node`. That theory didn't survive contact with reality: the Mac dev
/// app doesn't restart itself (devhost's `availability.restart` is off by
/// design) and has no ticket to rejoin with even if it did, so a real
/// reset left the founding device dead until someone ran the CLI by hand
/// to rejoin it — see the incident note in LOG.md. `RouterState` (above)
/// exists to fix exactly this: the browser that clicked reset lands
/// straight on an empty, ready-for-setup journal instead of a dead server.
async fn reset_device(State(router_state): State<RouterState>) -> Response {
    let current = router_state.0.read().unwrap().clone();
    let node = match reset_journal(&current).await {
        Ok(node) => node,
        Err(e) => return internal(e),
    };
    let mut new_state = AppState::new(node, current.downloads_dir.clone(), current.data_dir.clone());
    new_state.embedder = current.embedder.clone(); // the opt-in is per process, not per journal
    let new_state = Arc::new(new_state);
    *router_state.0.write().unwrap() = new_state;
    Json(json!({
        "ok": true,
        "note": "journal deleted — this server now has a fresh empty one; use \
                 setup to pick a passcode or join an existing journal",
    }))
    .into_response()
}

// ---- setup: only reachable while this peer has never had a passcode ----

/// True exactly in the window where `setup/passcode` and `setup/join` are
/// allowed to run: no passcode has ever been set on this peer. This is what
/// makes it safe for both routes to sit in the *unauthenticated* group —
/// before a passcode exists there's nothing here to protect (an empty
/// journal, or one about to be replaced by whatever the ticket points at:
/// same trust model as pairing anywhere else in this app, possession of the
/// ticket + the master password is what authorizes it). The instant a
/// passcode exists, both refuse — same closed door as every other route.
fn ensure_unprovisioned(state: &AppState) -> std::result::Result<(), Response> {
    match state.journal().active_passcode_hash() {
        Ok(None) => Ok(()),
        Ok(Some(_)) => Err(err(StatusCode::FORBIDDEN, "already set up — log in normally")),
        Err(e) => Err(internal(e)),
    }
}

#[derive(Serialize)]
struct SetupStateBody {
    state: &'static str,
}

/// "empty" (no passcode yet — offer setup) or "ready" (normal login).
async fn setup_state(State(state): State<SharedState>) -> Response {
    match state.journal().active_passcode_hash() {
        Ok(None) => Json(SetupStateBody { state: "empty" }).into_response(),
        Ok(Some(_)) => Json(SetupStateBody { state: "ready" }).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct SetupPasscodeBody {
    passcode: String,
}

/// Start using the journal that's already here — freshly created at boot,
/// or left behind by a reset — by picking a passcode for it. There's no
/// separate "create" step: `main.rs` already creates an empty journal under
/// `MEMORIOUS_DATA`/`MEMORIOUS_PASSWORD` whenever none exists; this just
/// unlocks browser access to it.
async fn setup_passcode(State(state): State<SharedState>, Json(body): Json<SetupPasscodeBody>) -> Response {
    if let Err(resp) = ensure_unprovisioned(&state) {
        return resp;
    }
    match state.journal().set_passcode(&body.passcode) {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct SetupJoinBody {
    ticket: String,
    /// Set explicitly right after joining rather than left to arrive with
    /// the rest of the synced log — same belt-and-braces reasoning as the
    /// EC2 peers' compose entrypoint (`docs/DEPLOY.md`): this device is
    /// usable immediately instead of waiting on one specific event.
    passcode: String,
}

/// Replace this peer's empty/orphaned journal with a synced copy of an
/// existing one — the browser-driven equivalent of `memorious --data <dir>
/// join <ticket>`. Swaps a brand new `AppState` (new `Node`, new identity)
/// into the running server; a request already in flight against the old
/// journal finishes against it, the next one sees the new one. Concurrent
/// captures against the peer being replaced mid-join aren't guarded against
/// beyond that — acceptable because this only ever runs against an empty,
/// just-reset journal nobody is using yet.
async fn setup_join(State(router_state): State<RouterState>, Json(body): Json<SetupJoinBody>) -> Response {
    let current = router_state.0.read().unwrap().clone();
    if let Err(resp) = ensure_unprovisioned(&current) {
        return resp;
    }
    let password = match std::env::var("MEMORIOUS_PASSWORD").ok().filter(|p| !p.is_empty()) {
        Some(p) => p,
        None => return internal(anyhow::anyhow!("MEMORIOUS_PASSWORD not set in this process's environment")),
    };
    current.node.shutdown_ref().await;
    if let Err(e) = std::fs::remove_dir_all(&current.data_dir).context("clear the old journal before joining") {
        return internal(e);
    }
    let (node, _report) = match Node::join_from_ticket(&current.data_dir, &body.ticket, &password).await {
        Ok(pair) => pair,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    };
    for step in [
        node.journal().ensure_device_name("web"),
        node.journal().ensure_peer_join(),
        node.journal().ensure_version_seen(),
        node.journal().set_passcode(&body.passcode).map(|_| ()),
    ] {
        if let Err(e) = step {
            return internal(e);
        }
    }
    let mut new_state = AppState::new(node, current.downloads_dir.clone(), current.data_dir.clone());
    new_state.embedder = current.embedder.clone(); // the opt-in is per process, not per journal
    let new_state = Arc::new(new_state);
    *router_state.0.write().unwrap() = new_state;
    Json(json!({"ok": true})).into_response()
}

// ---- helpers ----

pub(crate) fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({"error": msg}))).into_response()
}

pub(crate) fn internal(e: anyhow::Error) -> Response {
    tracing::error!("internal error: {e:#}");
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

#[derive(Serialize)]
pub struct Never {}
