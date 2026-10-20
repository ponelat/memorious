//! The on-device embedding model (feature `embeddings`): BGE-small-en-v1.5
//! (384 dims, ~130MB, downloaded once into `cache_dir` on first use) run
//! through ONNX Runtime. Only peers that opt in ever load this.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::search::Embedder;

pub struct LocalEmbedder {
    model: Mutex<TextEmbedding>,
}

impl LocalEmbedder {
    /// Loads (downloading if needed) the model. Blocking — call from a
    /// blocking thread.
    pub fn load(cache_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(cache_dir)?;
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::BGESmallENV15).with_cache_dir(cache_dir.to_path_buf()),
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
        let mut out = self.model.lock().unwrap().embed(texts, Some(16))?;
        for v in &mut out {
            let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
            v.iter_mut().for_each(|x| *x /= n);
        }
        Ok(out)
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
