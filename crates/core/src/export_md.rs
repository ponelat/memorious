//! Derived markdown export: a one-way `YYYY/MM/DD.md` tree plus media files,
//! regenerable from the log on any peer. Canonical data stays the event log —
//! this is a mirror, never an input. Deterministic: same log → same bytes.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::event::{Event, Payload};
use crate::node::Node;

#[derive(Debug, Default, PartialEq)]
pub struct ExportReport {
    pub day_files_written: usize,
    pub day_files_unchanged: usize,
    pub media_written: usize,
    pub media_unchanged: usize,
}

/// (year, month, day) in UTC from unix ms (civil-from-days, Howard Hinnant).
fn civil(ms: i64) -> (i64, i64, i64) {
    let days = ms.div_euclid(86_400_000);
    let z = days + 719_468;
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

fn hhmm(ms: i64) -> String {
    let tod = ms.rem_euclid(86_400_000) / 60_000;
    format!("{:02}:{:02}", tod / 60, tod % 60)
}

fn entry_line(e: &Event, annotation: Option<&str>) -> String {
    let time = hhmm(e.recorded_at);
    let mut line = match &e.payload {
        Payload::Text { text } => {
            format!("- {time} {}", text.trim().replace('\n', "\n  "))
        }
        Payload::Photo { hash, .. } => {
            format!("- {time} ![photo](../../media/{hash}.jpg)")
        }
        Payload::Audio { hash, .. } => {
            format!("- {time} [audio](../../media/{hash}.m4a)")
        }
        Payload::Video { hash, .. } => {
            format!("- {time} [video](../../media/{hash}.mp4)")
        }
        other => format!("- {time} {other:?}"),
    };
    if let Some(text) = annotation {
        if !text.is_empty() {
            line.push_str(&format!("\n  > {}", text.trim().replace('\n', "\n  > ")));
        }
    }
    line
}

fn write_if_changed(path: &Path, content: &[u8]) -> Result<bool> {
    if let Ok(existing) = std::fs::read(path) {
        if existing == content {
            return Ok(false);
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

pub async fn export_markdown(node: &Node, out: &Path) -> Result<ExportReport> {
    let journal = node.journal();
    let entries = journal.list()?;
    let annotations = journal.annotations()?;
    let mut report = ExportReport::default();

    // Group by UTC day, oldest first (list() is already oldest-first).
    let mut days: BTreeMap<(i64, i64, i64), Vec<&Event>> = BTreeMap::new();
    for e in &entries {
        days.entry(civil(e.recorded_at)).or_default().push(e);
    }

    for ((y, m, d), day_entries) in &days {
        let mut content = format!("# {y:04}-{m:02}-{d:02}\n\n");
        for e in day_entries {
            content.push_str(&entry_line(e, annotations.get(&e.event_id).map(String::as_str)));
            content.push('\n');
        }
        let path = out.join(format!("{y:04}/{m:02}/{d:02}.md"));
        if write_if_changed(&path, content.as_bytes())? {
            report.day_files_written += 1;
        } else {
            report.day_files_unchanged += 1;
        }
    }

    // Media files, named by hash so identity is stable across runs.
    for e in &entries {
        let (hash, ext) = match &e.payload {
            Payload::Photo { hash, .. } => (hash, "jpg"),
            Payload::Audio { hash, .. } => (hash, "m4a"),
            Payload::Video { hash, .. } => (hash, "mp4"),
            _ => continue,
        };
        let path = out.join(format!("media/{hash}.{ext}"));
        if path.exists() {
            report.media_unchanged += 1;
            continue;
        }
        match node.blob_bytes(hash).await {
            Ok(bytes) => {
                write_if_changed(&path, &bytes)?;
                report.media_written += 1;
            }
            Err(err) => tracing::warn!("blob {hash} not exportable: {err:#}"),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MediaKind;
    use crate::Journal;
    use tempfile::tempdir;

    #[tokio::test]
    async fn export_builds_day_tree_and_reruns_change_nothing() {
        let dir = tempdir().unwrap();
        let node = Node::spawn(Journal::init(&dir.path().join("j"), "pw").unwrap())
            .await
            .unwrap();
        let journal = node.journal();

        // Two days of entries with fixed timestamps.
        let day1 = crate::import_v1::iso_to_ms("2025-06-01T08:15:00Z").unwrap();
        let day2 = crate::import_v1::iso_to_ms("2025-06-02T20:05:00Z").unwrap();
        journal
            .store
            .append_local_at(
                journal.device_id(),
                crate::EventKind::Capture,
                Payload::Text { text: "morning\nsecond line".into() },
                false,
                day1,
            )
            .unwrap();
        let photo = node
            .capture_blob_at(MediaKind::Photo, vec![1, 2, 3, 4], day2)
            .await
            .unwrap();
        journal.annotate(&photo.event_id, "a receipt").unwrap();
        // Redacted entries stay out of the export.
        let gone = journal.capture_text("secret").unwrap();
        journal.redact(&gone.event_id).unwrap();

        let out = dir.path().join("export");
        let report = export_markdown(&node, &out).await.unwrap();
        assert_eq!(report.day_files_written, 2);
        assert_eq!(report.media_written, 1);

        let d1 = std::fs::read_to_string(out.join("2025/06/01.md")).unwrap();
        assert!(d1.contains("# 2025-06-01"));
        assert!(d1.contains("- 08:15 morning\n  second line"));
        assert!(!d1.contains("secret"));
        let d2 = std::fs::read_to_string(out.join("2025/06/02.md")).unwrap();
        let hash = photo.blob_hash().unwrap();
        assert!(d2.contains(&format!("- 20:05 ![photo](../../media/{hash}.jpg)")));
        assert!(d2.contains("> a receipt"));
        assert!(out.join(format!("media/{hash}.jpg")).exists());

        // Re-run: nothing rewritten.
        let report = export_markdown(&node, &out).await.unwrap();
        assert_eq!(report.day_files_written, 0);
        assert_eq!(report.day_files_unchanged, 2);
        assert_eq!(report.media_written, 0);
        assert_eq!(report.media_unchanged, 1);

        node.shutdown().await;
    }
}
