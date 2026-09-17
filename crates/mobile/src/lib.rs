//! UniFFI face of the core for the iOS app. Thin and boring: methods block on a
//! private tokio runtime and hand JSON strings across the FFI (same shapes as
//! the HTTP API / Tauri commands), media as raw bytes.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use memorious_core::api_json::{entry_json, entry_json_annotated};
use memorious_core::event::{AudioKind, MediaKind};
use memorious_core::retention::RetentionPolicy;
use memorious_core::node::JournalTicket;
use memorious_core::{Journal, Node};
use serde_json::json;

uniffi::setup_scaffolding!();

const LAST_PEER_TICKET: &str = "last_peer_ticket";

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum JournalError {
    #[error("{msg}")]
    Failure { msg: String },
}

impl From<anyhow::Error> for JournalError {
    fn from(e: anyhow::Error) -> Self {
        Self::Failure {
            msg: format!("{e:#}"),
        }
    }
}

type Result<T> = std::result::Result<T, JournalError>;

fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// "ready" if a journal exists at `dir`, else "empty".
#[uniffi::export]
pub fn setup_state(dir: String) -> String {
    if PathBuf::from(dir).join("db.sqlite").exists() {
        "ready".into()
    } else {
        "empty".into()
    }
}

#[derive(uniffi::Object)]
pub struct MobileJournal {
    node: Arc<Node>,
}

fn spawn_node(journal: Journal) -> Result<Arc<Node>> {
    Ok(Arc::new(rt().block_on(Node::spawn(journal))?))
}

/// A phone frees space: transcribed voice notes and redacted media are
/// evicted after `RetentionPolicy::PHONE`'s windows (the always-on server
/// keeps everything). Installed once; a policy set later via
/// `set_retention_policy_json` is never overridden. Rules live in
/// crates/core/src/retention.rs.
fn ensure_phone_retention(journal: &Journal) -> Result<()> {
    if journal.retention_policy_if_set()?.is_none() {
        journal.set_retention_policy(&RetentionPolicy::PHONE)?;
    }
    Ok(())
}

/// This face only ships on the iPhone today; revisit when an iPad/Android
/// build exists.
const DEFAULT_DEVICE_NAME: &str = "iPhone";

/// The app keeps the master password in the iOS Keychain and passes it on
/// every open — the engine re-derives keys each time (a few hundred ms).
#[uniffi::export]
pub fn open_journal(dir: String, password: String) -> Result<Arc<MobileJournal>> {
    let journal = Journal::open(&PathBuf::from(dir), &password)?;
    journal.ensure_device_name(DEFAULT_DEVICE_NAME)?;
    journal.ensure_peer_join()?;
    journal.ensure_version_seen()?;
    ensure_phone_retention(&journal)?;
    let node = spawn_node(journal)?;
    Ok(Arc::new(MobileJournal { node }))
}

#[uniffi::export]
pub fn init_fresh(dir: String, password: String) -> Result<Arc<MobileJournal>> {
    let journal = Journal::init(&PathBuf::from(dir), &password)?;
    journal.ensure_device_name(DEFAULT_DEVICE_NAME)?;
    journal.ensure_peer_join()?;
    journal.ensure_version_seen()?;
    ensure_phone_retention(&journal)?;
    let node = spawn_node(journal)?;
    Ok(Arc::new(MobileJournal { node }))
}

/// Join an existing journal from a pairing ticket. Pairing pulls the event log
/// and proves the password, but defers media to the next `sync_now` — the app
/// runs that in the background so joining isn't blocked by the media fetch.
#[uniffi::export]
pub fn join_ticket(dir: String, ticket: String, password: String) -> Result<Arc<MobileJournal>> {
    let (node, _report) =
        rt().block_on(Node::pair_from_ticket(&PathBuf::from(dir), &ticket, &password))?;
    node.journal().ensure_device_name(DEFAULT_DEVICE_NAME)?;
    node.journal().ensure_peer_join()?;
    node.journal().ensure_version_seen()?;
    ensure_phone_retention(node.journal())?;
    node.journal()
        .store
        .meta_set(LAST_PEER_TICKET, ticket.trim().as_bytes())
        .map_err(JournalError::from)?;
    Ok(Arc::new(MobileJournal {
        node: Arc::new(node),
    }))
}

