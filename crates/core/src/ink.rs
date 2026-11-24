//! The infinite canvas on the journal: one canvas per UTC calendar month, drawn by a stream of
//! ink ops (stroke, image, move, delete, undo, redo, …) that are `infra` events —
//! `infra_kind: "ink"`, `target: "YYYY-MM"`, `data: {"op": …}`. See `docs/CANVAS-SYNC.md`.
//!
//! Riding `EventKind::Infra` means no wire change: every peer on a build that has the lossless
//! `Unknown` fallback stores and forwards ink it can't interpret, ink is never an entry in the
//! feed, and the canvas is just a fold over these ops in log order (`recorded_at`, `event_id`).
//! The core only validates the envelope and stores/serves it; what an op *means* (and how a
//! month is folded and later snapshotted) is the canvas's business, not this module's.

use anyhow::{bail, Result};
use serde_json::Value;

use crate::event::Event;
use crate::journal::Journal;

/// One op is a stroke's points, an image's rect, a lasso polygon: kilobytes. A megabyte of it
/// is a bug, not a drawing.
const MAX_OP_BYTES: usize = 512 * 1024;

/// `YYYY-MM`, month 01–12.
pub fn is_month_key(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 7 || b[4] != b'-' || !b[..4].iter().all(u8::is_ascii_digit) || !b[5..].iter().all(u8::is_ascii_digit) {
        return false;
    }
    matches!(s[5..].parse::<u32>(), Ok(1..=12))
}

fn check_op(month: &str, data: &Value) -> Result<()> {
    if !is_month_key(month) {
        bail!("ink month must be YYYY-MM, got {month:?}");
    }
    if !data.get("op").is_some_and(Value::is_string) {
        bail!("an ink event is an object with a string \"op\"");
    }
    if serde_json::to_vec(data)?.len() > MAX_OP_BYTES {
        bail!("ink op too large (max {MAX_OP_BYTES} bytes)");
    }
    Ok(())
}

impl Journal {
    /// Append one ink op to a month's canvas. `data` must be an object with a string `op`.
    pub fn append_ink(&self, month: &str, data: Value) -> Result<Event> {
        check_op(month, &data)?;
        self.append_ink_payload(month, data)
    }

    /// Append several ops in order, all or nothing as far as validation goes: every op is checked
    /// before the first is written, so a bad op never leaves half a batch behind.
    pub fn append_ink_batch(&self, month: &str, ops: Vec<Value>) -> Result<Vec<Event>> {
        for op in &ops {
            check_op(month, op)?;
        }
        ops.into_iter().map(|op| self.append_ink_payload(month, op)).collect()
    }

    fn append_ink_payload(&self, month: &str, data: Value) -> Result<Event> {
        let Value::Object(mut fields) = data else { bail!("an ink event is an object") };
        let op = match fields.remove("op") {
            Some(Value::String(op)) => op,
            _ => bail!("an ink event is an object with a string \"op\""),
        };
        self.store.append_local(self.device_id(), crate::EventKind::Ink, crate::Payload::Ink { month: month.into(), op, fields }, false)
    }

    /// Rewrite the legacy `infra`/`ink` events as first-class `ink` events, in place (ids, authors, sequence numbers and
    /// timestamps untouched). Idempotent. Readers handle both forms, so this is only about leaving the legacy form behind.
    pub fn migrate_ink(&self) -> Result<usize> {
        self.store.migrate_ink()
    }

    /// Ink ops for one month that arrived on this peer after `after` (0 = all), each with its arrival cursor.
    /// Arrival order, not canvas order: sort by (`recorded_at`, `event_id`) to fold. See `Store::ink_events`.
    pub fn ink_events(&self, month: &str, after: i64) -> Result<Vec<(i64, Event)>> {
        self.store.ink_events(month, after)
    }

    /// Be told, as it happens, which month got new ink — from this device or arriving by sync. The listener then asks
    /// `ink_events(month, cursor)` for the detail. (The server's `/api/ink/stream` is this, as server-sent events.)
    pub fn subscribe_ink(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.store.subscribe_ink()
    }

    /// Months that have any ink, oldest first.
    pub fn ink_months(&self) -> Result<Vec<String>> {
        self.store.ink_months()
    }

    /// `{events: [{event_id, recorded_at}]}` — the JSON every face (HTTP, mobile) returns for an append.
    pub fn ink_append_json(&self, month: &str, ops: Vec<Value>) -> Result<Value> {
        let evs = self.append_ink_batch(month, ops)?;
        Ok(serde_json::json!({
            "events": evs.iter().map(|e| serde_json::json!({"event_id": e.event_id, "recorded_at": e.recorded_at})).collect::<Vec<_>>()
        }))
    }

    /// `{device_id, cursor, events: [{event_id, device_id, recorded_at, data}]}` — arrival order after `after`.
    /// `device_id` is this peer's own (undo/redo are per author); `cursor` is what to pass as `after` next time.
    pub fn ink_list_json(&self, month: &str, after: i64) -> Result<Value> {
        if !is_month_key(month) {
            bail!("month must be YYYY-MM");
        }
        let evs = self.ink_events(month, after)?;
        Ok(serde_json::json!({
            "device_id": self.device_id(),
            "cursor": evs.last().map(|(r, _)| *r).unwrap_or(after),
            "events": evs.iter().map(|(_, e)| serde_json::json!({
                "event_id": e.event_id,
                "device_id": e.device_id,
                "recorded_at": e.recorded_at,
                "data": ink_data(e),
            })).collect::<Vec<_>>()
        }))
    }

    /// `{months: ["2026-09", ...]}`.
    pub fn ink_months_json(&self) -> Result<Value> {
        Ok(serde_json::json!({ "months": self.ink_months()? }))
    }
}

/// An ink event's op as the `{op, cid, ...}` object every face serves: new `ink` events carry it flat in the payload; legacy
/// `infra`/`ink` events carry it under `data`.
pub fn ink_data(e: &Event) -> Value {
    match &e.payload {
        crate::Payload::Ink { op, fields, .. } => {
            let mut m = fields.clone();
            m.insert("op".into(), op.clone().into());
            Value::Object(m)
        }
        crate::Payload::Infra { data, .. } => data.clone(),
        _ => Value::Null,
    }
}
