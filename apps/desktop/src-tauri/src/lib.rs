//! Tauri shell: the desktop is its own peer — core embedded, own data dir,
//! never a client of the server. The web UI talks to these commands through
//! the same JournalApi seam the browser uses for HTTP.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use memorious_core::api_json::{entry_json, entry_json_annotated};
use memorious_core::event::{AudioKind, MediaKind};
use memorious_core::node::JournalTicket;
use memorious_core::{Journal, Node};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};
use tokio::sync::Mutex;

const LAST_PEER_TICKET: &str = "last_peer_ticket";

/// OS keychain slot for the master password: unlock once, then app launches
/// are silent. Tests set MEMORIOUS_NO_KEYRING to stay off the real keychain.
const KEYRING_SERVICE: &str = "app.memorious";
const KEYRING_USER: &str = "master-password";

fn keyring_entry() -> Option<keyring::Entry> {
    if std::env::var_os("MEMORIOUS_NO_KEYRING").is_some() {
        return None;
    }
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).ok()
}

fn cached_password() -> Option<String> {
    keyring_entry()?.get_password().ok()
}

fn cache_password(password: &str) {
    if let Some(entry) = keyring_entry() {
        if let Err(e) = entry.set_password(password) {
            log::warn!("could not cache master password in keychain: {e}");
        }
    }
}

struct OwnedNode {
    node: Arc<Node>,
    // The lock lives exactly as long as this process owns the journal.
    _lock: memorious_core::local::JournalLock,
}

#[derive(Default)]
pub struct NodeState(Arc<Mutex<Option<OwnedNode>>>);

/// Memory-only copy of the unlocked password, used solely for a daemon
/// handoff. It is never written to an argument, environment variable, or file.
#[derive(Default)]
pub struct MasterPassword(Arc<Mutex<Option<String>>>);

/// Whether this UI has handed its local peer to a background daemon.
#[derive(Default)]
pub struct DaemonState(Arc<Mutex<Option<PathBuf>>>);

impl DaemonState {
    /// Start the desktop in socket-client mode.  This is also used by the
    /// handoff code and the mock-runtime transport test.
    pub fn connect(socket: PathBuf) -> Self {
        Self(Arc::new(Mutex::new(Some(socket))))
    }
}

async fn daemon_call(state: &State<'_, DaemonState>, method: &str, params: Value) -> Option<Result<Value>> {
    let socket = state.0.lock().await.clone()?;
    Some(memorious_core::local::call(&socket, method, params).await)
}

fn daemon_socket(dir: &std::path::Path) -> PathBuf {
    dir.join(".memorious-daemon.sock")
}

async fn remember_password(state: &State<'_, MasterPassword>, password: &str) {
    *state.0.lock().await = Some(password.to_owned());
}

/// Handle to the running peer-ping loop, so a reset (new journal, new node)
/// can stop the old loop instead of leaking it.
#[derive(Default)]
pub struct PingTask(std::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>);

/// One round: ping every known peer, log the reachable count.
async fn ping_once(n: &Node) {
    match n.ping_peers(std::time::Duration::from_secs(4)).await {
        Ok(pings) => {
            let ok = pings.iter().filter(|p| p.ok).count();
            log::info!("peer ping: {ok}/{} reachable", pings.len());
        }
        Err(err) => log::warn!("peer ping failed: {err:#}"),
    }
}

/// Ping now, then again every `PEER_PING_INTERVAL_MS` (default 15 min) for as
/// long as this node lives — the desktop app is its own scheduler while it's
/// open, no external cron needed. Replaces any loop from a previous node
/// (e.g. after `reset_device` + re-pair).
fn spawn_ping_loop<R: tauri::Runtime>(app: &AppHandle<R>, n: Arc<Node>) {
    let interval_ms: u64 = std::env::var("PEER_PING_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(900_000);
    let handle = tauri::async_runtime::spawn(async move {
        loop {
            ping_once(&n).await;
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
        }
    });
    let ping_state = app.state::<PingTask>();
    let old = ping_state.0.lock().unwrap().replace(handle);
    if let Some(old) = old {
        old.abort();
    }
}

fn data_dir<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("MEMORIOUS_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(app
        .path()
        .app_data_dir()
        .context("no app data dir")?
        .join("journal"))
}

/// Platform default for this device's friendly name ("desktop (macOS)").
fn default_device_name() -> String {
    let os = match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    };
    format!("desktop ({os})")
}

async fn open_with<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &State<'_, NodeState>,
    password: &str,
) -> Result<Arc<Node>> {
    let dir = data_dir(app)?;
    let lock = memorious_core::local::JournalLock::acquire(&dir)?;
    let journal = Journal::open(&dir, password)?;
    journal.ensure_device_name(&default_device_name())?;
    journal.ensure_peer_join()?;
    journal.ensure_version_seen()?;
    let n = Arc::new(Node::spawn(journal).await?);
    *state.0.lock().await = Some(OwnedNode { node: n.clone(), _lock: lock });
    spawn_ping_loop(app, n.clone());
    Ok(n)
}

