//! SQLite event log: canonical, append-only. FTS5 index is derived and rebuildable.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::event::{Event, EventKind, Payload};

/// Per-device heads: highest contiguous seq we hold for each device. The version vector.
pub type Heads = BTreeMap<String, u64>;

pub struct Store {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
  key TEXT PRIMARY KEY,
  value BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
  event_id    TEXT PRIMARY KEY,
  device_id   TEXT NOT NULL,
  seq         INTEGER NOT NULL,
  recorded_at INTEGER NOT NULL,
  kind        TEXT NOT NULL,
  payload     TEXT NOT NULL,
  will_enrich INTEGER NOT NULL DEFAULT 0,
  -- When THIS peer first saw the event. Local only, never syncs; the
  -- enrichment grace period counts from here, not from recorded_at.
  local_received_at INTEGER NOT NULL DEFAULT 0,
  UNIQUE(device_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_events_recorded ON events(recorded_at, event_id);
CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(text, event_id UNINDEXED);
";

impl Store {
    /// Open (or create) an encrypted store. `db_key_hex` is the raw SQLCipher
    /// key (already password-stretched — see crypto::KeySet::db_key_hex).
    pub fn open(path: &Path, db_key_hex: &str) -> Result<Self> {
        Self::open_with(path, Some(db_key_hex))
    }

    /// Open a legacy plaintext database — only the migration path may do this.
    pub(crate) fn open_plaintext(path: &Path) -> Result<Self> {
        Self::open_with(path, None)
    }

    fn open_with(path: &Path, db_key_hex: Option<&str>) -> Result<Self> {
        let conn = Connection::open(path).context("open sqlite")?;
        if let Some(hex) = db_key_hex {
            // Raw-key form: skips SQLCipher's own KDF (we already did Argon2id).
            conn.pragma_update(None, "key", format!("x'{hex}'"))
                .context("apply database key")?;
        }
        // Any read forces SQLCipher to actually check the key.
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))
            .context("unlock database — wrong master password?")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA).context("create schema")?;
        // Boring migration for pre-M5 databases; errors mean the column exists.
        let _ = conn.execute(
            "ALTER TABLE events ADD COLUMN local_received_at INTEGER NOT NULL DEFAULT 0",
            [],
        );
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // ---- meta ----

