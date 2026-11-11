//! Owner-only local control plane for a Memorious node.
//!
//! This deliberately is not another network face: it is a newline-delimited
//! JSON RPC socket for processes belonging to the same OS user.  A daemon owns
//! a journal directory exclusively; desktop, CLI, and capture sidecars talk to
//! it rather than opening the SQLite database themselves.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::event::{AudioKind, MediaKind};
use crate::{Journal, JournalTicket, Node};

/// 64 MiB plaintext media plus base64 framing.  This is intentionally only a
/// local desktop transport; remote capture keeps streaming multipart uploads.
const MAX_LINE: usize = 96 * 1024 * 1024;
/// The last explicitly-dialled peer is local convenience state, never an
/// event.  Desktop direct mode and daemon mode deliberately share this key.
const LAST_PEER_TICKET: &str = "last_peer_ticket";

/// An advisory, process-lifetime lock for one journal directory.
///
/// Every direct owner and daemon must take this lock before opening a journal.
/// Unlike a marker file it is released automatically if its process crashes.
pub struct JournalLock(File);

impl JournalLock {
    pub fn acquire(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let path = root.join(".memorious-owner.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;
        file.try_lock_exclusive().map_err(|_| {
            anyhow::anyhow!("journal is already owned by another Memorious process")
        })?;
        Ok(Self(file))
    }
}

impl Drop for JournalLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    fn ok(id: u64, result: Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: u64, error: anyhow::Error) -> Self {
        Self {
            id,
            result: None,
            error: Some(format!("{error:#}")),
        }
    }
}

/// A running local node.  Retain this value for as long as the daemon runs;
/// its lock is the single-owner guarantee.
pub struct Daemon {
    node: Arc<Node>,
    _lock: JournalLock,
}

impl Daemon {
    pub async fn open(root: &Path, password: &str) -> Result<Self> {
        let lock = JournalLock::acquire(root)?;
        let node = Arc::new(Node::spawn(Journal::open(root, password)?).await?);
        Ok(Self { node, _lock: lock })
    }

    pub fn node(&self) -> &Arc<Node> {
        &self.node
    }

    /// Serve until the process is cancelled.  The socket is mode 0600 and is
    /// removed on exit.  A caller controls shutdown by cancelling this task.
    pub async fn serve(self, socket: PathBuf) -> Result<()> {
        if socket.exists() {
            std::fs::remove_file(&socket)
                .with_context(|| format!("remove stale socket {}", socket.display()))?;
        }
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener =
            UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        }
        let node = self.node.clone();
        let sync_node = node.clone();
        let sync_task = tokio::spawn(async move {
            loop {
                if let Err(error) = sync_node.sync_known_peers().await {
                    tracing::debug!("periodic peer sync deferred: {error:#}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() { break; }
                }
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let node = node.clone();
                    let shutdown_tx = shutdown_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_connection(stream, node, shutdown_tx).await {
                            tracing::debug!("local client disconnected: {e:#}");
                        }
                    });
                }
            }
        }
        sync_task.abort();
        let _ = sync_task.await;
        let _ = std::fs::remove_file(&socket);
        self.node.shutdown_ref().await;
        Ok(())
    }
}

async fn serve_connection(
    stream: UnixStream,
    node: Arc<Node>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        if line.len() > MAX_LINE {
            bail!("local RPC line exceeds {MAX_LINE} bytes");
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let id = request.id;
                let captured = matches!(
                    request.method.as_str(),
                    "capture_text" | "capture_media" | "redact" | "annotate_voice"
                );
                match dispatch(&node, request, &shutdown_tx).await {
                    Ok(value) => {
                        if captured {
                            let node = node.clone();
                            tokio::spawn(async move {
                                let _ = node.sync_known_peers().await;
                            });
                        }
                        Response::ok(id, value)
                    }
                    Err(e) => Response::err(id, e),
                }
            }
            Err(e) => Response::err(0, e.into()),
        };
        write
            .write_all(serde_json::to_string(&response)?.as_bytes())
            .await?;
        write.write_all(b"\n").await?;
    }
    Ok(())
}

/// Run the same journal operation for an embedded native shell as for a local
/// daemon client. Process shutdown remains exclusive to the socket server.
pub async fn request(node: &Node, method: &str, params: Value) -> Result<Value> {
    if method == "shutdown" {
        bail!("embedded journal shutdown belongs to its owner");
    }
    let (shutdown, _) = tokio::sync::watch::channel(false);
    dispatch(
        node,
        Request {
            id: 0,
            method: method.into(),
            params,
        },
        &shutdown,
    )
    .await
}

