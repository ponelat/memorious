//! A completed blob fetch must not pin the ciphertext in anonymous memory.
//!
//! The always-on peer runs under a 192MB cgroup. A music-sized blob that
//! stayed on the heap for the whole transfer would be enough to matter.
//! `iroh-blobs` flushes bao leaves as parent nodes arrive, so a successful
//! transfer should stay well under half the payload. This guards that.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use memorious_core::event::MediaKind;
use memorious_core::node::Node;
use memorious_core::Journal;
use tempfile::tempdir;

fn rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .expect("VmRSS value")
                .parse()
                .expect("VmRSS kb");
            return kb * 1024;
        }
    }
    panic!("VmRSS missing from /proc/self/status");
}

/// Highest RSS observed while `flag` stays set. Samples from a dedicated
/// thread so a fast loopback transfer still gets measured.
fn watch_rss(flag: Arc<AtomicBool>) -> (Arc<AtomicU64>, std::thread::JoinHandle<()>) {
    let peak = Arc::new(AtomicU64::new(rss_bytes()));
    let peak_bg = peak.clone();
    let handle = std::thread::spawn(move || {
        while flag.load(Ordering::Relaxed) {
            let rss = rss_bytes();
            peak_bg.fetch_max(rss, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(1));
        }
        let rss = rss_bytes();
        peak_bg.fetch_max(rss, Ordering::Relaxed);
    });
    (peak, handle)
}

#[tokio::test]
async fn large_blob_fetch_does_not_hold_the_payload_in_ram() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();

    // Not a JPEG. Capture stores the bytes as-is; 16MiB is large enough that
    // holding a second copy is unmistakable, and small enough to transfer on
    // loopback inside the sampler's window.
    const SIZE: usize = 16 * 1024 * 1024;
    let bytes = vec![0x5Au8; SIZE];
    let event = a
        .capture_blob(MediaKind::Photo, bytes.clone())
        .await
        .unwrap();
    let hash = event.blob_hash().unwrap().to_string();

    // Land the event without the bytes, so the measured sync is the fetch.
    b.sync_events_with(&a.addr()).await.unwrap();
    assert!(!b.has_blob(&hash).await.unwrap());

    let flag = Arc::new(AtomicBool::new(true));
    let (peak, watcher) = watch_rss(flag.clone());
    let baseline = rss_bytes();
    peak.fetch_max(baseline, Ordering::Relaxed);

    b.sync_with(&a.addr()).await.unwrap();

    flag.store(false, Ordering::Relaxed);
    watcher.join().unwrap();

    assert!(b.has_blob(&hash).await.unwrap(), "fetch stored the blob");
    assert_eq!(b.blob_bytes(&hash).await.unwrap(), bytes);

    let growth = peak.load(Ordering::Relaxed).saturating_sub(baseline);
    // A streaming fetch holds a chunk group plus a 1MiB copy buffer. Half the
    // payload (8MiB) is far above that and far below "the whole blob".
    assert!(
        growth < (SIZE / 2) as u64,
        "blob fetch grew RSS by {growth} bytes (blob is {SIZE}); the transfer is buffering the payload"
    );

    a.shutdown().await;
    b.shutdown().await;
}
