//! Hybrid search, QMD-style: BM25 (FTS5) and vector similarity, fused with
//! Reciprocal Rank Fusion. Vectors are derived, local, rebuildable and never
//! sync — a peer without an embedder simply has keyword search. The embedder
//! is a trait so tests (and peers that opt out) need no model.

use anyhow::Result;

use crate::store::Store;

/// Turns text into unit-length vectors. `model()` keys stored vectors, so
/// swapping models re-embeds everything instead of mixing vector spaces.
pub trait Embedder: Send + Sync {
    fn model(&self) -> &str;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    /// Embed a user's query. Interactive: an implementation may give up
    /// (Err) rather than queue behind a backfill — search then degrades to
    /// keyword results instead of hanging.
    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        self.embed(&[text.to_string()])?.pop().ok_or_else(|| anyhow::anyhow!("no vector"))
    }
}

/// Cosine floor for a vector hit to count at all; below this it is noise.
const MIN_SIMILARITY: f32 = 0.25;
/// Candidates taken from each side before fusion.
const DEPTH: usize = 50;
/// The standard RRF damping constant.
const RRF_K: f32 = 60.0;

/// Embed up to `batch` not-yet-embedded rows. Returns how many were written;
/// call until it returns 0.
pub fn embed_pending(store: &Store, embedder: &dyn Embedder, batch: usize) -> Result<usize> {
    let rows = store.rows_missing_embedding(embedder.model(), batch)?;
    if rows.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = rows.iter().map(|(_, t)| t.clone()).collect();
    let vecs = embedder.embed(&texts)?;
    anyhow::ensure!(vecs.len() == rows.len(), "embedder returned {} vectors for {} texts", vecs.len(), rows.len());
    let items: Vec<(String, Vec<f32>)> = rows.into_iter().map(|(id, _)| id).zip(vecs).collect();
    store.put_embeddings(embedder.model(), &items)?;
    Ok(items.len())
}

/// Cosine floor for a suggested related word.
const MIN_WORD_SIMILARITY: f32 = 0.5;
/// A word in more than this share of the notes says nothing about a query.
const MAX_WORD_DOC_FRACTION: f32 = 0.5;

/// Embed up to `batch` indexed words that have no vector yet. Returns how many
/// were written; call until it returns 0.
pub fn embed_pending_words(store: &Store, embedder: &dyn Embedder, batch: usize) -> Result<usize> {
    let words = store.words_missing_embedding(embedder.model(), batch)?;
    if words.is_empty() {
        return Ok(0);
    }
    let vecs = embedder.embed(&words)?;
    anyhow::ensure!(vecs.len() == words.len(), "embedder returned {} vectors for {} words", vecs.len(), words.len());
    let items: Vec<(String, Vec<f32>)> = words.into_iter().zip(vecs).collect();
    store.put_word_embeddings(embedder.model(), &items)?;
    Ok(items.len())
}

/// Words from the journal closest in meaning to `query`, best first — what a
/// search is "also about". Never the query's own words or their inflections.
pub fn related_terms(store: &Store, embedder: &dyn Embedder, query: &str, k: usize) -> Result<Vec<(String, f32)>> {
    let own: Vec<String> = query.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
    let v = embedder.embed_query(query)?;
    let near = store.nearest_words(embedder.model(), &v, k + 3 * own.len() + 10, MIN_WORD_SIMILARITY, MAX_WORD_DOC_FRACTION)?;
    let same = |w: &str| own.iter().any(|q| q.len() >= 3 && (w.starts_with(q.as_str()) || q.starts_with(w)));
    Ok(near.into_iter().filter(|(w, _)| !same(w)).take(k).collect())
}

/// One ranked result and why it matched.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: String,
    /// Matched the query's words (FTS5).
    pub keyword: bool,
    /// Cosine similarity when the vector side matched, else `None`.
    pub similarity: Option<f32>,
}

/// Ranked event ids (annotation rows included, as in FTS) for `query`.
/// `fts_query` is the keyword form of the query; the vector side embeds `query`.
/// Without an embedder, or if embedding the query fails, this is keyword search.
pub fn hybrid_search(
    store: &Store,
    embedder: Option<&dyn Embedder>,
    query: &str,
    fts_query: &str,
    limit: usize,
) -> Result<Vec<String>> {
    Ok(hybrid_search_hits(store, embedder, query, fts_query, limit)?.into_iter().map(|h| h.id).collect())
}