async fn dispatch(
    node: &Node,
    request: Request,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
) -> Result<Value> {
    match request.method.as_str() {
        "health" => Ok(
            json!({ "device_id": node.journal().device_id(), "version": crate::VERSION, "pid": std::process::id() }),
        ),
        "capture_text" => {
            let text = request.params["text"].as_str().unwrap_or("");
            let recorded_at = request.params["recorded_at"]
                .as_i64()
                .unwrap_or_else(crate::store::now_ms);
            // A malformed client must not accidentally place an entry centuries away.
            let now = crate::store::now_ms();
            if !(now - 366_i64 * 86_400_000..=now + 86_400_000).contains(&recorded_at) {
                bail!("recorded_at must be within one year past or one day future");
            }
            let event = node.journal().capture_text_at(text, recorded_at)?;
            Ok(crate::api_json::entry_json(&event))
        }
        "capture_media" => {
            let kind = request.params["kind"].as_str().unwrap_or("");
            let encoded = request.params["bytes_b64"].as_str().unwrap_or("");
            let bytes = data_encoding::BASE64
                .decode(encoded.as_bytes())
                .context("media bytes are not base64")?;
            let event = match kind {
                "photo" => {
                    node.capture_blob(MediaKind::Photo, crate::media::normalize_photo(&bytes)?)
                        .await?
                }
                "audio" => {
                    if !crate::media::is_mp4_family(&bytes) {
                        bail!("audio must be an m4a/mp4 recording");
                    }
                    node.capture_audio_with_intent(
                        bytes,
                        AudioKind::Voice,
                        request.params["will_enrich"].as_bool().unwrap_or(false),
                    )
                    .await?
                }
                "music" => {
                    if !crate::media::is_mp4_family(&bytes) {
                        bail!("audio must be an m4a/mp4 recording");
                    }
                    node.capture_audio(bytes, AudioKind::Music).await?
                }
                "video" => {
                    if !crate::media::is_mp4_family(&bytes) {
                        bail!("video must be an mp4 recording");
                    }
                    node.capture_blob(MediaKind::Video, bytes).await?
                }
                _ => bail!("unknown media kind {kind:?}"),
            };
            Ok(crate::api_json::entry_json(&event))
        }
        "feed" => {
            let before = request.params["before"].as_i64();
            let limit = request.params["limit"].as_u64().unwrap_or(50).min(500) as usize;
            let annotations = node.journal().annotations()?;
            let mut entries = node.journal().list()?;
            entries.reverse();
            let page: Vec<_> = entries
                .iter()
                .filter(|e| before.map(|b| e.recorded_at < b).unwrap_or(true))
                .skip(request.params["offset"].as_u64().unwrap_or(0) as usize)
                .take(limit)
                .map(|e| crate::api_json::entry_json_annotated(e, &annotations))
                .collect();
            let next_before = page.last().and_then(|e| e["recorded_at"].as_i64());
            Ok(json!({"entries": page, "next_before": next_before}))
        }
        "media_bytes" => {
            let hash = request.params["hash"].as_str().unwrap_or("");
            let bytes = node.blob_bytes(hash).await?;
            Ok(json!({"bytes_b64": data_encoding::BASE64.encode(&bytes)}))
        }
        "trash" => {
            let mut entries = node.journal().trash()?;
            entries.reverse();
            let annotations = node.journal().annotations()?;
            Ok(
                json!({"entries": entries.iter().map(|e| crate::api_json::entry_json_annotated(e, &annotations)).collect::<Vec<_>>() }),
            )
        }
        "ping_peers" => {
            Ok(json!({"pings": node.ping_peers(std::time::Duration::from_secs(4)).await?}))
        }
        "search" => {
            let query = request.params["query"].as_str().unwrap_or("");
            let journal = node.journal();
            let annotations = journal.annotations()?;
            let redacted = journal.store.redacted_ids()?;
            let mut entries = Vec::new();
            for id in journal.store.search(query)? {
                let Some(event) = journal.store.get_event(&id)? else {
                    continue;
                };
                let display = match &event.payload {
                    crate::Payload::Annotation { target, .. } => journal.store.get_event(target)?,
                    _ => Some(event),
                };
                if let Some(event) = display {
                    if event.kind == crate::EventKind::Capture
                        && !redacted.contains(&event.event_id)
                        && !entries
                            .iter()
                            .any(|entry: &Value| entry["event_id"] == event.event_id.as_str())
                    {
                        entries.push(crate::api_json::entry_json_annotated(&event, &annotations));
                    }
                }
            }
            Ok(json!({"entries": entries}))
        }
        "annotate_voice" => {
            let id = request.params["event_id"].as_str().unwrap_or("");
            let event = node
                .journal()
                .store
                .get_event(id)?
                .context("voice entry not found")?;
            if !matches!(event.payload, crate::Payload::Audio { .. })
                || !crate::retention::wants_transcription(&event)
            {
                bail!("only voice captures may receive a transcription");
            }
            node.journal()
                .annotate(id, request.params["text"].as_str().unwrap_or(""))?;
            Ok(json!({}))
        }
        "redact" => {
            let id = request.params["event_id"].as_str().unwrap_or("");
            Ok(crate::api_json::entry_json(&node.journal().redact(id)?))
        }
        "status" => node.status_json().await,
        "set_device_name" => {
            node.journal().set_device_name(
                request.params["device_id"].as_str().unwrap_or(""),
                request.params["name"].as_str().unwrap_or(""),
            )?;
            Ok(json!({}))
        }
        "set_net_config" => {
            let net = serde_json::from_value(request.params["net"].clone())
                .context("invalid network config")?;
            node.journal().set_net_config(&net)?;
            Ok(json!({}))
        }
        "list_agents" => Ok(serde_json::to_value(node.agent_keys()?)?),
        "mint_agent" => {
            let name = request.params["name"].as_str().unwrap_or("").trim();
            if name.is_empty() {
                bail!("agent name is empty");
            }
            Ok(Value::String(node.mint_agent_ticket(name).await?))
        }
        "revoke_agent" => {
            node.revoke_agent_key(request.params["id"].as_str().unwrap_or(""))?;
            Ok(json!({}))
        }
        "check_master_password" => {
            node.journal()
                .check_master_password(request.params["password"].as_str().unwrap_or(""))?;
            Ok(json!({}))
        }
        "rotate_master_password" => {
            node.journal()
                .rotate_master_password(request.params["password"].as_str().unwrap_or(""))?;
            Ok(json!({}))
        }
        "adopt_master_password" => {
            node.journal()
                .adopt_master_password(request.params["password"].as_str().unwrap_or(""))?;
            Ok(json!({}))
        }
        "export_markdown" => {
            let out = PathBuf::from(request.params["out"].as_str().unwrap_or(""));
            if out.as_os_str().is_empty() {
                bail!("missing export destination");
            }
            let report = crate::export_md::export_markdown(node, &out).await?;
            Ok(
                json!({"path": out, "days": report.day_files_written + report.day_files_unchanged, "media": report.media_written + report.media_unchanged}),
            )
        }
        "sync" => {
            let explicit_ticket = request.params["ticket"].as_str().unwrap_or("").trim();
            if explicit_ticket.is_empty() {
                let peers = node.sync_saved_peers().await?;
                if !peers.is_empty() {
                    if peers.iter().all(|peer| !peer.ok) {
                        let failures = peers
                            .iter()
                            .map(|peer| {
                                format!(
                                    "{}: {}",
                                    &peer.endpoint_id[..10.min(peer.endpoint_id.len())],
                                    peer.error.as_deref().unwrap_or("unreachable")
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let hint = if node.journal().net_config().relay_mode == "disabled" {
                            "\nRelays are disabled. For peers on other networks, enable relays in network settings and restart this journal, or use a fresh ticket with a reachable direct address."
                        } else {
                            "\nMake sure a peer is running, or paste a fresh ticket from it."
                        };
                        bail!("Could not sync any known peer:\n{failures}{hint}");
                    }
                    return Ok(json!({
                        "sent": peers.iter().map(|peer| peer.sent).sum::<usize>(),
                        "received": peers.iter().map(|peer| peer.received).sum::<usize>(),
                        "blobs": peers.iter().map(|peer| peer.blobs).sum::<usize>(),
                        "peers": peers,
                    }));
                }
            }
            let ticket = request.params["ticket"]
                .as_str()
                .filter(|ticket| !ticket.trim().is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    node.journal()
                        .store
                        .meta_get(LAST_PEER_TICKET)
                        .ok()
                        .flatten()
                        .and_then(|v| String::from_utf8(v).ok())
                })
                .context("no known peer — paste a ticket")?;
            let ticket = JournalTicket::decode(ticket.trim())?;
            if ticket.addr()?.id == node.endpoint().id() {
                bail!("This is this device’s own ticket. Paste the ticket from another device in the same journal.");
            }
            if ticket.secret != *node.journal().secret() {
                bail!("ticket is for a different journal");
            }
            let report = node.sync_with(&ticket.addr()?).await?;
            node.journal()
                .store
                .meta_set(LAST_PEER_TICKET, ticket.encode().as_bytes())?;
            Ok(json!({
                "sent": report.sent,
                "received": report.received,
                "blobs": report.blobs_fetched,
            }))
        }
        "shutdown" => {
            shutdown_tx.send(true).context("daemon shutdown")?;
            Ok(json!({"stopping": true}))
        }
        _ => bail!("unknown local RPC method {:?}", request.method),
    }
}

/// Make one local RPC call.  This is used by the CLI and is intentionally
/// public so optional sidecars can use the exact same bounded protocol.
pub async fn call(socket: &Path, method: &str, params: Value) -> Result<Value> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect {}", socket.display()))?;
    let (read, mut write) = stream.into_split();
    let request = Request {
        id: 1,
        method: method.into(),
        params,
    };
    write
        .write_all(serde_json::to_string(&request)?.as_bytes())
        .await?;
    write.write_all(b"\n").await?;
    let mut lines = BufReader::new(read).lines();
    let line = lines
        .next_line()
        .await?
        .context("daemon closed before responding")?;
    let response: Response = serde_json::from_str(&line).context("invalid daemon response")?;
    if let Some(error) = response.error {
        bail!("{error}");
    }
    response
        .result
        .context("daemon response has neither result nor error")
}

pub fn default_socket() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir())
        .join("memorious")
        .join("node.sock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Payload;
    use crate::Journal;
    use tempfile::tempdir;

    async fn quiet_journal(root: &Path) -> Journal {
        let journal = Journal::init(root, "password").unwrap();
        journal
            .set_net_config(&crate::node::NetConfig {
                relay_mode: "disabled".into(),
                relay_urls: vec![],
                public_lookup: false,
            })
            .unwrap();
        journal
    }

    #[tokio::test]
    async fn sync_without_a_ticket_uses_peers_that_connected_inbound() {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        let first = Node::spawn(quiet_journal(a.path()).await).await.unwrap();
        // Pairing establishes an inbound contact; the first node never pasted a ticket.
        let (second, _) = Node::pair_from_ticket(b.path(), &first.ticket().unwrap(), "password")
            .await
            .unwrap();
        second
            .journal()
            .capture_text("arrived after pairing")
            .unwrap();
        assert!(first
            .journal()
            .store
            .meta_get(LAST_PEER_TICKET)
            .unwrap()
            .is_none());
        let result = request(&first, "sync", json!({})).await.unwrap();
        assert_eq!(result["received"], 1);
        assert_eq!(result["peers"][0]["ok"], true);
        assert_eq!(first.journal().timeline().unwrap().entries, 1);
        second.shutdown().await;
        first.shutdown().await;
    }

    #[tokio::test]
    async fn sync_stale_last_ticket_does_not_skip_another_reachable_peer() {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        let c = tempdir().unwrap();
        let first = Node::spawn(quiet_journal(a.path()).await).await.unwrap();
        let (offline, _) = Node::pair_from_ticket(b.path(), &first.ticket().unwrap(), "password")
            .await
            .unwrap();
        let stale_ticket = offline.ticket().unwrap();
        let (online, _) = Node::pair_from_ticket(c.path(), &first.ticket().unwrap(), "password")
            .await
            .unwrap();
        offline.shutdown().await;
        first
            .journal()
            .store
            .meta_set(LAST_PEER_TICKET, stale_ticket.as_bytes())
            .unwrap();
        online
            .journal()
            .capture_text("from the reachable peer")
            .unwrap();
        let result = request(&first, "sync", json!({})).await.unwrap();
        assert_eq!(result["received"], 1);
        let peers = result["peers"].as_array().unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers.iter().filter(|peer| peer["ok"] == true).count(), 1);
        assert!(peers.iter().any(|peer| peer["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())));
        online.shutdown().await;
        first.shutdown().await;
    }

    #[tokio::test]
    async fn sync_rejects_this_devices_own_ticket_before_dialling() {
        let dir = tempdir().unwrap();
        let node = Node::spawn(quiet_journal(dir.path()).await).await.unwrap();
        let error = request(&node, "sync", json!({"ticket":node.ticket().unwrap()}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("this device"), "{error:#}");
        node.shutdown().await;
    }

    #[tokio::test]
    async fn embedded_requests_share_daemon_behavior_without_shutdown() {
        let dir = tempdir().unwrap();
        let node = Node::spawn(Journal::init(dir.path(), "password").unwrap())
            .await
            .unwrap();
        request(&node, "capture_text", json!({"text": "native capture"}))
            .await
            .unwrap();
        assert_eq!(
            request(&node, "feed", json!({})).await.unwrap()["entries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        request(&node, "capture_text", json!({"text":"second capture"}))
            .await
            .unwrap();
        let offset = request(&node, "feed", json!({"offset":1, "limit":1}))
            .await
            .unwrap();
        assert_eq!(offset["entries"][0]["text"], "native capture");
        assert!(request(&node, "shutdown", json!({})).await.is_err());
        node.shutdown().await;
    }

    #[tokio::test]
    async fn native_search_and_trash_keep_the_winning_annotation() {
        let dir = tempdir().unwrap();
        let node = Node::spawn(Journal::init(dir.path(), "password").unwrap())
            .await
            .unwrap();
        let event = node.journal().capture_text("searchable note").unwrap();
        node.journal()
            .annotate(&event.event_id, "searchable annotation")
            .unwrap();
        let found = request(&node, "search", json!({"query":"searchable"}))
            .await
            .unwrap();
        assert_eq!(found["entries"].as_array().unwrap().len(), 1);
        assert_eq!(found["entries"][0]["annotation"], "searchable annotation");
        node.journal().redact(&event.event_id).unwrap();
        assert_eq!(
            request(&node, "trash", json!({})).await.unwrap()["entries"][0]["annotation"],
            "searchable annotation"
        );
        node.shutdown().await;
    }

    #[tokio::test]
    async fn native_voice_transcripts_are_annotations_and_music_is_never_transcribed() {
        let dir = tempdir().unwrap();
        let node = Node::spawn(Journal::init(dir.path(), "password").unwrap())
            .await
            .unwrap();
        let voice = node
            .capture_audio(vec![1, 2, 3], AudioKind::Voice)
            .await
            .unwrap();
        request(
            &node,
            "annotate_voice",
            json!({"event_id":voice.event_id,"text":"local transcript"}),
        )
        .await
        .unwrap();
        assert_eq!(
            node.journal().annotations().unwrap()[&voice.event_id],
            "local transcript"
        );
        let music = node
            .capture_audio(vec![4, 5, 6], AudioKind::Music)
            .await
            .unwrap();
        assert!(request(
            &node,
            "annotate_voice",
            json!({"event_id":music.event_id,"text":"must not happen"})
        )
        .await
        .is_err());
        node.shutdown().await;
    }

    #[test]
    fn journal_lock_excludes_another_owner() {
        let dir = tempdir().unwrap();
        let _first = JournalLock::acquire(dir.path()).unwrap();
        assert!(JournalLock::acquire(dir.path()).is_err());
    }

    #[test]
    fn explicit_capture_time_is_canonical_event_time() {
        let dir = tempdir().unwrap();
        let journal = Journal::init(dir.path(), "password").unwrap();
        let event = journal
            .capture_text_at("spoken while offline", 1_700_000_000_000)
            .unwrap();
        assert_eq!(event.recorded_at, 1_700_000_000_000);
        assert!(
            matches!(event.payload, Payload::Text { ref text } if text == "spoken while offline")
        );
    }

    #[tokio::test]
    async fn daemon_socket_captures_spoken_time_without_opening_a_second_journal() {
        let dir = tempdir().unwrap();
        Journal::init(dir.path(), "password").unwrap();
        let socket = dir.path().join("daemon.sock");
        let daemon = Daemon::open(dir.path(), "password").await.unwrap();
        let task = tokio::spawn(daemon.serve(socket.clone()));
        for _ in 0..40 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let spoken_at = crate::store::now_ms() - 5_000;
        let result = call(
            &socket,
            "capture_text",
            json!({
                "text": "captured through the sidecar seam",
                "recorded_at": spoken_at,
            }),
        )
        .await
        .unwrap();
        assert_eq!(result["recorded_at"], spoken_at);
        let entries = call(&socket, "feed", json!({})).await.unwrap();
        assert_eq!(entries["entries"].as_array().unwrap().len(), 1);
        assert_eq!(
            call(&socket, "shutdown", json!({})).await.unwrap()["stopping"],
            true
        );
        task.await.unwrap().unwrap();
        assert!(!socket.exists());
    }
}
