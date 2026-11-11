//! Thin native desktop adapter. Storage, sync, encryption and search stay in core.
pub mod background;
#[cfg(feature = "gui")]
pub mod tray;
use anyhow::{bail, Result};
use memorious_core::{
    api_json::{entry_json, entry_json_annotated},
    event::{AudioKind, MediaKind},
    local::JournalLock,
    Journal, Node,
};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

pub struct DesktopJournal {
    node: Arc<Node>,
    _lock: JournalLock,
    sync_tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}
impl DesktopJournal {
    pub async fn create(dir: &Path, password: &str) -> Result<Self> {
        let lock = JournalLock::acquire(dir)?;
        Self::spawn(Journal::init(dir, password)?, lock).await
    }
    pub async fn open(dir: &Path, password: &str) -> Result<Self> {
        let lock = JournalLock::acquire(dir)?;
        Self::spawn(Journal::open(dir, password)?, lock).await
    }
    async fn spawn(journal: Journal, lock: JournalLock) -> Result<Self> {
        journal.ensure_device_name("desktop (Linux)")?;
        journal.ensure_peer_join()?;
        journal.ensure_version_seen()?;
        Ok(Self {
            node: Arc::new(Node::spawn(journal).await?),
            _lock: lock,
            sync_tasks: Default::default(),
        })
    }
    pub async fn join(dir: &Path, ticket: &str, password: &str) -> Result<Self> {
        let lock = JournalLock::acquire(dir)?;
        let (node, _) = Node::pair_from_ticket(dir, ticket, password).await?;
        node.journal().ensure_device_name("desktop (Linux)")?;
        node.journal().ensure_peer_join()?;
        node.journal().ensure_version_seen()?;
        node.journal()
            .store
            .meta_set("last_peer_ticket", ticket.trim().as_bytes())?;
        Ok(Self {
            node: Arc::new(node),
            _lock: lock,
            sync_tasks: Default::default(),
        })
    }
    pub fn capture_text(&self, text: &str) -> Result<Value> {
        if text.trim().is_empty() {
            bail!("Capture is empty");
        }
        let event = self.node.journal().capture_text(text)?;
        self.sync_after_capture();
        Ok(entry_json(&event))
    }
    pub fn entries(&self, query: &str, trash: bool, limit: usize) -> Result<Vec<Value>> {
        let journal = self.node.journal();
        if !query.trim().is_empty() && !trash {
            return memorious_core::mcp::search_entries(
                journal,
                None,
                query,
                &memorious_core::mcp::fts_quote(query),
                limit,
            );
        }
        if !trash {
            return memorious_core::mcp::recent_entries(journal, limit);
        }
        let annotations = journal.annotations()?;
        Ok(journal
            .trash()?
            .iter()
            .rev()
            .filter_map(|e| {
                let value = entry_json_annotated(e, &annotations);
                if query.trim().is_empty()
                    || value["text"]
                        .as_str()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&query.to_lowercase())
                    || value["annotation"]
                        .as_str()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&query.to_lowercase())
                {
                    Some(value)
                } else {
                    None
                }
            })
            .take(limit)
            .collect())
    }
    pub fn redact(&self, id: &str) -> Result<()> {
        self.node.journal().redact(id)?;
        Ok(())
    }
    pub fn ticket(&self) -> Result<String> {
        self.node.ticket()
    }
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let value = memorious_core::local::request(&self.node, method, params).await?;
        if matches!(
            method,
            "capture_text" | "capture_media" | "redact" | "annotate_voice"
        ) {
            self.sync_after_capture();
        }
        Ok(value)
    }
    pub async fn status(&self) -> Result<Value> {
        self.node.status_json().await
    }
    pub async fn media(&self, hash: &str) -> Result<Vec<u8>> {
        self.node.blob_bytes(hash).await
    }
    pub async fn capture_media(&self, kind: &str, bytes: Vec<u8>) -> Result<Value> {
        let event = match kind {
            "photo" => {
                self.node
                    .capture_blob(
                        MediaKind::Photo,
                        memorious_core::media::normalize_photo(&bytes)?,
                    )
                    .await?
            }
            "voice" | "music" => {
                if !memorious_core::media::is_mp4_family(&bytes) {
                    bail!("Choose an AAC recording in an m4a container");
                }
                self.node
                    .capture_audio(
                        bytes,
                        if kind == "music" {
                            AudioKind::Music
                        } else {
                            AudioKind::Voice
                        },
                    )
                    .await?
            }
            _ => bail!("Unsupported capture kind"),
        };
        self.sync_after_capture();
        Ok(entry_json(&event))
    }
    pub async fn sync(&self, ticket: &str) -> Result<Value> {
        self.request("sync", json!({"ticket": ticket})).await
    }
    fn sync_after_capture(&self) {
        let node = self.node.clone();
        let task = tokio::spawn(async move {
            let _ = node.sync_known_peers().await;
        });
        let mut tasks = self.sync_tasks.lock().unwrap();
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }
    pub async fn close(mut self) {
        for task in std::mem::take(self.sync_tasks.get_mut().unwrap()) {
            task.abort();
            let _ = task.await;
        }
        self.node.shutdown_ref().await;
    }
}