impl MobileJournal {
    /// `media.evicted = true` on entries whose blob this phone no longer holds
    /// (retention.rs) — the UI shows the transcript with a "pruned" marker
    /// instead of a play button that would fail.
    fn mark_evicted(&self, entries: &mut [serde_json::Value]) {
        for e in entries.iter_mut() {
            let Some(hash) = e["media"]["hash"].as_str().map(str::to_string) else { continue };
            if let Ok(false) = rt().block_on(self.node.has_blob(&hash)) {
                e["media"]["evicted"] = true.into();
            }
        }
    }

    fn capture_audio_kind(&self, bytes: Vec<u8>, kind: AudioKind) -> Result<String> {
        if !memorious_core::media::is_mp4_family(&bytes) {
            return Err(JournalError::Failure {
                msg: "audio must be an m4a recording".into(),
            });
        }
        let e = rt().block_on(self.node.capture_audio(bytes, kind))?;
        Ok(entry_json(&e).to_string())
    }
}

#[uniffi::export]
impl MobileJournal {
    pub fn capture_text(&self, text: String) -> Result<String> {
        let e = self.node.journal().capture_text(&text)?;
        Ok(entry_json(&e).to_string())
    }

    /// Any decodable image in; JPEG stored.
    pub fn capture_photo(&self, bytes: Vec<u8>) -> Result<String> {
        let jpeg = memorious_core::media::normalize_photo(&bytes)?;
        let e = rt().block_on(self.node.capture_blob(MediaKind::Photo, jpeg))?;
        Ok(entry_json(&e).to_string())
    }

    /// iOS records AAC/m4a natively; anything else is refused. This is a
    /// voice note: transcribed by a peer, audio evictable afterwards.
    pub fn capture_audio(&self, bytes: Vec<u8>) -> Result<String> {
        self.capture_audio_kind(bytes, AudioKind::Voice)
    }

    /// A music recording: the audio *is* the record — never transcribed,
    /// never evicted (crates/core/src/retention.rs).
    pub fn capture_music(&self, bytes: Vec<u8>) -> Result<String> {
        self.capture_audio_kind(bytes, AudioKind::Music)
    }

    /// A voice note this phone intends to transcribe itself (on-device
    /// speech): `will_enrich` makes peers hold off for the grace period
    /// (enrich.rs). If the phone never annotates, a peer does after that.
    pub fn capture_voice_with_intent(&self, bytes: Vec<u8>, will_enrich: bool) -> Result<String> {
        if !memorious_core::media::is_mp4_family(&bytes) {
            return Err(JournalError::Failure {
                msg: "audio must be an m4a recording".into(),
            });
        }
        let e = rt().block_on(self.node.capture_audio_with_intent(
            bytes,
            AudioKind::Voice,
            will_enrich,
        ))?;
        Ok(entry_json(&e).to_string())
    }

    /// The phone's own transcript for a capture — the same annotation a
    /// peer would write. Latest annotation wins, as everywhere.
    pub fn annotate(&self, target_event_id: String, text: String) -> Result<String> {
        let e = self.node.journal().annotate(&target_event_id, &text)?;
        Ok(entry_json(&e).to_string())
    }

    /// This device's retention policy as JSON (see retention.rs for the shape).
    pub fn retention_policy_json(&self) -> Result<String> {
        let p = self.node.journal().retention_policy().map_err(JournalError::from)?;
        Ok(serde_json::to_string(&p).map_err(|e| JournalError::Failure { msg: e.to_string() })?)
    }

    pub fn set_retention_policy_json(&self, json: String) -> Result<()> {
        let p: RetentionPolicy =
            serde_json::from_str(&json).map_err(|e| JournalError::Failure { msg: e.to_string() })?;
        self.node.journal().set_retention_policy(&p)?;
        Ok(())
    }

    /// H.264/AAC MP4 — the canonical video format; anything else is refused.
    /// (The app exports camera footage to MP4 before handing it over.)
    pub fn capture_video(&self, bytes: Vec<u8>) -> Result<String> {
        if !memorious_core::media::is_mp4_family(&bytes) {
            return Err(JournalError::Failure {
                msg: "video must be an mp4 recording".into(),
            });
        }
        let e = rt().block_on(self.node.capture_blob(MediaKind::Video, bytes))?;
        Ok(entry_json(&e).to_string())
    }

    /// Probe every known peer (~4s worst case; blocking like the rest of this
    /// face — call it off the main thread). Same JSON shape as the server's
    /// POST /api/peers/ping.
    pub fn ping_peers_json(&self) -> Result<String> {
        let pings = rt()
            .block_on(self.node.ping_peers(std::time::Duration::from_secs(4)))?;
        Ok(serde_json::json!({ "pings": pings }).to_string())
    }