async fn node<R: tauri::Runtime>(app: &AppHandle<R>, state: &State<'_, NodeState>) -> Result<Arc<Node>> {
    {
        let guard = state.0.lock().await;
        if let Some(n) = guard.as_ref() {
            return Ok(n.node.clone());
        }
    }
    let dir = data_dir(app)?;
    if !dir.join("db.sqlite").exists() {
        return Err(anyhow!("journal not set up yet"));
    }
    let Some(password) = cached_password() else {
        return Err(anyhow!("journal is locked — enter the master password"));
    };
    open_with(app, state, &password)
        .await
        .context("journal is locked — enter the master password")
}

fn estr(e: anyhow::Error) -> String {
    format!("{e:#}")
}

const DAEMON_FLAG: &str = "--memorious-daemon";

/// Private, local-only configuration for the optional desktop PTT worker.
/// The journal has no PTT events or credentials: the worker submits ordinary
/// captures through the daemon socket.  `api_key` is never returned to the UI.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct PttConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_ptt_key")]
    key: String,
    #[serde(default)]
    api_key: String,
    #[serde(default = "default_stt_model")]
    stt_model: String,
    #[serde(default = "default_cleanup_model")]
    cleanup_model: String,
}

fn default_ptt_key() -> String { "KEY_RIGHTCTRL".into() }
fn default_stt_model() -> String { "gpt-4o-mini-transcribe".into() }
fn default_cleanup_model() -> String { "gpt-4o-mini".into() }

impl Default for PttConfig {
    fn default() -> Self {
        Self { enabled: false, key: default_ptt_key(), api_key: String::new(), stt_model: default_stt_model(), cleanup_model: default_cleanup_model() }
    }
}

fn ptt_config_path(dir: &std::path::Path) -> PathBuf { dir.join(".memorious-ptt.json") }
fn ptt_status_path(dir: &std::path::Path) -> PathBuf { dir.join(".memorious-ptt-status.json") }

fn write_ptt_status(dir: &std::path::Path, state: &str, error: &str) {
    let _ = std::fs::write(ptt_status_path(dir), json!({"state": state, "error": error}).to_string());
}

fn load_ptt_config(dir: &std::path::Path) -> Result<PttConfig> {
    let path = ptt_config_path(dir);
    if !path.exists() { return Ok(PttConfig::default()); }
    Ok(serde_json::from_slice(&std::fs::read(&path)?).context("invalid PTT configuration")?)
}

fn save_ptt_config(dir: &std::path::Path, config: &PttConfig) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = ptt_config_path(dir);
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(config)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn ptt_worker(exe: &std::path::Path) -> Option<PathBuf> {
    let name = if cfg!(windows) { "memorious-ptt.exe" } else { "memorious-ptt" };
    let worker = exe.parent()?.join(name);
    worker.is_file().then_some(worker)
}

fn start_ptt_worker(data: &std::path::Path, socket: &std::path::Path) -> Result<Option<std::process::Child>> {
    let config = load_ptt_config(data)?;
    if !config.enabled { return Ok(None); }
    if config.api_key.trim().is_empty() { return Err(anyhow!("PTT is enabled but no transcription API key is configured")); }
    let exe = std::env::current_exe()?;
    let worker = ptt_worker(&exe).ok_or_else(|| anyhow!("this desktop build does not include push-to-talk on this platform"))?;
    Ok(Some(Command::new(worker)
        .arg("--data-dir").arg(data)
        .arg("--socket").arg(socket)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn().context("start push-to-talk worker")?))
}

/// The bundled desktop executable has a tiny no-window personality for
/// background ownership. The password arrives once on stdin and never enters
/// argv, an environment variable, or a persisted configuration file.
fn run_daemon_personality() -> Option<Result<()>> {
    let mut args = std::env::args_os();
    let _exe = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new(DAEMON_FLAG)) {
        return None;
    }
    let Some(data) = args.next().map(PathBuf::from) else {
        return Some(Err(anyhow!("missing daemon journal directory")));
    };
    let Some(socket) = args.next().map(PathBuf::from) else {
        return Some(Err(anyhow!("missing daemon socket")));
    };
    let mut password = String::new();
    let mut stdin = std::io::stdin();
    if let Err(e) = stdin.read_to_string(&mut password) {
        return Some(Err(e.into()));
    }
    Some(tokio::runtime::Runtime::new()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(async move {
            // A bad optional PTT setting must never make the actual journal
            // handoff fail. Surface it in the desktop status instead.
            let mut worker = match start_ptt_worker(&data, &socket) {
                Ok(worker) => worker,
                Err(error) => {
                    write_ptt_status(&data, "failed", &error.to_string());
                    None
                }
            };
            let daemon = memorious_core::local::Daemon::open(&data, &password).await;
            let result = match daemon {
                Ok(daemon) => daemon.serve(socket).await,
                Err(error) => Err(error),
            };
            if let Some(child) = worker.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_file(ptt_status_path(&data));
            result
        })))
}