/// `hybrid_search`, keeping which side(s) each result came from.
pub fn hybrid_search_hits(
    store: &Store,
    embedder: Option<&dyn Embedder>,
    query: &str,
    fts_query: &str,
    limit: usize,
) -> Result<Vec<Hit>> {
    let mut keyword = store.search(fts_query)?;
    let semantic = match embedder {
        Some(e) => match e.embed_query(query) {
            Ok(v) => store.nearest(e.model(), &v, DEPTH, MIN_SIMILARITY)?,
            Err(err) => {
                tracing::warn!("query embedding failed, keyword only: {err:#}");
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    if semantic.is_empty() {
        keyword.truncate(limit);
        return Ok(keyword.into_iter().map(|id| Hit { id, keyword: true, similarity: None }).collect());
    }
    let mut scores: std::collections::HashMap<String, f32> = std::collections::HashMap::new();
    let mut hits: std::collections::HashMap<String, Hit> = std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let keyword: Vec<(String, Option<f32>)> = keyword.into_iter().take(DEPTH).map(|id| (id, None)).collect();
    let semantic: Vec<(String, Option<f32>)> = semantic.into_iter().map(|(id, sim)| (id, Some(sim))).collect();
    for (is_keyword, list) in [(true, keyword), (false, semantic)] {
        for (rank, (id, sim)) in list.into_iter().enumerate() {
            let s = scores.entry(id.clone()).or_insert_with(|| {
                order.push(id.clone());
                0.0
            });
            *s += 1.0 / (RRF_K + rank as f32 + 1.0);
            let h = hits.entry(id.clone()).or_insert(Hit { id, keyword: false, similarity: None });
            h.keyword |= is_keyword;
            h.similarity = h.similarity.or(sim);
        }
    }
    // Stable on ties: first-seen order (keyword list first).
    order.sort_by(|a, b| scores[b].total_cmp(&scores[a]));
    order.truncate(limit);
    Ok(order.into_iter().map(|id| hits.remove(&id).unwrap()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Journal;

    /// Axis 0 = vehicles, 1 = fruit, 2 = everything else. "car" and
    /// "automobile" share an axis, so they match semantically with zero
    /// keyword overlap — the thing FTS can't do.
    struct Fake(&'static str);
    impl Embedder for Fake {
        fn model(&self) -> &str {
            self.0
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let t = t.to_lowercase();
                    let mut v = [0.0f32; 3];
                    for w in t.split(|c: char| !c.is_alphanumeric()) {
                        match w {
                            "car" | "automobile" | "truck" => v[0] += 1.0,
                            "apple" | "pear" | "fruit" => v[1] += 1.0,
                            "" => {}
                            _ => v[2] += 0.1,
                        }
                    }
                    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
                    v.iter().map(|x| x / n).collect()
                })
                .collect())
        }
    }

    fn journal() -> (tempfile::TempDir, Journal) {
        let d = tempfile::tempdir().unwrap();
        let j = Journal::init(&d.path().join("j"), "pw").unwrap();
        (d, j)
    }

    fn texts(j: &Journal, ids: &[String]) -> Vec<String> {
        ids.iter()
            .map(|id| j.store.get_event(id).unwrap().unwrap().fts_text().unwrap().to_string())
            .collect()
    }

    #[test]
    fn embed_pending_is_incremental_and_idempotent() {
        let (_d, j) = journal();
        let e = Fake("fake-1");
        j.capture_text("an apple").unwrap();
        j.capture_text("a car").unwrap();
        assert_eq!(embed_pending(&j.store, &e, 100).unwrap(), 2);
        assert_eq!(embed_pending(&j.store, &e, 100).unwrap(), 0);
        j.capture_text("a pear").unwrap();
        assert_eq!(embed_pending(&j.store, &e, 100).unwrap(), 1);
    }

    #[test]
    fn batches_are_bounded() {
        let (_d, j) = journal();
        let e = Fake("fake-1");
        for i in 0..5 {
            j.capture_text(&format!("note {i}")).unwrap();
        }
        assert_eq!(embed_pending(&j.store, &e, 2).unwrap(), 2);
        assert_eq!(embed_pending(&j.store, &e, 2).unwrap(), 2);
        assert_eq!(embed_pending(&j.store, &e, 2).unwrap(), 1);
    }

    #[test]
    fn vectors_find_meaning_with_no_shared_words() {
        let (_d, j) = journal();
        let e = Fake("fake-1");
        j.capture_text("bought an apple").unwrap();
        j.capture_text("my automobile broke down").unwrap();
        embed_pending(&j.store, &e, 100).unwrap();
        // Keyword search alone: nothing.
        assert!(j.store.search("\"car\"").unwrap().is_empty());
        let ids = hybrid_search(&j.store, Some(&e), "car", "\"car\"", 5).unwrap();
        assert_eq!(texts(&j, &ids)[0], "my automobile broke down");
    }

    #[test]
    fn entry_matching_both_ways_outranks_single_way_matches() {
        let (_d, j) = journal();
        let e = Fake("fake-1");
        j.capture_text("the car is red").unwrap(); // keyword + semantic
        j.capture_text("automobile club").unwrap(); // semantic only
        j.capture_text("a car park of wonders and other long words here").unwrap(); // keyword, weak semantic
        embed_pending(&j.store, &e, 100).unwrap();
        let ids = hybrid_search(&j.store, Some(&e), "car", "\"car\"", 5).unwrap();
        assert_eq!(texts(&j, &ids)[0], "the car is red");
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn hits_say_whether_keyword_meaning_or_both_matched() {
        let (_d, j) = journal();
        let e = Fake("fake-1");
        j.capture_text("the car is red").unwrap(); // keyword + semantic
        j.capture_text("automobile club").unwrap(); // semantic only
        embed_pending(&j.store, &e, 100).unwrap();
        let hits = hybrid_search_hits(&j.store, Some(&e), "car", "\"car\"", 5).unwrap();
        let by_text = |t: &str| {
            let id = hits.iter().map(|h| &h.id).find(|id| texts(&j, std::slice::from_ref(*id))[0] == t).unwrap();
            hits.iter().find(|h| &h.id == id).unwrap()
        };
        let both = by_text("the car is red");
        assert!(both.keyword && both.similarity.is_some());
        let sem = by_text("automobile club");
        assert!(!sem.keyword && sem.similarity.unwrap() >= MIN_SIMILARITY);
        // No embedder: every hit is keyword with no similarity.
        let kw = hybrid_search_hits(&j.store, None, "car", "\"car\"", 5).unwrap();
        assert!(kw.iter().all(|h| h.keyword && h.similarity.is_none()));
    }

    #[test]
    fn related_terms_are_the_nearest_words_in_the_journal() {
        let (_d, j) = journal();
        let e = Fake("fake-1");
        j.capture_text("my automobile and truck broke down").unwrap();
        j.capture_text("an apple a day").unwrap();
        embed_pending_words(&j.store, &e, 100).unwrap();
        let words: Vec<String> = related_terms(&j.store, &e, "car", 5).unwrap().into_iter().map(|(w, _)| w).collect();
        assert!(words.contains(&"automobile".to_string()) && words.contains(&"truck".to_string()), "{words:?}");
        assert!(!words.contains(&"apple".to_string()), "{words:?}");
        // Idempotent, and a query's own words (and their inflections) are never suggested.
        assert_eq!(embed_pending_words(&j.store, &e, 100).unwrap(), 0);
        let again: Vec<String> = related_terms(&j.store, &e, "trucks", 5).unwrap().into_iter().map(|(w, _)| w).collect();
        assert!(!again.contains(&"truck".to_string()), "{again:?}");
    }

    #[test]
    fn words_in_most_notes_are_not_suggested() {
        let (_d, j) = journal();
        let e = Fake("fake-1");
        for t in ["truck one", "truck two", "truck three", "truck four automobile"] {
            j.capture_text(t).unwrap();
        }
        embed_pending_words(&j.store, &e, 100).unwrap();
        let words: Vec<String> = related_terms(&j.store, &e, "car", 5).unwrap().into_iter().map(|(w, _)| w).collect();
        assert_eq!(words, ["automobile"]);
    }

    #[test]
    fn without_an_embedder_it_is_plain_keyword_search() {
        let (_d, j) = journal();
        j.capture_text("my automobile").unwrap();
        j.capture_text("the car").unwrap();
        let ids = hybrid_search(&j.store, None, "car", "\"car\"", 5).unwrap();
        assert_eq!(texts(&j, &ids), ["the car"]);
    }

    #[test]
    fn unembedded_rows_still_found_by_keyword_and_model_swap_reembeds() {
        let (_d, j) = journal();
        j.capture_text("the car").unwrap();
        let a = Fake("fake-1");
        // Not embedded yet: hybrid degrades to keyword rather than failing.
        let ids = hybrid_search(&j.store, Some(&a), "car", "\"car\"", 5).unwrap();
        assert_eq!(ids.len(), 1);
        embed_pending(&j.store, &a, 100).unwrap();
        // A different model must not reuse the old vectors.
        let b = Fake("fake-2");
        assert_eq!(embed_pending(&j.store, &b, 100).unwrap(), 1);
    }

    #[test]
    fn limit_is_honoured() {
        let (_d, j) = journal();
        let e = Fake("fake-1");
        for i in 0..6 {
            j.capture_text(&format!("car number {i}")).unwrap();
        }
        embed_pending(&j.store, &e, 100).unwrap();
        assert_eq!(hybrid_search(&j.store, Some(&e), "car", "\"car\"", 3).unwrap().len(), 3);
    }
}
