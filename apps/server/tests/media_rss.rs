//! Parallel `GET /api/media` must not pin every blob in anonymous memory.
//!
//! The browser feed fetches each mounted photo, audio, and video as soon as
//! the row mounts. `memorious-serve` runs in a 192MB cgroup (idle RSS ~11MB).
//! A handler that materialises ciphertext and plaintext for every request at
//! once is what the OOM killer saw: anon-rss ~194MB, then a restart, then the
//! same burst again.
//!
//! This drives N large blobs through the real router, discards each response
//! frame as it arrives, and samples `RssAnon` (the figure the OOM killer uses;
//! `VmRSS` also counts the page cache).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use memorious_core::node::{NetConfig, Node};
use memorious_core::{Journal, MediaKind};
use memorious_server::{app, AppState};
use tower::ServiceExt;

const BLOBS: usize = 6;
const BLOB_LEN: usize = 4 * 1024 * 1024;
/// One blob's plaintext, not the whole burst. Six full copies are ~24MB of
/// plaintext alone, and the decrypt holds the ciphertext beside that.
const MAX_GROWTH: u64 = 8 * 1024 * 1024;

fn rss_anon_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("RssAnon:") {
            let kb: u64 = rest.split_whitespace().next().expect("RssAnon").parse().unwrap();
            return kb * 1024;
        }
    }
    panic!("RssAnon missing from /proc/self/status");
}

fn trim_heap() {
    extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    unsafe { malloc_trim(0) };
}

fn watch_rss(flag: Arc<AtomicBool>) -> (Arc<AtomicU64>, std::thread::JoinHandle<()>) {
    let peak = Arc::new(AtomicU64::new(rss_anon_bytes()));
    let peak_bg = peak.clone();
    let handle = std::thread::spawn(move || {
        while flag.load(Ordering::Relaxed) {
            peak_bg.fetch_max(rss_anon_bytes(), Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(1));
        }
        peak_bg.fetch_max(rss_anon_bytes(), Ordering::Relaxed);
    });
    (peak, handle)
}

fn mix(mut acc: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        acc = acc.wrapping_mul(0x9E37_79B1_85EB_CA87).wrapping_add(b as u64);
    }
    acc
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_media_reads_do_not_hold_every_blob() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::init(&dir.path().join("j"), "pw").unwrap();
    journal.set_passcode("sesame").unwrap();
    journal
        .set_net_config(&NetConfig {
            relay_mode: "disabled".into(),
            relay_urls: vec![],
            public_lookup: false,
        })
        .unwrap();
    let node = Node::spawn_with_gc_interval(journal, Duration::from_secs(3600))
        .await
        .unwrap();

    let mut hashes = Vec::with_capacity(BLOBS);
    let mut sums = Vec::with_capacity(BLOBS);
    for i in 0..BLOBS {
        let bytes = vec![0xA0 + i as u8; BLOB_LEN];
        sums.push(mix(0, &bytes));
        let event = node.capture_blob(MediaKind::Photo, bytes).await.unwrap();
        hashes.push(event.blob_hash().unwrap().to_string());
    }

    // Capture allocated each plaintext and its ciphertext in turn. Give those
    // pages back so the reads are measured from a quiet baseline.
    trim_heap();
    let baseline = rss_anon_bytes();

    let state = Arc::new(AppState::new(node, None, dir.path().join("j")));
    let router = app(state.clone(), None);

    let flag = Arc::new(AtomicBool::new(true));
    let (peak, watcher) = watch_rss(flag.clone());
    peak.fetch_max(baseline, Ordering::Relaxed);

    let mut tasks = tokio::task::JoinSet::new();
    for (hash, sum) in hashes.into_iter().zip(sums) {
        let router = router.clone();
        tasks.spawn(async move {
            let resp = router
                .oneshot(
                    Request::get(format!("/api/media/{hash}"))
                        .header(header::AUTHORIZATION, "Bearer sesame")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{hash}");
            assert_eq!(
                resp.headers().get(header::CONTENT_TYPE).map(|v| v.as_bytes()),
                Some(&b"image/jpeg"[..])
            );
            let mut body = resp.into_body();
            let mut acc = 0u64;
            let mut len = 0usize;
            while let Some(frame) = body.frame().await {
                let frame = frame.expect("media body");
                let Ok(data) = frame.into_data() else { continue };
                acc = mix(acc, &data);
                len += data.len();
                drop(data);
            }
            assert_eq!(len, BLOB_LEN, "short body for {hash}");
            assert_eq!(acc, sum, "plaintext mismatch for {hash}");
        });
    }
    while let Some(joined) = tasks.join_next().await {
        joined.expect("media task");
    }

    flag.store(false, Ordering::Relaxed);
    watcher.join().unwrap();

    let growth = peak.load(Ordering::Relaxed).saturating_sub(baseline);
    eprintln!(
        "parallel media reads: baseline {baseline} peak {} growth {growth}",
        peak.load(Ordering::Relaxed)
    );
    assert!(
        growth < MAX_GROWTH,
        "parallel /api/media grew anonymous RSS by {growth} bytes \
         (limit {MAX_GROWTH}); each request is still holding its whole blob"
    );

    state.node.shutdown_ref().await;
}