async fn wait_for_socket(socket: &std::path::Path, present: bool) -> Result<()> {
    for _ in 0..80 {
        if socket.exists() == present {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    Err(anyhow!("background daemon did not {}", if present { "start" } else { "stop" }))
}

#[tauri::command]
async fn background_status<R: tauri::Runtime>(
    app: AppHandle<R>,
    daemon: State<'_, DaemonState>,
) -> Result<Value, String> {
    let configured = daemon.0.lock().await.clone().or_else(|| {
        let socket = data_dir(&app).ok().map(|dir| daemon_socket(&dir))?;
        socket.exists().then_some(socket)
    });
    let Some(socket) = configured else { return Ok(json!({"mode": "direct"})); };
    match memorious_core::local::call(&socket, "health", json!({})).await {
        Ok(_) => Ok(json!({"mode": "background"})),
        Err(e) => Ok(json!({"mode": "stopped", "error": e.to_string(), "socket": socket})),
    }
}

#[tauri::command]
async fn enable_background<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    master: State<'_, MasterPassword>,
) -> Result<(), String> {
    if daemon.0.lock().await.is_some() { return Ok(()); }
    let password = master.0.lock().await.clone().or_else(cached_password)
        .ok_or("journal is locked — enter the master password")?;
    let dir = data_dir(&app).map_err(estr)?;
    let socket = daemon_socket(&dir);
    let owned = state.0.lock().await.take().ok_or("journal is not open")?;
    if let Some(h) = app.state::<PingTask>().0.lock().unwrap().take() { h.abort(); }
    owned.node.shutdown_ref().await;
    drop(owned); // releases the owner lock before the child attempts to acquire it
    let exe = std::env::current_exe().map_err(|e| estr(e.into()))?;
    let mut child = Command::new(exe)
        .arg(DAEMON_FLAG).arg(&dir).arg(&socket)
        .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn().map_err(|e| estr(e.into()))?;
    child.stdin.take().ok_or("could not open daemon stdin")?.write_all(password.as_bytes()).map_err(|e| estr(e.into()))?;
    wait_for_socket(&socket, true).await.map_err(estr)?;
    memorious_core::local::call(&socket, "health", json!({})).await.map_err(estr)?;
    *daemon.0.lock().await = Some(socket);
    Ok(())
}

#[tauri::command]
async fn disable_background(
    daemon: State<'_, DaemonState>,
) -> Result<(), String> {
    let socket = daemon.0.lock().await.clone().ok_or("background mode is not active")?;
    memorious_core::local::call(&socket, "shutdown", json!({})).await.map_err(estr)?;
    wait_for_socket(&socket, false).await.map_err(estr)?;
    *daemon.0.lock().await = None;
    Ok(())
}

#[tauri::command]
async fn reclaim_background(
    daemon: State<'_, DaemonState>,
) -> Result<(), String> {
    // A crashed daemon has already released its advisory lock. Clearing this
    // state lets the next UI operation lazily reopen the embedded owner.
    *daemon.0.lock().await = None;
    Ok(())
}

#[tauri::command]
async fn ptt_status<R: tauri::Runtime>(
    app: AppHandle<R>,
    daemon: State<'_, DaemonState>,
) -> Result<Value, String> {
    let dir = data_dir(&app).map_err(estr)?;
    let config = load_ptt_config(&dir).map_err(estr)?;
    let background = daemon_call(&daemon, "health", json!({})).await
        .map(|result| result.is_ok()).unwrap_or(false);
    let supported = std::env::current_exe().ok().and_then(|exe| ptt_worker(&exe)).is_some();
    let worker = std::fs::read(ptt_status_path(&dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    Ok(json!({
        "available": background,
        "supported": supported,
        "enabled": config.enabled,
        "key": config.key,
        "stt_model": config.stt_model,
        "cleanup_model": config.cleanup_model,
        "api_key_configured": !config.api_key.trim().is_empty(),
        "worker": worker,
        "restart_required": config.enabled && worker.is_none(),
    }))
}

#[tauri::command]
async fn set_ptt_config<R: tauri::Runtime>(
    app: AppHandle<R>,
    daemon: State<'_, DaemonState>,
    enabled: bool,
    key: String,
    api_key: Option<String>,
    stt_model: String,
    cleanup_model: String,
) -> Result<(), String> {
    if daemon_call(&daemon, "health", json!({})).await.is_none() {
        return Err("push-to-talk is available only while background mode is running".into());
    }
    if std::env::current_exe().ok().and_then(|exe| ptt_worker(&exe)).is_none() {
        return Err("this desktop build does not include push-to-talk on this platform".into());
    }
    let key = key.trim();
    if key.is_empty() || !key.starts_with("KEY_") {
        return Err("PTT key must be a Linux evdev key name such as KEY_RIGHTCTRL".into());
    }
    let dir = data_dir(&app).map_err(estr)?;
    let mut config = load_ptt_config(&dir).map_err(estr)?;
    config.enabled = enabled;
    config.key = key.to_owned();
    config.stt_model = stt_model.trim().to_owned();
    config.cleanup_model = cleanup_model.trim().to_owned();
    if let Some(api_key) = api_key {
        config.api_key = api_key.trim().to_owned();
    }
    if config.enabled && config.api_key.is_empty() {
        return Err("a transcription API key is required to enable push-to-talk".into());
    }
    save_ptt_config(&dir, &config).map_err(estr)
}

// ---- setup ----

#[tauri::command]
async fn setup_state<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
) -> Result<String, String> {
    if let Some(result) = daemon_call(&daemon, "health", json!({})).await {
        result.map_err(estr)?;
        return Ok("ready".into());
    }
    let dir = data_dir(&app).map_err(estr)?;
    // A fresh desktop window may be opening after an earlier one handed this
    // journal to its background process. Reattach before attempting a direct
    // open, whose lock correctly rejects us.
    let socket = daemon_socket(&dir);
    if socket.exists() && memorious_core::local::call(&socket, "health", json!({})).await.is_ok() {
        *daemon.0.lock().await = Some(socket);
        return Ok("ready".into());
    }
    if !dir.join("db.sqlite").exists() {
        return Ok("empty".into());
    }
    // Auto-unlock from the keychain when possible; otherwise the UI asks.
    Ok(if node(&app, &state).await.is_ok() {
        "ready".into()
    } else {
        "locked".into()
    })
}

#[tauri::command]
async fn unlock<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    master: State<'_, MasterPassword>,
    password: String,
) -> Result<(), String> {
    if daemon.0.lock().await.is_some() {
        // The handoff only occurs after the daemon has unlocked the journal;
        // never send its password back across the local socket.
        return Ok(());
    }
    if state.0.lock().await.is_some() {
        return Ok(());
    }
    open_with(&app, &state, &password).await.map_err(estr)?;
    remember_password(&master, &password).await;
    cache_password(&password);
    Ok(())
}

#[tauri::command]
async fn setup_init<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    master: State<'_, MasterPassword>,
    password: String,
) -> Result<(), String> {
    if daemon.0.lock().await.is_some() {
        return Err("journal is owned by the background daemon".into());
    }
    let dir = data_dir(&app).map_err(estr)?;
    let lock = memorious_core::local::JournalLock::acquire(&dir).map_err(estr)?;
    let journal = Journal::init(&dir, &password).map_err(estr)?;
    journal.ensure_device_name(&default_device_name()).map_err(estr)?;
    journal.ensure_peer_join().map_err(estr)?;
    journal.ensure_version_seen().map_err(estr)?;
    let n = Arc::new(Node::spawn(journal).await.map_err(estr)?);
    *state.0.lock().await = Some(OwnedNode { node: n.clone(), _lock: lock });
    spawn_ping_loop(&app, n);
    remember_password(&master, &password).await;
    cache_password(&password);
    Ok(())
}

