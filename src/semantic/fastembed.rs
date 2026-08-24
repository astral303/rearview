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
        let path = resolve_runtime(&executable_dir()?)?;
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

#[cfg(not(feature = "release-dynamic-ort"))]
fn init_onnx_runtime() -> Result<()> {
    Ok(())
}

/// Find the ONNX Runtime bundled in `dir`, or say where it was looked for.
#[cfg(feature = "release-dynamic-ort")]
fn resolve_runtime(dir: &Path) -> std::result::Result<PathBuf, String> {
    onnx_runtime_candidates(dir)
        .into_iter()
        .find(|candidate| candidate.exists())
        .ok_or_else(|| {
            format!(
                "ONNX Runtime was not found, so semantic search cannot start. \
                 Looked in {} and its lib directory. The release archive ships \
                 it in lib beside the rearview binary; keep the two together \
                 when you move the binary.",
                dir.display()
            )
        })
}

#[cfg(feature = "release-dynamic-ort")]
fn executable_dir() -> std::result::Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("Could not locate the rearview binary: {error}"))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "The rearview binary has no parent directory.".to_string())
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

    #[cfg(feature = "release-dynamic-ort")]
    #[test]
    fn finds_the_runtime_beside_the_binary() {
        let install = tempfile::tempdir().unwrap();
        let runtime = install.path().join("libonnxruntime.so");
        std::fs::write(&runtime, b"").unwrap();

        assert_eq!(resolve_runtime(install.path()).unwrap(), runtime);
    }

    #[cfg(feature = "release-dynamic-ort")]
    #[test]
    fn finds_the_runtime_bundled_under_lib() {
        let install = tempfile::tempdir().unwrap();
        std::fs::create_dir(install.path().join("lib")).unwrap();
        let runtime = install.path().join("lib").join("libonnxruntime.dylib");
        std::fs::write(&runtime, b"").unwrap();

        assert_eq!(resolve_runtime(install.path()).unwrap(), runtime);
    }

    // ort blocks forever rather than returning an error when it cannot open the
    // library, so this miss has to be reported before ort is reached.
    #[cfg(feature = "release-dynamic-ort")]
    #[test]
    fn names_the_directory_when_no_runtime_is_bundled() {
        let install = tempfile::tempdir().unwrap();

        let error = resolve_runtime(install.path()).unwrap_err();

        assert!(error.contains("ONNX Runtime was not found"));
        assert!(error.contains(&install.path().display().to_string()));
    }
}
