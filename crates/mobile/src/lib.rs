//! UniFFI face of the core for the iOS app. Thin and boring: methods block on a
//! private tokio runtime and hand JSON strings across the FFI (same shapes as
//! the HTTP API / Tauri commands), media as raw bytes.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use memorious_core::api_json::{entry_json, entry_json_annotated};
use memorious_core::event::MediaKind;
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

/// The app keeps the master password in the iOS Keychain and passes it on
/// every open — the engine re-derives keys each time (a few hundred ms).
#[uniffi::export]
pub fn open_journal(dir: String, password: String) -> Result<Arc<MobileJournal>> {
    let node = spawn_node(Journal::open(&PathBuf::from(dir), &password)?)?;
    Ok(Arc::new(MobileJournal { node }))
}

#[uniffi::export]
pub fn init_fresh(dir: String, password: String) -> Result<Arc<MobileJournal>> {
    let node = spawn_node(Journal::init(&PathBuf::from(dir), &password)?)?;
    Ok(Arc::new(MobileJournal { node }))
}

/// Join an existing journal from a pairing ticket. Pairing pulls the event log
/// and proves the password, but defers media to the next `sync_now` — the app
/// runs that in the background so joining isn't blocked by the media fetch.
#[uniffi::export]
pub fn join_ticket(dir: String, ticket: String, password: String) -> Result<Arc<MobileJournal>> {
    let (node, _report) =
        rt().block_on(Node::pair_from_ticket(&PathBuf::from(dir), &ticket, &password))?;
    node.journal()
        .store
        .meta_set(LAST_PEER_TICKET, ticket.trim().as_bytes())
        .map_err(JournalError::from)?;
    Ok(Arc::new(MobileJournal {
        node: Arc::new(node),
    }))
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

    /// iOS records AAC/m4a natively; anything else is refused.
    pub fn capture_audio(&self, bytes: Vec<u8>) -> Result<String> {
        if !memorious_core::media::is_mp4_family(&bytes) {
            return Err(JournalError::Failure {
                msg: "audio must be an m4a recording".into(),
            });
        }
        let e = rt().block_on(self.node.capture_blob(MediaKind::Audio, bytes))?;
        Ok(entry_json(&e).to_string())
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

    pub fn feed(&self, before: Option<i64>, limit: u32) -> Result<String> {
        let annotations = self.node.journal().annotations().map_err(JournalError::from)?;
        let mut entries = self.node.journal().list().map_err(JournalError::from)?;
        entries.reverse();
        let page: Vec<_> = entries
            .iter()
            .filter(|e| before.map(|b| e.recorded_at < b).unwrap_or(true))
            .take(limit.clamp(1, 500) as usize)
            .map(|e| entry_json_annotated(e, &annotations))
            .collect();
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

    pub fn status_json(&self) -> Result<String> {
        let journal = self.node.journal();
        let mut v = json!({
            "device_id": journal.device_id(),
            "entries": journal.list().map_err(JournalError::from)?.len(),
            "trash": journal.trash().map_err(JournalError::from)?.len(),
            "heads": journal.store.heads().map_err(JournalError::from)?,
        });
        if let Ok(t) = self.node.ticket() {
            v["ticket"] = t.into();
        }
        Ok(v.to_string())
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
        j2.capture_text("reply".into()).unwrap();
        let report: serde_json::Value =
            serde_json::from_str(&j2.sync_now(None).unwrap()).unwrap();
        assert_eq!(report["sent"], 1);
    }
}
