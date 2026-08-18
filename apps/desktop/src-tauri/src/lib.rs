//! Tauri shell: the desktop is its own peer — core embedded, own data dir,
//! never a client of the server. The web UI talks to these commands through
//! the same JournalApi seam the browser uses for HTTP.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use memorious_core::api_json::{entry_json, entry_json_annotated};
use memorious_core::event::MediaKind;
use memorious_core::node::JournalTicket;
use memorious_core::{Journal, Node};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};
use tokio::sync::Mutex;

const LAST_PEER_TICKET: &str = "last_peer_ticket";

/// OS keychain slot for the master password: unlock once, then app launches
/// are silent. Tests set MEMORIOUS_NO_KEYRING to stay off the real keychain.
const KEYRING_SERVICE: &str = "com.ponelat.memorious";
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
    let n = Arc::new(Node::spawn(journal).await?);
    *state.0.lock().await = Some(n.clone());
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
    let n = Arc::new(Node::spawn(journal).await.map_err(estr)?);
    *state.0.lock().await = Some(n);
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
    cache_password(&password);
    n.journal()
        .store
        .meta_set(LAST_PEER_TICKET, ticket.trim().as_bytes())
        .map_err(estr)?;
    *state.0.lock().await = Some(Arc::new(n));
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
        "audio" => {
            if !memorious_core::media::is_mp4_family(&bytes) {
                return Err("audio must be an m4a/mp4 recording".into());
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
    let e = n.capture_blob(kind, bytes).await.map_err(estr)?;
    Ok(entry_json(&e))
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

pub fn handlers<R: tauri::Runtime>(
) -> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        setup_state,
        unlock,
        setup_init,
        setup_join,
        capture_text,
        capture_media,
        feed,
        media_bytes,
        redact,
        trash_list,
        search,
        status,
        set_device_name,
        set_net_config,
        sync_now,
    ]
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(NodeState::default())
        .invoke_handler(handlers())
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
