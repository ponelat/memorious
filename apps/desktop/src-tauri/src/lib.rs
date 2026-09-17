//! Tauri shell: the desktop is its own peer — core embedded, own data dir,
//! never a client of the server. The web UI talks to these commands through
//! the same JournalApi seam the browser uses for HTTP.

use std::path::PathBuf;
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

#[derive(Default)]
pub struct NodeState(Arc<Mutex<Option<Arc<Node>>>>);

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
    let journal = Journal::open(&dir, password)?;
    journal.ensure_device_name(&default_device_name())?;
    journal.ensure_peer_join()?;
    journal.ensure_version_seen()?;
    let n = Arc::new(Node::spawn(journal).await?);
    *state.0.lock().await = Some(n.clone());
    spawn_ping_loop(app, n.clone());
    Ok(n)
}

async fn node<R: tauri::Runtime>(app: &AppHandle<R>, state: &State<'_, NodeState>) -> Result<Arc<Node>> {
    {
        let guard = state.0.lock().await;
        if let Some(n) = guard.as_ref() {
            return Ok(n.clone());
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

// ---- setup ----

#[tauri::command]
async fn setup_state<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
) -> Result<String, String> {
    let dir = data_dir(&app).map_err(estr)?;
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
    password: String,
) -> Result<(), String> {
    if state.0.lock().await.is_some() {
        return Ok(());
    }
    open_with(&app, &state, &password).await.map_err(estr)?;
    cache_password(&password);
    Ok(())
}

#[tauri::command]
async fn setup_init<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    password: String,
) -> Result<(), String> {
    let dir = data_dir(&app).map_err(estr)?;
    let journal = Journal::init(&dir, &password).map_err(estr)?;
    journal.ensure_device_name(&default_device_name()).map_err(estr)?;
    journal.ensure_peer_join().map_err(estr)?;
    journal.ensure_version_seen().map_err(estr)?;
    let n = Arc::new(Node::spawn(journal).await.map_err(estr)?);
    *state.0.lock().await = Some(n.clone());
    spawn_ping_loop(&app, n);
    cache_password(&password);
    Ok(())
}

#[tauri::command]
async fn setup_join<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    ticket: String,
    password: String,
) -> Result<Value, String> {
    let dir = data_dir(&app).map_err(estr)?;
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
    *state.0.lock().await = Some(n.clone());
    spawn_ping_loop(&app, n);
    Ok(json!({"received": report.received, "blobs": report.blobs_fetched}))
}

// ---- capture ----

#[tauri::command]
async fn capture_text<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    text: String,
) -> Result<Value, String> {
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
) -> Result<Value, String> {
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
    before: Option<i64>,
    limit: Option<usize>,
) -> Result<Value, String> {
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
    hash: String,
) -> Result<tauri::ipc::Response, String> {
    let n = node(&app, &state).await.map_err(estr)?;
    let bytes = n.blob_bytes(&hash).await.map_err(estr)?;
    Ok(tauri::ipc::Response::new(bytes))
}

#[tauri::command]
async fn redact<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    event_id: String,
) -> Result<(), String> {
    let n = node(&app, &state).await.map_err(estr)?;
    n.journal().redact(&event_id).map_err(estr)?;
    Ok(())
}

#[tauri::command]
async fn trash_list<R: tauri::Runtime>(app: AppHandle<R>, state: State<'_, NodeState>) -> Result<Value, String> {
    let n = node(&app, &state).await.map_err(estr)?;
    let mut entries = n.journal().trash().map_err(estr)?;
    entries.reverse();
    Ok(json!({"entries": entries.iter().map(entry_json).collect::<Vec<_>>()}))
}

#[tauri::command]
async fn search<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    q: String,
) -> Result<Value, String> {
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
async fn status<R: tauri::Runtime>(app: AppHandle<R>, state: State<'_, NodeState>) -> Result<Value, String> {
    let n = node(&app, &state).await.map_err(estr)?;
    n.status_json().await.map_err(estr)
}

#[tauri::command]
async fn set_device_name<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    device_id: String,
    name: String,
) -> Result<(), String> {
    let n = node(&app, &state).await.map_err(estr)?;
    n.journal().set_device_name(&device_id, &name).map_err(estr)?;
    Ok(())
}

/// Store the network config (relays / public lookup). Applied on relaunch.
#[tauri::command]
async fn set_net_config<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    net: memorious_core::node::NetConfig,
) -> Result<(), String> {
    let n = node(&app, &state).await.map_err(estr)?;
    n.journal().set_net_config(&net).map_err(estr)?;
    Ok(())
}

// ---- master password ----

/// Originate a master-password change: re-wraps existing media, re-keys the
/// local database, and starts using it immediately. Every other device must
/// separately call `adopt_master_password` once told the new password.
#[tauri::command]
async fn rotate_master_password<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    password: String,
) -> Result<(), String> {
    let n = node(&app, &state).await.map_err(estr)?;
    n.journal().rotate_master_password(&password).map_err(estr)?;
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
    password: String,
) -> Result<(), String> {
    let n = node(&app, &state).await.map_err(estr)?;
    n.journal().adopt_master_password(&password).map_err(estr)?;
    cache_password(&password);
    Ok(())
}

// ---- sync ----

#[tauri::command]
async fn sync_now<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, NodeState>,
    ticket: Option<String>,
) -> Result<Value, String> {
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
) -> Result<Value, String> {
    let n = node(&app, &state).await.map_err(estr)?;
    let dir = export_dir(&app).map_err(estr)?;
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
) -> Result<(), String> {
    let dir = data_dir(&app).map_err(estr)?;
    if let Some(h) = app.state::<PingTask>().0.lock().unwrap().take() {
        h.abort();
    }
    if let Some(n) = state.0.lock().await.take() {
        n.shutdown_ref().await;
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
        rotate_master_password,
        adopt_master_password,
        sync_now,
        export_journal,
        reset_device,
    ]
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .manage(NodeState::default())
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
                let n = app_handle.state::<NodeState>().0.lock().await.clone();
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
