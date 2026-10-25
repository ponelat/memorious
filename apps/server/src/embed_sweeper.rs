//! Background pass that embeds new entries for semantic search. Runs only on
//! peers that opt in (`MEMORIOUS_EMBEDDINGS=on` in a build with the
//! `embeddings` feature); everywhere else search is keyword-only.

use std::sync::Arc;
use std::time::Duration;

use memorious_core::search::{embed_pending, embed_pending_words, Embedder};

/// Small batches, and a breather between them: the model sits behind one
/// mutex, and a search waiting for it must not starve behind a long backfill.
const BATCH: usize = 8;
/// Words are one or two tokens: far cheaper per item than entries.
const WORD_BATCH: usize = 64;
const BREATHER: Duration = Duration::from_millis(40);

/// Embed everything pending right now. Returns rows written.
pub fn sweep_once(store: &memorious_core::store::Store, embedder: &dyn Embedder) -> anyhow::Result<usize> {
    let mut total = 0;
    loop {
        let n = embed_pending(store, embedder, BATCH)?;
        if n == 0 {
            break;
        }
        total += n;
        std::thread::sleep(BREATHER);
    }
    // Then the vocabulary behind related-terms; not counted in the return.
    while embed_pending_words(store, embedder, WORD_BATCH)? > 0 {
        std::thread::sleep(BREATHER);
    }
    Ok(total)
}

/// Run forever, re-resolving the current `AppState` each tick (reset and
/// re-pair swap it). Embedding is blocking CPU work, so it goes off-runtime.
pub fn spawn(state: crate::RouterState, embedder: Arc<dyn Embedder>) {
    tokio::spawn(async move {
        loop {
            let snapshot = state.0.read().unwrap().clone();
            let e = embedder.clone();
            let res = tokio::task::spawn_blocking(move || sweep_once(&snapshot.node.journal().store, e.as_ref())).await;
            match res {
                Ok(Ok(n)) if n > 0 => tracing::info!("embedded {n} entries"),
                Ok(Err(err)) => tracing::warn!("embedding sweep failed: {err:#}"),
                Err(err) => tracing::warn!("embedding sweep panicked: {err}"),
                _ => {}
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}