    pub fn feed(&self, before: Option<i64>, limit: u32) -> Result<String> {
        let annotations = self.node.journal().annotations().map_err(JournalError::from)?;
        let mut entries = self.node.journal().list().map_err(JournalError::from)?;
        entries.reverse();
        let mut page: Vec<_> = entries
            .iter()
            .filter(|e| before.map(|b| e.recorded_at < b).unwrap_or(true))
            .take(limit.clamp(1, 500) as usize)
            .map(|e| entry_json_annotated(e, &annotations))
            .collect();
        self.mark_evicted(&mut page);
        let next_before = page.last().and_then(|e| e["recorded_at"].as_i64());
        Ok(json!({"entries": page, "next_before": next_before}).to_string())
    }

    pub fn media_bytes(&self, hash: String) -> Result<Vec<u8>> {
        Ok(rt().block_on(self.node.blob_bytes(&hash))?)
    }

    pub fn redact(&self, event_id: String) -> Result<()> {
        self.node.journal().redact(&event_id)?;
        Ok(())
    }

    pub fn trash_json(&self) -> Result<String> {
        let mut entries = self.node.journal().trash().map_err(JournalError::from)?;
        entries.reverse();
        Ok(json!({"entries": entries.iter().map(entry_json).collect::<Vec<_>>()}).to_string())
    }

    pub fn search(&self, q: String) -> Result<String> {
        let journal = self.node.journal();
        let redacted = journal.store.redacted_ids().map_err(JournalError::from)?;
        let mut out = Vec::new();
        for id in journal.store.search(&q).map_err(JournalError::from)? {
            if let Some(e) = journal.store.get_event(&id).map_err(JournalError::from)? {
                let display = match &e.payload {
                    memorious_core::Payload::Annotation { target, .. } => {
                        journal.store.get_event(target).map_err(JournalError::from)?
                    }
                    _ => Some(e),
                };
                if let Some(e) = display {
                    if e.kind == memorious_core::EventKind::Capture && !redacted.contains(&e.event_id)
                    {
                        out.push(entry_json(&e));
                    }
                }
            }
        }
        Ok(json!({"entries": out}).to_string())
    }

    /// The shared status shape (device, stats, names, peers, net config) —
    /// same JSON the HTTP API and Tauri commands return.
    pub fn status_json(&self) -> Result<String> {
        Ok(rt().block_on(self.node.status_json())?.to_string())
    }

    /// Name a device (this one or any peer). Editable; latest wins everywhere.
    pub fn set_device_name(&self, device_id: String, name: String) -> Result<()> {
        self.node.journal().set_device_name(&device_id, &name)?;
        Ok(())
    }

    /// Store the network config from its JSON form. Applied on next launch.
    pub fn set_net_config(&self, json: String) -> Result<()> {
        let cfg: memorious_core::node::NetConfig =
            serde_json::from_str(&json).map_err(|e| JournalError::Failure {
                msg: format!("bad net config: {e}"),
            })?;
        self.node.journal().set_net_config(&cfg)?;
        Ok(())
    }

    /// Originate a master-password change on this device: re-wraps existing
    /// media, re-keys the local database, and starts using it immediately.
    /// Every other device must separately call `adopt_master_password` once
    /// told the new password (out-of-band — it never travels in a ticket).
    pub fn rotate_master_password(&self, password: String) -> Result<()> {
        self.node.journal().rotate_master_password(&password)?;
        Ok(())
    }

    /// Catch up to a rotation another device already published. Rejects a
    /// wrong guess (verified against that device's password proof) without
    /// changing anything.
    pub fn adopt_master_password(&self, password: String) -> Result<()> {
        self.node.journal().adopt_master_password(&password)?;
        Ok(())
    }

