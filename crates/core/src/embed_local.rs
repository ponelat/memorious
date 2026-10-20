//! The on-device embedding model (feature `embeddings`): BGE-small-en-v1.5
//! (384 dims, ~130MB, downloaded once into `cache_dir` on first use) run
//! through ONNX Runtime. Only peers that opt in ever load this.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use ort::ep::ExecutionProvider as _;

use crate::search::Embedder;

const QUERY_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

fn run(mut model: std::sync::MutexGuard<'_, TextEmbedding>, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let mut out = model.embed(texts, Some(16))?;
    for v in &mut out {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        v.iter_mut().for_each(|x| *x /= n);
    }
    Ok(out)
}

pub struct LocalEmbedder {
    model: Mutex<TextEmbedding>,
}

impl LocalEmbedder {
    /// Loads (downloading if needed) the model. Blocking — call from a
    /// blocking thread.
    pub fn load(cache_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(cache_dir)?;
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::BGESmallENV15)
                .with_cache_dir(cache_dir.to_path_buf())
                // CPU only: ONNX's CoreML provider recompiles for every new
                // batch shape, which made each sweep batch take seconds.
                .with_execution_providers(vec![ort::ep::CPU::default().build()]),
        )
        .context("load embedding model")?;
        Ok(Self { model: Mutex::new(model) })
    }
}

impl Embedder for LocalEmbedder {
    fn model(&self) -> &str {
        "bge-small-en-v1.5"
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let guard = self.model.lock().unwrap();
        run(guard, texts)
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        // Wait for the model at most QUERY_WAIT; a backfill batch holds it.
        let deadline = std::time::Instant::now() + QUERY_WAIT;
        let guard = loop {
            match self.model.try_lock() {
                Ok(g) => break g,
                Err(std::sync::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(_) => anyhow::bail!("embedding model busy"),
            }
        };
        run(guard, &[text.to_string()])?.pop().ok_or_else(|| anyhow::anyhow!("no vector"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Downloads the real model (~130MB); run with `--ignored`.
    #[test]
    #[ignore]
    fn real_model_ranks_related_text_above_unrelated() {
        let dir = tempfile::tempdir().unwrap();
        let e = LocalEmbedder::load(dir.path()).unwrap();
        let v = e
            .embed(&["my car broke down".into(), "the automobile would not start".into(), "bought fresh apples".into()])
            .unwrap();
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert_eq!(v[0].len(), 384);
        assert!(dot(&v[0], &v[1]) > dot(&v[0], &v[2]) + 0.1);
    }
}
