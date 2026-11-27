//! The infinite canvas on the journal: one canvas per UTC calendar month, drawn by a stream of
//! ink ops (stroke, image, move, delete, undo, redo, …) that are `infra` events —
//! `infra_kind: "ink"`, `target: "YYYY-MM"`, `data: {"op": …}`. See `docs/CANVAS-SYNC.md`.
//!
//! Riding `EventKind::Infra` means no wire change: every peer on a build that has the lossless
//! `Unknown` fallback stores and forwards ink it can't interpret, ink is never an entry in the
//! feed, and the canvas is just a fold over these ops in log order (`recorded_at`, `event_id`).
//! The core only validates the envelope and stores/serves it; what an op *means* is the
//! canvas's business, not this module's.

use anyhow::{bail, Result};
use serde_json::Value;

use crate::event::Event;
use crate::journal::Journal;

/// One op is a stroke's points, an image's rect, a lasso polygon: kilobytes. A megabyte of it
/// is a bug, not a drawing.
const MAX_OP_BYTES: usize = 512 * 1024;

/// A canvas period key: a UTC ISO week `YYYY-Www` (W01-W53, new) or a legacy UTC month `YYYY-MM` (01-12).
pub fn is_period_key(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 7 || !b[..4].iter().all(u8::is_ascii_digit) || b[4] != b'-' {
        return false;
    }
    if b[5] == b'W' {
        return b.len() == 8 && b[6].is_ascii_digit() && b[7].is_ascii_digit() && matches!(s[6..].parse::<u32>(), Ok(1..=53));
    }
    b.len() == 7 && b[5].is_ascii_digit() && b[6].is_ascii_digit() && matches!(s[5..].parse::<u32>(), Ok(1..=12))
}

/// Legacy name for [`is_period_key`].
pub fn is_month_key(s: &str) -> bool {
    is_period_key(s)
}

// ---- UTC calendar maths (days from civil / civil from days, Howard Hinnant) ----

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const DAY_MS: i64 = 86_400_000;

/// Unix ms of a UTC date and time.
pub fn utc_ms(y: i64, m: i64, d: i64, h: i64, min: i64, s: i64) -> i64 {
    days_from_civil(y, m, d) * DAY_MS + (h * 3600 + min * 60 + s) * 1000
}

/// The canvas week an instant belongs to: a UTC ISO week, Monday 00:00 to Sunday 24:00, keyed `YYYY-Www` by the ISO year.
pub fn week_key(ms: i64) -> String {
    let days = ms.div_euclid(DAY_MS);
    let weekday = (days + 3).rem_euclid(7); // Monday = 0 (1970-01-01 was a Thursday)
    let thursday = days - weekday + 3; // the Thursday of this ISO week decides the ISO year
    let (iso_year, _, _) = civil_from_days(thursday);
    let week = (thursday - days_from_civil(iso_year, 1, 1)) / 7 + 1;
    format!("{iso_year:04}-W{week:02}")
}

/// Start (inclusive) of a period, unix ms.
pub fn period_start_ms(key: &str) -> Option<i64> {
    if !is_period_key(key) {
        return None;
    }
    let year: i64 = key[..4].parse().ok()?;
    if key.as_bytes()[5] == b'W' {
        let week: i64 = key[6..].parse().ok()?;
        // ISO week 1 is the one containing January 4th
        let jan4 = days_from_civil(year, 1, 4);
        let week1_monday = jan4 - (jan4 + 3).rem_euclid(7);
        Some((week1_monday + (week - 1) * 7) * DAY_MS)
    } else {
        Some(utc_ms(year, key[5..].parse().ok()?, 1, 0, 0, 0))
    }
}

/// End (exclusive) of a period, unix ms: the instant the next one starts. For a week that is Sunday midnight = Monday 00:00 UTC.
pub fn period_end_ms(key: &str) -> Option<i64> {
    let start = period_start_ms(key)?;
    if key.as_bytes()[5] == b'W' {
        Some(start + 7 * DAY_MS)
    } else {
        let (year, month): (i64, i64) = (key[..4].parse().ok()?, key[5..].parse().ok()?);
        Some(if month == 12 { utc_ms(year + 1, 1, 1, 0, 0, 0) } else { utc_ms(year, month + 1, 1, 0, 0, 0) })
    }
}

/// The period just before `key` (same kind: the previous week, or the previous month).
pub fn previous_period(key: &str) -> Option<String> {
    let start = period_start_ms(key)?;
    Some(if key.as_bytes()[5] == b'W' {
        week_key(start - 1)
    } else {
        let (y, m): (i64, i64) = (key[..4].parse().ok()?, key[5..].parse().ok()?);
        if m == 1 { format!("{:04}-12", y - 1) } else { format!("{y:04}-{:02}", m - 1) }
    })
}

fn check_op(period: &str, data: &Value) -> Result<()> {
    if !is_period_key(period) {
        bail!("ink period must be YYYY-Www or YYYY-MM, got {period:?}");
    }
    if data.get("op").and_then(Value::as_str).is_none() {
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
        self.store.append_local(self.device_id(), crate::EventKind::Ink, crate::Payload::Ink { period: month.into(), op, fields }, false)
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
        if !is_period_key(month) {
            bail!("period must be YYYY-Www or YYYY-MM");
        }
        let evs = self.ink_events(month, after)?;
        Ok(serde_json::json!({
            "device_id": self.device_id(),
            "cursor": evs.last().map(|(r, _)| *r).unwrap_or(after),
            "events": evs.iter().filter(|(_, e)| !matches!(&e.payload, crate::Payload::Unknown { type_tag, .. } if type_tag == "collected")).map(|(_, e)| serde_json::json!({
                "event_id": e.event_id,
                "device_id": e.device_id,
                "seq": e.seq,
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