    /// Traffic-light replication state: {"color","pending","stalest_ms","peers"}.
    pub fn sync_health(&self) -> Result<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let h = self.node.journal().sync_health(now).map_err(JournalError::from)?;
        Ok(serde_json::to_string(&h).map_err(anyhow::Error::from)?)
    }

    pub fn my_ticket(&self) -> Result<String> {
        rt().block_on(self.node.dialable_addr())?;
        Ok(self.node.ticket()?)
    }

    /// Write the markdown mirror (`YYYY/MM/DD.md` + `media/`, see
    /// core::export_md) into `out_dir`; the app zips and shares it. Media this
    /// phone let go of by retention is skipped, not an error. Returns a JSON
    /// report `{ "days": n, "media": n }`.
    pub fn export_markdown(&self, out_dir: String) -> Result<String> {
        let report = rt().block_on(memorious_core::export_md::export_markdown(
            &self.node,
            &PathBuf::from(out_dir),
        ))?;
        Ok(json!({
            "days": report.day_files_written + report.day_files_unchanged,
            "media": report.media_written + report.media_unchanged,
        })
        .to_string())
    }

    /// Release the network endpoint, the blob store and its database, so the
    /// app can delete the data dir (reset this device). The object is dead
    /// afterwards; drop it.
    pub fn close(&self) {
        rt().block_on(self.node.shutdown_ref());
    }

    /// Sync with the ticket's peer (or the last-used one). Returns a report JSON.
    pub fn sync_now(&self, ticket: Option<String>) -> Result<String> {
        let journal = self.node.journal();
        let ticket_str = match ticket {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => journal
                .store
                .meta_get(LAST_PEER_TICKET)
                .map_err(JournalError::from)?
                .and_then(|b| String::from_utf8(b).ok())
                .ok_or_else(|| JournalError::Failure {
                    msg: "no known peer — pair first".into(),
                })?,
        };
        let t = JournalTicket::decode(&ticket_str).map_err(JournalError::from)?;
        if &t.secret != journal.secret() {
            return Err(JournalError::Failure {
                msg: "ticket is for a different journal".into(),
            });
        }
        let addr = t.addr().map_err(JournalError::from)?;
        let report = rt().block_on(self.node.sync_with(&addr))?;
        journal
            .store
            .meta_set(LAST_PEER_TICKET, ticket_str.as_bytes())
            .map_err(JournalError::from)?;
        Ok(json!({
            "sent": report.sent,
            "received": report.received,
            "blobs": report.blobs_fetched,
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_surface_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("j").to_string_lossy().to_string();
        assert_eq!(setup_state(d.clone()), "empty");
        let j = init_fresh(d.clone(), "pw".into()).unwrap();
        assert_eq!(setup_state(d.clone()), "ready");
        // Wrong password never opens the journal.
        assert!(open_journal(d.clone(), "nope".into()).is_err());
        let e: serde_json::Value =
            serde_json::from_str(&j.capture_text("from ffi".into()).unwrap()).unwrap();
        assert_eq!(e["kind"], "text");
        let feed: serde_json::Value = serde_json::from_str(&j.feed(None, 50).unwrap()).unwrap();
        assert_eq!(feed["entries"].as_array().unwrap().len(), 1);
        let ticket = j.my_ticket().unwrap();
        assert!(ticket.starts_with("memorious"));

        // Second device joins by ticket and converges.
        let d2 = dir.path().join("j2").to_string_lossy().to_string();
        let j2 = join_ticket(d2, ticket, "pw".into()).unwrap();
        let feed2: serde_json::Value = serde_json::from_str(&j2.feed(None, 50).unwrap()).unwrap();
        assert_eq!(feed2["entries"], feed["entries"]);
        // and sync_now uses the remembered ticket
        // (4 sent: the "reply" capture, this device's default-name
        // annotation, its peer-join infra event, and its version-seen
        // infra event — all events, all sync like everything else)
        j2.capture_text("reply".into()).unwrap();
        let report: serde_json::Value =
            serde_json::from_str(&j2.sync_now(None).unwrap()).unwrap();
        assert_eq!(report["sent"], 4);

        // Status carries the sync-page surface; the default name is set and
        // editable over the FFI.
        let status: serde_json::Value =
            serde_json::from_str(&j2.status_json().unwrap()).unwrap();
        let me = status["device_id"].as_str().unwrap().to_string();
        assert_eq!(status["names"][&me], "iPhone");
        assert!(status["storage"]["db_bytes"].as_u64().unwrap() > 0);
        assert_eq!(status["peers"][0]["discovery"], "ticket");
        assert_eq!(status["peers"][0]["conn"]["proxied"], false);
        j2.set_device_name(me.clone(), "Josh's phone".into()).unwrap();
        let status: serde_json::Value =
            serde_json::from_str(&j2.status_json().unwrap()).unwrap();
        assert_eq!(status["names"][&me], "Josh's phone");
        assert_eq!(status["net"]["relay_mode"], "default");
        j2.set_net_config(r#"{"relay_mode":"disabled","relay_urls":[],"public_lookup":false}"#.into())
            .unwrap();
        assert!(j2.set_net_config("not json".into()).is_err());
    }
}