#[tauri::command]
async fn setup_join<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    master: State<'_, MasterPassword>,
    ticket: String,
    password: String,
) -> Result<Value, String> {
    if daemon.0.lock().await.is_some() {
        return Err("journal is owned by the background daemon".into());
    }
    let dir = data_dir(&app).map_err(estr)?;
    let lock = memorious_core::local::JournalLock::acquire(&dir).map_err(estr)?;
    let (n, report) = Node::join_from_ticket(&dir, &ticket, &password)
        .await
        .map_err(estr)?;
    n.journal()
        .ensure_device_name(&default_device_name())
        .map_err(estr)?;
    n.journal().ensure_peer_join().map_err(estr)?;
    n.journal().ensure_version_seen().map_err(estr)?;
    cache_password(&password);
    n.journal()
        .store
        .meta_set(LAST_PEER_TICKET, ticket.trim().as_bytes())
        .map_err(estr)?;
    let n = Arc::new(n);
    *state.0.lock().await = Some(OwnedNode { node: n.clone(), _lock: lock });
    spawn_ping_loop(&app, n);
    remember_password(&master, &password).await;
    Ok(json!({"received": report.received, "blobs": report.blobs_fetched}))
}

// ---- capture ----

#[tauri::command]
async fn capture_text<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    text: String,
) -> Result<Value, String> {
    if let Some(result) = daemon_call(&daemon, "capture_text", json!({"text": text})).await {
        return result.map_err(estr);
    }
    let n = node(&app, &state).await.map_err(estr)?;
    let e = n.journal().capture_text(&text).map_err(estr)?;
    Ok(entry_json(&e))
}

