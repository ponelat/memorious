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
        self.append_infra("ink", Some(month), data)
    }

    /// Append several ops in order, all or nothing as far as validation goes: every op is checked
    /// before the first is written, so a bad op never leaves half a batch behind.
    pub fn append_ink_batch(&self, month: &str, ops: Vec<Value>) -> Result<Vec<Event>> {
        for op in &ops {
            check_op(month, op)?;
        }
        ops.into_iter().map(|op| self.append_infra("ink", Some(month), op)).collect()
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
}
