use crate::error::{AppError, Result};
use crate::semantic::embed::SemanticEmbedder;
use crate::semantic::types::DEFAULT_EMBEDDING_BATCH_SIZE;
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use std::path::PathBuf;

#[cfg(feature = "release-dynamic-ort")]
use std::{path::Path, sync::Once};

pub struct FastembedEmbedder {
    model: TextEmbedding,
}

impl FastembedEmbedder {
    pub fn new() -> Result<Self> {
        Self::new_with_download_progress(crate::semantic::cache::model_cache_dir(), true)
    }

    pub fn new_quiet() -> Result<Self> {
        Self::new_with_download_progress(crate::semantic::cache::model_cache_dir(), false)
    }

    pub fn cache_dir() -> PathBuf {
        crate::semantic::cache::model_cache_dir()
    }

    fn new_with_download_progress(
        cache_dir: PathBuf,
        show_download_progress: bool,
    ) -> Result<Self> {
        init_onnx_runtime();
        let model = TextEmbedding::try_new(
            TextInitOptions::new(EmbeddingModel::BGESmallENV15)
                .with_cache_dir(cache_dir)
                .with_show_download_progress(show_download_progress),
        )
        .map_err(to_config_error)?;
        Ok(Self { model })
    }
}

impl SemanticEmbedder for FastembedEmbedder {
    fn embed_passages(&mut self, passages: &[String]) -> Result<Vec<Vec<f32>>> {
        self.model
            .embed(
                prefixed_passages(passages),
                Some(DEFAULT_EMBEDDING_BATCH_SIZE),
            )
            .map_err(to_config_error)
    }

    fn embed_query(&mut self, query: &str) -> Result<Option<Vec<f32>>> {
        let embeddings = self
            .model
            .embed(
                vec![prefixed_query(query)],
                Some(DEFAULT_EMBEDDING_BATCH_SIZE),
            )
            .map_err(to_config_error)?;
        Ok(embeddings.first().cloned())
    }
}

pub fn prefixed_query(query: &str) -> String {
    format!("Represent this sentence for searching relevant passages: {query}")
}

pub fn prefixed_passages(passages: &[String]) -> Vec<String> {
    passages.to_vec()
}

#[cfg(feature = "release-dynamic-ort")]
fn init_onnx_runtime() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Some(path) = bundled_onnx_runtime_path() {
            let _ = ort::init_from(path).map(|builder| builder.commit());
        }
    });
}

#[cfg(not(feature = "release-dynamic-ort"))]
fn init_onnx_runtime() {}

#[cfg(feature = "release-dynamic-ort")]
fn bundled_onnx_runtime_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for candidate in onnx_runtime_candidates(dir) {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(feature = "release-dynamic-ort")]
fn onnx_runtime_candidates(dir: &Path) -> [PathBuf; 4] {
    [
        dir.join("libonnxruntime.so"),
        dir.join("lib").join("libonnxruntime.so"),
        dir.join("libonnxruntime.dylib"),
        dir.join("lib").join("libonnxruntime.dylib"),
    ]
}

fn to_config_error(err: impl std::fmt::Display) -> AppError {
    AppError::ConfigError(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_query_for_fastembed() {
        assert_eq!(
            prefixed_query("rust cache"),
            "Represent this sentence for searching relevant passages: rust cache"
        );
    }

    #[test]
    fn leaves_passages_unprefixed_for_fastembed() {
        assert_eq!(
            prefixed_passages(&["one".to_string(), "two".to_string()]),
            vec!["one".to_string(), "two".to_string()]
        );
    }
}