/// Media kind travels as a header because the body is the bytes themselves.
const MEDIA_KIND_HEADER: &str = "media-kind";

/// Media arrives as a raw request body — an `ArrayBuffer` from the webview, not a
/// JSON array of numbers. A pasted screenshot is megabytes, and the JSON shape
/// costs ~30x that in transient allocation on both sides; on Linux that killed the
/// WebKit web process (the app looked like it crashed).
#[tauri::command]
async fn capture_media<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    request: tauri::ipc::Request<'_>,
) -> Result<Value, String> {
    let kind = request
        .headers()
        .get(MEDIA_KIND_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| format!("missing {MEDIA_KIND_HEADER} header"))?
        .to_owned();
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("media must be sent as raw bytes".into());
    };
    let bytes = bytes.clone();
    if let Some(result) = daemon_call(&daemon, "capture_media", json!({
        "kind": kind,
        "bytes_b64": data_encoding::BASE64.encode(&bytes),
    })).await {
        return result.map_err(estr);
    }
    let n = node(&app, &state).await.map_err(estr)?;
    // "music" is audio whose recording is the record — never transcribed,
    // never evicted (crates/core/src/retention.rs).
    let mut audio_kind = AudioKind::Voice;
    let (kind, bytes) = match kind.as_str() {
        "photo" => {
            let jpeg = tokio::task::spawn_blocking(move || {
                memorious_core::media::normalize_photo(&bytes)
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(estr)?;
            (MediaKind::Photo, jpeg)
        }
        "audio" | "music" => {
            if !memorious_core::media::is_mp4_family(&bytes) {
                return Err("audio must be an m4a/mp4 recording".into());
            }
            if kind == "music" {
                audio_kind = AudioKind::Music;
            }
            (MediaKind::Audio, bytes)
        }
        "video" => {
            if !memorious_core::media::is_mp4_family(&bytes) {
                return Err("video must be an mp4 recording".into());
            }
            (MediaKind::Video, bytes)
        }
        other => return Err(format!("unknown media kind {other}")),
    };
    let e = match kind {
        MediaKind::Audio => n.capture_audio(bytes, audio_kind).await,
        other => n.capture_blob(other, bytes).await,
    }
    .map_err(estr)?;
    Ok(entry_json(&e))
}

#[tauri::command]
async fn ping_peers<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
) -> Result<Value, String> {
    if let Some(result) = daemon_call(&daemon, "ping_peers", json!({})).await {
        return result.map_err(estr);
    }
    let n = node(&app, &state).await.map_err(estr)?;
    let pings = n
        .ping_peers(std::time::Duration::from_secs(4))
        .await
        .map_err(estr)?;
    Ok(serde_json::json!({ "pings": pings }))
}

// ---- reading ----

#[tauri::command]
async fn feed<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    before: Option<i64>,
    limit: Option<usize>,
) -> Result<Value, String> {
    if let Some(result) = daemon_call(&daemon, "feed", json!({"before": before, "limit": limit})).await {
        return result.map_err(estr);
    }
    let n = node(&app, &state).await.map_err(estr)?;
    let limit = limit.unwrap_or(50).min(500);
    let annotations = n.journal().annotations().map_err(estr)?;
    let mut entries = n.journal().list().map_err(estr)?;
    entries.reverse();
    let page: Vec<_> = entries
        .iter()
        .filter(|e| before.map(|b| e.recorded_at < b).unwrap_or(true))
        .take(limit)
        .map(|e| entry_json_annotated(e, &annotations))
        .collect();
    let next_before = page.last().and_then(|e| e["recorded_at"].as_i64());
    Ok(json!({"entries": page, "next_before": next_before}))
}

#[tauri::command]
async fn media_bytes<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    hash: String,
) -> Result<tauri::ipc::Response, String> {
    if let Some(result) = daemon_call(&daemon, "media_bytes", json!({"hash": hash})).await {
        let result = result.map_err(estr)?;
        let bytes = data_encoding::BASE64.decode(result["bytes_b64"].as_str().unwrap_or("").as_bytes())
            .map_err(|e| e.to_string())?;
        return Ok(tauri::ipc::Response::new(bytes));
    }
    let n = node(&app, &state).await.map_err(estr)?;
    let bytes = n.blob_bytes(&hash).await.map_err(estr)?;
    Ok(tauri::ipc::Response::new(bytes))
}

#[tauri::command]
async fn redact<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    event_id: String,
) -> Result<(), String> {
    if let Some(result) = daemon_call(&daemon, "redact", json!({"event_id": event_id})).await {
        result.map_err(estr)?;
        return Ok(());
    }
    let n = node(&app, &state).await.map_err(estr)?;
    n.journal().redact(&event_id).map_err(estr)?;
    Ok(())
}

