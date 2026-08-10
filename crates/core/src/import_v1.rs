//! One-off importer: v1's `GET /api/export` JSON replayed as capture events.
//! Chunks flatten to plain entries; conversations are discarded; photos are
//! re-fetched (caller supplies the fetcher) and re-encoded to JPEG; original
//! `recorded_at` preserved; re-runs are idempotent via per-chunk meta markers.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::event::MediaKind;
use crate::node::Node;

#[derive(Debug, Deserialize)]
pub struct V1Export {
    pub chunks: Vec<V1Chunk>,
}

#[derive(Debug, Deserialize)]
pub struct V1Chunk {
    pub id: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub recorded_at: Option<String>,
    #[serde(default)]
    pub photo_url: Option<String>,
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub text_entries: usize,
    pub photo_entries: usize,
    pub skipped: usize,
    pub photo_failures: usize,
}

fn marker(chunk_id: &str) -> String {
    format!("v1import:{chunk_id}")
}

/// Parse "YYYY-MM-DDTHH:MM:SS(.mmm)Z" to unix ms. No timezone math beyond Z.
pub fn iso_to_ms(iso: &str) -> Option<i64> {
    let s = iso.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut dp = date.split('-');
    let (y, m, d): (i64, i64, i64) = (
        dp.next()?.parse().ok()?,
        dp.next()?.parse().ok()?,
        dp.next()?.parse().ok()?,
    );
    let mut tp = time.split(':');
    let (hh, mm): (i64, i64) = (tp.next()?.parse().ok()?, tp.next()?.parse().ok()?);
    let rest = tp.next().unwrap_or("0");
    let (ss, ms): (i64, i64) = match rest.split_once('.') {
        Some((s_part, frac)) => {
            let frac3: String = frac.chars().chain("000".chars()).take(3).collect();
            (s_part.parse().ok()?, frac3.parse().ok()?)
        }
        None => (rest.parse().ok()?, 0),
    };
    // days-from-civil (Howard Hinnant)
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + hh) * 60 + mm) * 60_000 + ss * 1000 + ms)
}

/// `fetch_photo` receives the chunk's `photo_url` verbatim and returns image bytes.
pub async fn import_v1<F>(node: &Node, export: &V1Export, mut fetch_photo: F) -> Result<ImportReport>
where
    F: FnMut(&str) -> Result<Vec<u8>>,
{
    let journal = node.journal();
    let mut chunks: Vec<&V1Chunk> = export.chunks.iter().collect();
    chunks.sort_by(|a, b| {
        (a.recorded_at.as_deref(), &a.id).cmp(&(b.recorded_at.as_deref(), &b.id))
    });

    let mut report = ImportReport::default();
    for chunk in chunks {
        if journal.store.meta_get(&marker(&chunk.id))?.is_some() {
            report.skipped += 1;
            continue;
        }
        let recorded_at = chunk
            .recorded_at
            .as_deref()
            .and_then(iso_to_ms)
            .with_context(|| format!("chunk {} has no usable recorded_at", chunk.id))?;

        let mut event_ids = Vec::new();
        if let Some(text) = chunk.content.as_deref() {
            if !text.trim().is_empty() {
                let e = journal.store.append_local_at(
                    journal.device_id(),
                    crate::event::EventKind::Capture,
                    crate::event::Payload::Text { text: text.into() },
                    false,
                    recorded_at,
                )?;
                event_ids.push(e.event_id);
                report.text_entries += 1;
            }
        }
        if let Some(url) = chunk.photo_url.as_deref() {
            match fetch_photo(url).and_then(|bytes| crate::media::normalize_photo(&bytes)) {
                Ok(jpeg) => {
                    let e = node
                        .capture_blob_at(MediaKind::Photo, jpeg, recorded_at)
                        .await?;
                    event_ids.push(e.event_id);
                    report.photo_entries += 1;
                }
                Err(err) => {
                    // Leave the chunk unmarked so a re-run retries the photo —
                    // unless we already wrote its text half, which must not double.
                    tracing::warn!("photo fetch failed for chunk {}: {err:#}", chunk.id);
                    report.photo_failures += 1;
                    if event_ids.is_empty() {
                        continue;
                    }
                }
            }
        }
        journal
            .store
            .meta_set(&marker(&chunk.id), event_ids.join(",").as_bytes())?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Journal;
    use tempfile::tempdir;

    #[test]
    fn iso_parses_v1_timestamps() {
        assert_eq!(iso_to_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(iso_to_ms("1970-01-02T00:00:00Z"), Some(86_400_000));
        // Round-trip against a known value: 2026-03-14T18:37:44.280Z
        let ms = iso_to_ms("2026-03-14T18:37:44.280Z").unwrap();
        assert_eq!(ms, 1_773_513_464_280);
    }

    fn png_bytes() -> Vec<u8> {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            3,
            3,
            image::Rgb([9, 9, 9]),
        ));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn fixture() -> V1Export {
        serde_json::from_value(serde_json::json!({
            "chunks": [
                {"id": "c1", "content": "first note", "conversation_id": "conv1",
                 "recorded_at": "2025-06-01T08:00:00.000Z"},
                {"id": "c2", "content": "with a photo", "conversation_id": "",
                 "recorded_at": "2025-06-01T09:30:00.000Z",
                 "photo": "p.jpg", "photo_url": "/api/files/chunks/c2/p.jpg"},
                {"id": "c3", "content": "   ", "recorded_at": "2025-06-02T10:00:00.000Z"}
            ]
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn import_preserves_timestamps_flattens_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let node = Node::spawn(Journal::init(&dir.path().join("j")).unwrap())
            .await
            .unwrap();
        let export = fixture();

        let report = import_v1(&node, &export, |_| Ok(png_bytes())).await.unwrap();
        assert_eq!(report.text_entries, 2);
        assert_eq!(report.photo_entries, 1);
        assert_eq!(report.skipped, 0);

        let entries = node.journal().list().unwrap();
        assert_eq!(entries.len(), 3); // c1 text, c2 text, c2 photo; c3 blank dropped
        assert_eq!(entries[0].recorded_at, iso_to_ms("2025-06-01T08:00:00.000Z").unwrap());
        // conversation ids are gone: flat entries only
        assert!(entries.iter().all(|e| e.kind == crate::EventKind::Capture));

        // Re-run: nothing changes.
        let report = import_v1(&node, &export, |_| panic!("must not refetch"))
            .await
            .unwrap();
        assert_eq!(report.skipped, 3);
        assert_eq!(node.journal().list().unwrap().len(), 3);

        node.shutdown().await;
    }

    #[tokio::test]
    async fn failed_photo_is_retried_next_run() {
        let dir = tempdir().unwrap();
        let node = Node::spawn(Journal::init(&dir.path().join("j")).unwrap())
            .await
            .unwrap();
        let export: V1Export = serde_json::from_value(serde_json::json!({
            "chunks": [{"id": "p1", "recorded_at": "2025-01-01T00:00:00Z",
                        "photo_url": "/api/files/x"}]
        }))
        .unwrap();

        let report = import_v1(&node, &export, |_| anyhow::bail!("network down"))
            .await
            .unwrap();
        assert_eq!(report.photo_failures, 1);
        assert_eq!(node.journal().list().unwrap().len(), 0);

        let report = import_v1(&node, &export, |_| Ok(png_bytes())).await.unwrap();
        assert_eq!(report.photo_entries, 1);
        assert_eq!(node.journal().list().unwrap().len(), 1);

        node.shutdown().await;
    }
}