    pub fn meta_get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let v = conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(v)
    }

    pub fn meta_set(&self, key: &str, value: &[u8]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Every meta row — the migration tool copies identity wholesale.
    pub(crate) fn meta_all(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT key, value FROM meta")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // ---- event log ----

    /// Append an event authored by the local device. Assigns the next seq for `device_id`.
    pub fn append_local(
        &self,
        device_id: &str,
        kind: EventKind,
        payload: Payload,
        will_enrich: bool,
    ) -> Result<Event> {
        self.append_local_at(device_id, kind, payload, will_enrich, now_ms())
    }

    /// append_local with an explicit `recorded_at` — for the import tool, which
    /// must preserve original capture times.
    pub fn append_local_at(
        &self,
        device_id: &str,
        kind: EventKind,
        payload: Payload,
        will_enrich: bool,
        recorded_at: i64,
    ) -> Result<Event> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let head: u64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM events WHERE device_id = ?1",
                [device_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let event = Event {
            event_id: uuid::Uuid::now_v7().to_string(),
            device_id: device_id.to_string(),
            seq: head + 1,
            recorded_at,
            kind,
            payload,
            will_enrich,
        };
        insert_event(&tx, &event)?;
        tx.commit()?;
        Ok(event)
    }

    /// Insert an event received from a peer. Idempotent: an event we already hold is a no-op
    /// (returns false). A seq gap is a protocol violation and errors out.
    pub fn insert_remote(&self, event: &Event) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let head: u64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM events WHERE device_id = ?1",
                [&event.device_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if event.seq <= head {
            return Ok(false); // duplicate
        }
        if event.seq != head + 1 {
            bail!(
                "seq gap for device {}: have head {}, got {}",
                event.device_id,
                head,
                event.seq
            );
        }
        insert_event(&tx, event)?;
        tx.commit()?;
        Ok(true)
    }

    /// Highest seq per device.
    pub fn heads(&self) -> Result<Heads> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT device_id, MAX(seq) FROM events GROUP BY device_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))?;
        let mut heads = Heads::new();
        for row in rows {
            let (d, s) = row?;
            heads.insert(d, s);
        }
        Ok(heads)
    }

    /// Events the peer (with the given heads) is missing, ordered per device by seq.
    pub fn events_missing_from(&self, peer_heads: &Heads) -> Result<Vec<Event>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT event_id, device_id, seq, recorded_at, kind, payload, will_enrich
             FROM events ORDER BY device_id, seq",
        )?;
        let rows = stmt.query_map([], row_to_event)?;
        let mut out = Vec::new();
        for row in rows {
            let ev = row?;
            let peer_head = peer_heads.get(&ev.device_id).copied().unwrap_or(0);
            if ev.seq > peer_head {
                out.push(ev);
            }
        }
        Ok(out)
    }

    /// All events, ordered by (recorded_at, event_id) — a deterministic timeline.
    pub fn all_events(&self) -> Result<Vec<Event>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT event_id, device_id, seq, recorded_at, kind, payload, will_enrich
             FROM events ORDER BY recorded_at, event_id",
        )?;
        let rows = stmt.query_map([], row_to_event)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn get_event(&self, event_id: &str) -> Result<Option<Event>> {
        let conn = self.conn.lock().unwrap();
        let ev = conn
            .query_row(
                "SELECT event_id, device_id, seq, recorded_at, kind, payload, will_enrich
                 FROM events WHERE event_id = ?1",
                [event_id],
                row_to_event,
            )
            .optional()?;
        Ok(ev)
    }

    /// Event ids struck out by redact events.
    pub fn redacted_ids(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT payload FROM events WHERE kind = 'redact'")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            let payload: Payload = serde_json::from_str(&row?)?;
            if let Payload::Redact { target } = payload {
                out.insert(target);
            }
        }
        Ok(out)
    }

    /// Blob hashes referenced by any event.
    pub fn referenced_blob_hashes(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT payload FROM events WHERE kind = 'capture' ORDER BY recorded_at",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let payload: Payload = serde_json::from_str(&row?)?;
            if let Payload::Photo { hash, .. }
            | Payload::Audio { hash, .. }
            | Payload::Video { hash, .. } = payload
            {
                out.push(hash);
            }
        }
        Ok(out)
    }

    /// The capture payload referencing a blob hash — where its wrapped
    /// content key lives.
    pub fn capture_payload_for_hash(&self, hash: &str) -> Result<Option<Payload>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT payload FROM events WHERE kind = 'capture'")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            let payload: Payload = serde_json::from_str(&row?)?;
            if let Payload::Photo { hash: h, .. }
            | Payload::Audio { hash: h, .. }
            | Payload::Video { hash: h, .. } = &payload
            {
                if h == hash {
                    return Ok(Some(payload));
                }
            }
        }
        Ok(None)
    }

    /// When this peer first saw the event (local clock; never syncs).
    pub fn local_received_at(&self, event_id: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let v = conn
            .query_row(
                "SELECT local_received_at FROM events WHERE event_id = ?1",
                [event_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v)
    }

    /// FTS search over entry text + annotations. Returns matching event ids.
    pub fn search(&self, query: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT event_id FROM events_fts WHERE events_fts MATCH ?1 ORDER BY rank",
        )?;
        let rows = stmt.query_map([query], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

fn insert_event(tx: &rusqlite::Transaction, event: &Event) -> Result<()> {
    let kind = serde_json::to_string(&event.kind)?;
    let kind = kind.trim_matches('"');
    let payload = serde_json::to_string(&event.payload)?;
    tx.execute(
        "INSERT INTO events (event_id, device_id, seq, recorded_at, kind, payload, will_enrich, local_received_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            event.event_id,
            event.device_id,
            event.seq,
            event.recorded_at,
            kind,
            payload,
            event.will_enrich,
            now_ms(),
        ],
    )?;
    if let Some(text) = event.fts_text() {
        tx.execute(
            "INSERT INTO events_fts (text, event_id) VALUES (?1, ?2)",
            params![text, event.event_id],
        )?;
    }
    Ok(())
}

fn row_to_event(r: &rusqlite::Row) -> rusqlite::Result<Event> {
    let kind_str: String = r.get(4)?;
    let payload_str: String = r.get(5)?;
    let kind: EventKind = serde_json::from_str(&format!("\"{kind_str}\""))
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let payload: Payload = serde_json::from_str(&payload_str)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    Ok(Event {
        event_id: r.get(0)?,
        device_id: r.get(1)?,
        seq: r.get(2)?,
        recorded_at: r.get(3)?,
        kind,
        payload,
        will_enrich: r.get(6)?,
    })
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const TEST_KEY: &str = "0707070707070707070707070707070707070707070707070707070707070707";

    fn open_temp() -> (tempfile::TempDir, Store) {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite"), TEST_KEY).unwrap();
        (dir, store)
    }

    fn text(t: &str) -> Payload {
        Payload::Text { text: t.into() }
    }

    #[test]
    fn append_assigns_contiguous_seqs() {
        let (_d, store) = open_temp();
        let e1 = store
            .append_local("dev-a", EventKind::Capture, text("one"), false)
            .unwrap();
        let e2 = store
            .append_local("dev-a", EventKind::Capture, text("two"), false)
            .unwrap();
        assert_eq!(e1.seq, 1);
        assert_eq!(e2.seq, 2);
        assert_ne!(e1.event_id, e2.event_id);
    }

    #[test]
    fn heads_reflect_appends_per_device() {
        let (_d, store) = open_temp();
        store
            .append_local("dev-a", EventKind::Capture, text("a1"), false)
            .unwrap();
        store
            .append_local("dev-a", EventKind::Capture, text("a2"), false)
            .unwrap();
        let remote = Event {
            event_id: "evt-b1".into(),
            device_id: "dev-b".into(),
            seq: 1,
            recorded_at: 42,
            kind: EventKind::Capture,
            payload: text("b1"),
            will_enrich: false,
        };
        assert!(store.insert_remote(&remote).unwrap());
        let heads = store.heads().unwrap();
        assert_eq!(heads.get("dev-a"), Some(&2));
        assert_eq!(heads.get("dev-b"), Some(&1));
    }

    #[test]
    fn insert_remote_is_idempotent_and_rejects_gaps() {
        let (_d, store) = open_temp();
        let ev = |seq: u64| Event {
            event_id: format!("evt-{seq}"),
            device_id: "dev-b".into(),
            seq,
            recorded_at: 42,
            kind: EventKind::Capture,
            payload: text("x"),
            will_enrich: false,
        };
        assert!(store.insert_remote(&ev(1)).unwrap());
        assert!(!store.insert_remote(&ev(1)).unwrap()); // duplicate: no-op
        assert!(store.insert_remote(&ev(3)).is_err()); // gap: refused
        assert_eq!(store.heads().unwrap().get("dev-b"), Some(&1));
    }

    #[test]
    fn missing_from_peer_heads() {
        let (_d, store) = open_temp();
        for t in ["a1", "a2", "a3"] {
            store
                .append_local("dev-a", EventKind::Capture, text(t), false)
                .unwrap();
        }
        let mut peer = Heads::new();
        peer.insert("dev-a".into(), 1);
        let missing = store.events_missing_from(&peer).unwrap();
        assert_eq!(missing.len(), 2);
        assert_eq!(missing[0].seq, 2);
        assert_eq!(missing[1].seq, 3);
        // peer with nothing gets everything
        let all = store.events_missing_from(&Heads::new()).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn redaction_is_an_event_not_a_delete() {
        let (_d, store) = open_temp();
        let e = store
            .append_local("dev-a", EventKind::Capture, text("secret"), false)
            .unwrap();
        store
            .append_local(
                "dev-a",
                EventKind::Redact,
                Payload::Redact {
                    target: e.event_id.clone(),
                },
                false,
            )
            .unwrap();
        // both events still in the log
        assert_eq!(store.all_events().unwrap().len(), 2);
        assert!(store.redacted_ids().unwrap().contains(&e.event_id));
    }

    #[test]
    fn fts_search_finds_text_and_annotations() {
        let (_d, store) = open_temp();
        let e = store
            .append_local("dev-a", EventKind::Capture, text("bought a kayak today"), false)
            .unwrap();
        store
            .append_local("dev-a", EventKind::Capture, text("nothing else"), false)
            .unwrap();
        let ann = store
            .append_local(
                "dev-a",
                EventKind::Annotation,
                Payload::Annotation {
                    target: e.event_id.clone(),
                    text: "paddling transcript".into(),
                },
                false,
            )
            .unwrap();
        let hits = store.search("kayak").unwrap();
        assert_eq!(hits, vec![e.event_id.clone()]);
        let hits = store.search("paddling").unwrap();
        assert_eq!(hits, vec![ann.event_id]);
    }

    #[test]
    fn events_round_trip_through_storage() {
        let (_d, store) = open_temp();
        let photo = store
            .append_local(
                "dev-a",
                EventKind::Capture,
                Payload::Photo {
                    hash: "deadbeef".into(),
                    size: 123,
                    crypto: None,
                },
                true,
            )
            .unwrap();
        let got = store.get_event(&photo.event_id).unwrap().unwrap();
        assert_eq!(got, photo);
        assert_eq!(store.referenced_blob_hashes().unwrap(), vec!["deadbeef"]);
        let looked_up = store.capture_payload_for_hash("deadbeef").unwrap().unwrap();
        assert_eq!(looked_up, photo.payload);
        assert!(store.capture_payload_for_hash("cafe").unwrap().is_none());
    }

    #[test]
    fn database_file_is_ciphertext_and_needs_the_right_key() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        {
            let store = Store::open(&path, TEST_KEY).unwrap();
            store
                .append_local("dev-a", EventKind::Capture, text("very secret words"), false)
                .unwrap();
            // Fold WAL content back into the main file before inspecting it.
            store
                .conn
                .lock()
                .unwrap()
                .pragma_update(None, "wal_checkpoint", "TRUNCATE")
                .ok();
        }
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.starts_with(b"SQLite format 3"), "header must be encrypted");
        let needle = b"very secret words";
        assert!(
            !raw.windows(needle.len()).any(|w| w == needle),
            "plaintext must not appear in the database file"
        );

        let wrong = "0808080808080808080808080808080808080808080808080808080808080808";
        let err = Store::open(&path, wrong).err().expect("must fail");
        assert!(format!("{err:#}").contains("wrong master password"));
        // and the right key still opens it
        let store = Store::open(&path, TEST_KEY).unwrap();
        assert_eq!(store.all_events().unwrap().len(), 1);
    }
}