#[tauri::command]
async fn trash_list<R: tauri::Runtime>(app: AppHandle<R>, state: State<'_, NodeState>, daemon: State<'_, DaemonState>) -> Result<Value, String> {
    if let Some(result) = daemon_call(&daemon, "trash", json!({})).await { return result.map_err(estr); }
    let n = node(&app, &state).await.map_err(estr)?;
    let mut entries = n.journal().trash().map_err(estr)?;
    entries.reverse();
    Ok(json!({"entries": entries.iter().map(entry_json).collect::<Vec<_>>()}))
}

#[tauri::command]
async fn search<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    q: String,
) -> Result<Value, String> {
    if let Some(result) = daemon_call(&daemon, "search", json!({"query": q})).await { return result.map_err(estr); }
    let n = node(&app, &state).await.map_err(estr)?;
    let journal = n.journal();
    let run = || -> Result<Vec<Value>> {
        let redacted = journal.store.redacted_ids()?;
        let mut out = Vec::new();
        for id in journal.store.search(&q)? {
            if let Some(e) = journal.store.get_event(&id)? {
                let display = match &e.payload {
                    memorious_core::Payload::Annotation { target, .. } => {
                        journal.store.get_event(target)?
                    }
                    _ => Some(e),
                };
                if let Some(e) = display {
                    if e.kind == memorious_core::EventKind::Capture
                        && !redacted.contains(&e.event_id)
                    {
                        out.push(entry_json(&e));
                    }
                }
            }
        }
        Ok(out)
    };
    Ok(json!({"entries": run().map_err(estr)?}))
}

#[tauri::command]
async fn status<R: tauri::Runtime>(app: AppHandle<R>, state: State<'_, NodeState>, daemon: State<'_, DaemonState>) -> Result<Value, String> {
    if let Some(result) = daemon_call(&daemon, "status", json!({})).await { return result.map_err(estr); }
    let n = node(&app, &state).await.map_err(estr)?;
    n.status_json().await.map_err(estr)
}

#[tauri::command]
async fn set_device_name<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    device_id: String,
    name: String,
) -> Result<(), String> {
    if let Some(result) = daemon_call(&daemon, "set_device_name", json!({"device_id": device_id, "name": name})).await {
        result.map_err(estr)?;
        return Ok(());
    }
    let n = node(&app, &state).await.map_err(estr)?;
    n.journal().set_device_name(&device_id, &name).map_err(estr)?;
    Ok(())
}

/// Store the network config (relays / public lookup). Applied on relaunch.
#[tauri::command]
async fn set_net_config<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    net: memorious_core::node::NetConfig,
) -> Result<(), String> {
    if let Some(result) = daemon_call(&daemon, "set_net_config", json!({"net": net})).await {
        result.map_err(estr)?;
        return Ok(());
    }
    let n = node(&app, &state).await.map_err(estr)?;
    n.journal().set_net_config(&net).map_err(estr)?;
    Ok(())
}

// ---- MCP agent keys ----

#[tauri::command]
async fn mint_agent<R: tauri::Runtime>(app: AppHandle<R>, state: State<'_, NodeState>, daemon: State<'_, DaemonState>, name: String) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if let Some(result) = daemon_call(&daemon, "mint_agent", json!({"name": name})).await {
        return result.map_err(estr)?.as_str().map(str::to_owned).ok_or_else(|| "invalid daemon agent ticket".into());
    }
    let n = node(&app, &state).await.map_err(estr)?;
    n.mint_agent_ticket(name).await.map_err(estr)
}

#[tauri::command]
async fn list_agents<R: tauri::Runtime>(app: AppHandle<R>, state: State<'_, NodeState>, daemon: State<'_, DaemonState>) -> Result<Vec<memorious_core::mcp_quic::AgentKey>, String> {
    if let Some(result) = daemon_call(&daemon, "list_agents", json!({})).await {
        return serde_json::from_value(result.map_err(estr)?).map_err(|e| e.to_string());
    }
    let n = node(&app, &state).await.map_err(estr)?;
    n.agent_keys().map_err(estr)
}

#[tauri::command]
async fn revoke_agent<R: tauri::Runtime>(app: AppHandle<R>, state: State<'_, NodeState>, daemon: State<'_, DaemonState>, id: String) -> Result<(), String> {
    if let Some(result) = daemon_call(&daemon, "revoke_agent", json!({"id": id})).await {
        result.map_err(estr)?;
        return Ok(());
    }
    let n = node(&app, &state).await.map_err(estr)?;
    n.revoke_agent_key(&id).map_err(estr)
}

// ---- master password ----

