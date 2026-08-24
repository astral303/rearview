use crate::error::{AppError, Result};
use crate::semantic::embed::SemanticEmbedder;
use crate::semantic::types::DEFAULT_EMBEDDING_BATCH_SIZE;
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use std::path::PathBuf;

#[cfg(feature = "release-dynamic-ort")]
use std::{path::Path, sync::OnceLock};

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
        init_onnx_runtime()?;
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

/// Point `ort` at the ONNX Runtime shipped beside the binary.
///
/// The library has to be located here rather than left to `ort`, which falls
/// back to opening it by bare name. That searches the loader's own paths and
/// never the directory the binary sits in, and when it comes up empty `ort`
/// 2.0.0-rc.12 blocks forever instead of returning an error, because the error
/// it builds re-enters the `OnceLock` it is initialising.
///
/// This covers the library being absent, which is the case that was
/// reproduced. A library that is present but failing to open reaches `ort`
/// through a different path that was not tested.
#[cfg(feature = "release-dynamic-ort")]
fn init_onnx_runtime() -> Result<()> {
    static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        let path = bundled_onnx_runtime_path().ok_or_else(missing_runtime_message)?;
        ort::init_from(&path)
            .map(|environment| environment.commit())
            .map_err(|error| {
                format!(
                    "Failed to load ONNX Runtime from {}: {error}",
                    path.display()
                )
            })?;
        Ok(())
    })
    .clone()
    .map_err(AppError::ConfigError)
}

#[cfg(feature = "release-dynamic-ort")]
fn missing_runtime_message() -> String {
    let searched = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .map(|dir| format!(" Looked in {} and its lib directory.", dir.display()))
        .unwrap_or_default();
    format!(
        "ONNX Runtime was not found, so semantic search cannot start.{searched} \
         The release archive ships it in lib beside the rearview binary; keep \
         the two together when you move the binary."
    )
}

#[cfg(not(feature = "release-dynamic-ort"))]
fn init_onnx_runtime() -> Result<()> {
    Ok(())
}

#[cfg(feature = "release-dynamic-ort")]
fn bundled_onnx_runtime_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    onnx_runtime_candidates(dir)
        .into_iter()
        .find(|candidate| candidate.exists())
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