/// Originate a master-password change: re-wraps existing media, re-keys the
/// local database, and starts using it immediately. Every other device must
/// separately call `adopt_master_password` once told the new password.
#[tauri::command]
async fn rotate_master_password<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    master: State<'_, MasterPassword>,
    password: String,
) -> Result<(), String> {
    if let Some(result) = daemon_call(&daemon, "rotate_master_password", json!({"password": password})).await {
        result.map_err(estr)?;
        remember_password(&master, &password).await;
        cache_password(&password);
        return Ok(());
    }
    let n = node(&app, &state).await.map_err(estr)?;
    n.journal().rotate_master_password(&password).map_err(estr)?;
    remember_password(&master, &password).await;
    cache_password(&password);
    Ok(())
}

/// Catch up to a rotation another device already published. Rejects a wrong
/// guess (verified against that device's password proof) without changing
/// anything.
#[tauri::command]
async fn adopt_master_password<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    master: State<'_, MasterPassword>,
    password: String,
) -> Result<(), String> {
    if let Some(result) = daemon_call(&daemon, "adopt_master_password", json!({"password": password})).await {
        result.map_err(estr)?;
        remember_password(&master, &password).await;
        cache_password(&password);
        return Ok(());
    }
    let n = node(&app, &state).await.map_err(estr)?;
    n.journal().adopt_master_password(&password).map_err(estr)?;
    remember_password(&master, &password).await;
    cache_password(&password);
    Ok(())
}

// ---- sync ----

#[tauri::command]
async fn sync_now<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
    ticket: Option<String>,
) -> Result<Value, String> {
    if let Some(result) = daemon_call(&daemon, "sync", json!({"ticket": ticket})).await {
        return result.map_err(estr);
    }
    let n = node(&app, &state).await.map_err(estr)?;
    let journal = n.journal();
    let ticket_str = match ticket {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => journal
            .store
            .meta_get(LAST_PEER_TICKET)
            .map_err(estr)?
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or("no known peer — paste a ticket")?,
    };
    let t = JournalTicket::decode(&ticket_str).map_err(estr)?;
    if &t.secret != journal.secret() {
        return Err("ticket is for a different journal".into());
    }
    let report = n.sync_with(&t.addr().map_err(estr)?).await.map_err(estr)?;
    journal
        .store
        .meta_set(LAST_PEER_TICKET, ticket_str.as_bytes())
        .map_err(estr)?;
    Ok(json!({
        "sent": report.sent,
        "received": report.received,
        "blobs": report.blobs_fetched,
    }))
}

// ---- this device: export + reset ----

/// Where the markdown mirror goes: `MEMORIOUS_EXPORT_DIR`, else
/// ~/Documents/memorious-journal, else ~/memorious-journal (a Linux session
/// without XDG user dirs has no Documents folder). A folder rather than a
/// zip: on a desktop the mirror is worth more as files, and a re-export
/// updates it in place.
fn export_dir<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<PathBuf> {
    export_dir_from(
        std::env::var_os("MEMORIOUS_EXPORT_DIR").map(PathBuf::from),
        app.path().document_dir().ok(),
        app.path().home_dir().ok(),
    )
}

fn export_dir_from(
    override_dir: Option<PathBuf>,
    documents: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(dir) = override_dir {
        return Ok(dir);
    }
    let base = documents
        .or(home)
        .ok_or_else(|| anyhow!("no documents or home dir to export into"))?;
    Ok(base.join("memorious-journal"))
}

/// Mirror the journal as markdown by day (YYYY/MM/DD.md) plus the media this
/// device holds (crates/core/src/export_md.rs). Returns where and how much.
#[tauri::command]
async fn export_journal<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
) -> Result<Value, String> {
    let dir = export_dir(&app).map_err(estr)?;
    if let Some(result) = daemon_call(&daemon, "export_markdown", json!({"out": dir})).await {
        return result.map_err(estr);
    }
    let n = node(&app, &state).await.map_err(estr)?;
    let report = memorious_core::export_md::export_markdown(&n, &dir)
        .await
        .map_err(estr)?;
    Ok(json!({
        "path": dir.to_string_lossy(),
        "days": report.day_files_written + report.day_files_unchanged,
        "media": report.media_written + report.media_unchanged,
    }))
}

/// Delete this device's copy of the journal and forget its password. Other
/// devices keep theirs; the app returns to first-run setup. The node is shut
/// down first so the database and blob store release their files.
#[tauri::command]
async fn reset_device<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    daemon: State<'_, DaemonState>,
) -> Result<(), String> {
    let dir = data_dir(&app).map_err(estr)?;
    let socket = daemon.0.lock().await.clone();
    if let Some(socket) = socket {
        memorious_core::local::call(&socket, "shutdown", json!({}))
            .await
            .map_err(estr)?;
        for _ in 0..40 {
            if !socket.exists() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        if socket.exists() {
            return Err("background daemon did not stop".into());
        }
        *daemon.0.lock().await = None;
    }
    if let Some(h) = app.state::<PingTask>().0.lock().unwrap().take() {
        h.abort();
    }
    if let Some(n) = state.0.lock().await.take() {
        n.node.shutdown_ref().await;
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| format!("could not delete {}: {e}", dir.display()))?;
    }
    if let Some(entry) = keyring_entry() {
        let _ = entry.delete_credential();
    }
    Ok(())
}

// ---- window chrome ----

/// Linux: the title bar is GTK's client-side decoration, drawn by the theme in
/// grey. Paint it the brand instead — flame orange, white title and window
/// buttons — through a CSS provider at application priority (wins over the
/// theme). Does nothing where the window manager draws the decorations itself
/// (server-side, e.g. some X11 setups). macOS keeps its native title bar.
#[cfg(target_os = "linux")]
const TITLE_BAR_CSS: &str = "
headerbar.default-decoration,
.titlebar.default-decoration {
  background: #ff5200;
  background-image: none;
  border-color: #ff5200;
  box-shadow: none;
  color: #ffffff;
  text-shadow: none;
}
headerbar.default-decoration .title,
.titlebar.default-decoration .title,
headerbar.default-decoration button.titlebutton,
.titlebar.default-decoration button.titlebutton {
  color: #ffffff;
  text-shadow: none;
  -gtk-icon-shadow: none;
}
headerbar.default-decoration:backdrop,
.titlebar.default-decoration:backdrop {
  background: #ff5200;
  color: rgba(255, 255, 255, 0.7);
}
headerbar.default-decoration:backdrop .title,
.titlebar.default-decoration:backdrop .title,
headerbar.default-decoration:backdrop button.titlebutton,
.titlebar.default-decoration:backdrop button.titlebutton {
  color: rgba(255, 255, 255, 0.7);
}
";

#[cfg(target_os = "linux")]
fn brand_title_bar<R: tauri::Runtime>(window: &tauri::WebviewWindow<R>) {
    use gtk::prelude::*;
    let Ok(gtk_window) = window.gtk_window() else {
        return;
    };
    let Some(screen) = WidgetExt::screen(&gtk_window) else {
        return;
    };
    let css = gtk::CssProvider::new();
    if let Err(e) = css.load_from_data(TITLE_BAR_CSS.as_bytes()) {
        log::warn!("title bar css not applied: {e}");
        return;
    }
    gtk::StyleContext::add_provider_for_screen(
        &screen,
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

pub fn handlers<R: tauri::Runtime>(
) -> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        setup_state,
        unlock,
        setup_init,
        setup_join,
        background_status,
        enable_background,
        disable_background,
        reclaim_background,
        ptt_status,
        set_ptt_config,
        capture_text,
        capture_media,
        ping_peers,
        feed,
        media_bytes,
        redact,
        trash_list,
        search,
        status,
        set_device_name,
        set_net_config,
        mint_agent,
        list_agents,
        revoke_agent,
        rotate_master_password,
        adopt_master_password,
        sync_now,
        export_journal,
        reset_device,
    ]
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if let Some(result) = run_daemon_personality() {
        if let Err(e) = result { eprintln!("background daemon: {e:#}"); }
        return;
    }
    let app = tauri::Builder::default()
        .manage(NodeState::default())
        .manage(DaemonState::default())
        .manage(MasterPassword::default())
        .manage(PingTask::default())
        // Links in entries open in the system browser; without this the webview
        // silently drops target=_blank navigations.
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(handlers())
        .setup(|app| {
            #[cfg(target_os = "linux")]
            if let Some(window) = app.get_webview_window("main") {
                brand_title_bar(&window);
            }
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // Safe shutdown: hold the exit open long enough for one last ping — the
    // app is a peer too, and this is its only chance to push before it goes
    // quiet until next launch.
    app.run(move |app_handle, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = event {
            api.prevent_exit();
            let app_handle = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                let n = app_handle
                    .state::<NodeState>()
                    .0
                    .lock()
                    .await
                    .as_ref()
                    .map(|owned| owned.node.clone());
                if let Some(n) = n {
                    log::info!("shutting down — final peer ping");
                    ping_once(&n).await;
                }
                app_handle.exit(0);
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Linux boxes without XDG user dirs (a bare NixOS session) have no
    /// Documents folder; the mirror then goes under home rather than failing.
    #[test]
    fn export_dir_falls_back_from_documents_to_home() {
        let over = Some(PathBuf::from("/tmp/override"));
        let docs = Some(PathBuf::from("/home/u/Documents"));
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            export_dir_from(over.clone(), docs.clone(), home.clone()).unwrap(),
            PathBuf::from("/tmp/override")
        );
        assert_eq!(
            export_dir_from(None, docs, home.clone()).unwrap(),
            PathBuf::from("/home/u/Documents/memorious-journal")
        );
        assert_eq!(
            export_dir_from(None, None, home).unwrap(),
            PathBuf::from("/home/u/memorious-journal")
        );
        assert!(export_dir_from(None, None, None).is_err());
    }
}
