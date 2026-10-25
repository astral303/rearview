use crate::error::{AppError, Result};
use crate::semantic::embed::SemanticEmbedder;
use crate::semantic::hugging_face::cache::HuggingFaceCache;
use crate::semantic::types::{DEFAULT_EMBEDDING_BATCH_SIZE, MODEL_DIMENSIONS};
use fastembed::{
    EmbeddingModel, InitOptionsUserDefined, ModelInfo, OutputKey, Pooling, QuantizationMode,
    TextEmbedding, TextInitOptions, TokenizerFiles, UserDefinedEmbeddingModel,
};
use hf_hub::api::sync::{Api, ApiBuilder};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[cfg(feature = "release-dynamic-ort")]
use std::sync::OnceLock;

const EMBEDDING_MODEL: EmbeddingModel = EmbeddingModel::BGESmallENV15;

pub struct FastembedEmbedder {
    model: TextEmbedding,
}

/// The model folder's name in a Hugging Face cache: `models--<owner>--<name>`.
pub fn model_folder_name() -> Result<String> {
    Ok(hf_hub::Repo::model(model_info()?.model_code.clone()).folder_name())
}

/// fastembed's description of the semantic model.
fn model_info() -> Result<&'static ModelInfo<EmbeddingModel>> {
    TextEmbedding::get_model_info(&EMBEDDING_MODEL).map_err(to_config_error)
}

impl FastembedEmbedder {
    pub fn new() -> Result<Self> {
        Self::new_with_download_progress(true)
    }

    pub fn new_quiet() -> Result<Self> {
        Self::new_with_download_progress(false)
    }

    pub fn cache_dir() -> PathBuf {
        HuggingFaceCache::locate().path().to_path_buf()
    }

    fn new_with_download_progress(show_download_progress: bool) -> Result<Self> {
        init_onnx_runtime()?;
        let model = HuggingFaceCache::locate().load_with(|hugging_face_cache| {
            load_model(
                hugging_face_cache,
                crate::semantic::hugging_face::token::hugging_face_token,
                show_download_progress,
            )
        })?;
        Ok(Self { model })
    }
}

/// The model from `cache_dir`, downloaded into it first if missing, sending
/// the login token `token` returns.
///
/// `TextEmbedding::try_new` would download into `HF_HOME`, without `hub`,
/// whenever that is set, whatever folder it is given. This loads the files
/// fastembed names for the model the way `try_new` does, but from
/// `cache_dir`.
fn load_model(
    cache_dir: &Path,
    token: impl FnOnce() -> Result<Option<String>>,
    show_download_progress: bool,
) -> Result<TextEmbedding> {
    let inputs = LoaderInputs::of_semantic_model()?;
    let paths = download_missing_model_files(
        cache_dir,
        &inputs.model_code,
        &inputs.file_names(),
        token,
        show_download_progress,
    )?;
    let read = |file_name: &str| -> Result<Vec<u8>> {
        let path = &paths[file_name];
        std::fs::read(path).map_err(|error| to_config_error(format!("{}: {error}", path.display())))
    };
    let [
        tokenizer_file,
        config_file,
        special_tokens_map_file,
        tokenizer_config_file,
    ] = inputs.tokenizer_files;
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: read(tokenizer_file)?,
        config_file: read(config_file)?,
        special_tokens_map_file: read(special_tokens_map_file)?,
        tokenizer_config_file: read(tokenizer_config_file)?,
    };
    let mut model = UserDefinedEmbeddingModel::new(read(&inputs.model_file)?, tokenizer_files)
        .with_quantization(inputs.quantization);
    model.pooling = inputs.pooling.clone();
    model.output_key = inputs.output_key.clone();
    for file_name in &inputs.additional_files {
        let referenced_name = Path::new(file_name).file_name().map_or_else(
            || file_name.clone(),
            |name| name.to_string_lossy().into_owned(),
        );
        model = model.with_external_initializer(referenced_name, read(file_name)?);
    }
    TextEmbedding::try_new_from_user_defined(model, inputs.init_options.clone())
        .map_err(to_config_error)
}

/// The inputs `load_model` passes fastembed, derived as `try_new` derives
/// them. The parity record serialises them.
struct LoaderInputs {
    model_code: String,
    model_file: String,
    additional_files: Vec<String>,
    /// In `TokenizerFiles` field order.
    tokenizer_files: [&'static str; 4],
    pooling: Option<Pooling>,
    quantization: QuantizationMode,
    output_key: Option<OutputKey>,
    /// `try_new`'s options, its maximum length included.
    init_options: InitOptionsUserDefined,
}

impl LoaderInputs {
    fn of_semantic_model() -> Result<Self> {
        let info = model_info()?;
        Ok(Self {
            model_code: info.model_code.clone(),
            model_file: info.model_file.clone(),
            additional_files: info.additional_files.clone(),
            tokenizer_files: TOKENIZER_FILE_NAMES,
            pooling: TextEmbedding::get_default_pooling_method(&EMBEDDING_MODEL),
            quantization: TextEmbedding::get_quantization_mode(&EMBEDDING_MODEL),
            output_key: info.output_key.clone(),
            init_options: TextInitOptions::new(EMBEDDING_MODEL).into(),
        })
    }

    /// Every file the model loads from: the tokenizer files, the model file
    /// and its additional files.
    fn file_names(&self) -> Vec<&str> {
        self.tokenizer_files
            .into_iter()
            .chain([self.model_file.as_str()])
            .chain(self.additional_files.iter().map(String::as_str))
            .collect()
    }
}

/// Each of `model_code`'s `file_names` in `cache_dir`, downloading the files
/// missing from it. `token` is called only for a download.
fn download_missing_model_files<'a>(
    cache_dir: &Path,
    model_code: &str,
    file_names: &[&'a str],
    token: impl FnOnce() -> Result<Option<String>>,
    show_download_progress: bool,
) -> Result<HashMap<&'a str, PathBuf>> {
    let cached = hf_hub::Cache::new(cache_dir.to_path_buf()).model(model_code.to_string());
    let cached_paths = file_names
        .iter()
        .map(|&file_name| Some((file_name, cached.get(file_name)?)))
        .collect::<Option<HashMap<_, _>>>();
    if let Some(paths) = cached_paths {
        return Ok(paths);
    }
    let repository = hugging_face_api(cache_dir, token()?, show_download_progress)?
        .model(model_code.to_string());
    file_names
        .iter()
        .map(|&file_name| {
            let path = repository.get(file_name).map_err(to_config_error)?;
            Ok((file_name, path))
        })
        .collect()
}

/// The files fastembed's `load_tokenizer_hf_hub` reads for `try_new`, in
/// `TokenizerFiles` field order.
const TOKENIZER_FILE_NAMES: [&str; 4] = [
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];

/// hf-hub's client over `cache_dir`, at `HF_ENDPOINT` when that is set, as
/// fastembed's own download is.
fn hugging_face_api(
    cache_dir: &Path,
    token: Option<String>,
    show_download_progress: bool,
) -> Result<Api> {
    // `ApiBuilder::new` opens hf-hub's default cache and panics without a
    // home folder. `from_cache` also reads a token file beside `cache_dir`;
    // `token`, the one the `hf` CLI reads, replaces it.
    let builder = ApiBuilder::from_cache(hf_hub::Cache::new(cache_dir.to_path_buf()))
        .with_token(token)
        .with_progress(show_download_progress);
    let builder = match crate::semantic::hugging_face::hugging_face_endpoint() {
        Some(endpoint) => builder.with_endpoint(endpoint),
        None => builder,
    };
    builder.build().map_err(to_config_error)
}

impl SemanticEmbedder for FastembedEmbedder {
    fn dimensions(&self) -> usize {
        MODEL_DIMENSIONS
    }

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
mod parity_record;

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

    #[test]
    fn model_dimensions_match_fastembeds_model_info() {
        assert_eq!(MODEL_DIMENSIONS, model_info().unwrap().dim);
    }

    fn model_code() -> String {
        model_info().unwrap().model_code.clone()
    }

    fn model_folder_name() -> String {
        super::model_folder_name().unwrap()
    }

    /// The path hf-hub's cache gives `file_name` in the snapshot
    /// `place_in_cache` writes.
    fn snapshot_path(cache: &Path, file_name: &str) -> PathBuf {
        cache
            .join(model_folder_name())
            .join("snapshots")
            .join("snapshot")
            .join(file_name)
    }

    /// Writes `file_names` as empty plain files into the snapshot `snapshot`
    /// of the model folder in the Hugging Face cache `cache`, with `refs/main`
    /// naming that snapshot.
    fn place_in_cache(cache: &Path, file_names: &[&str]) {
        let refs = cache.join(model_folder_name()).join("refs");
        std::fs::create_dir_all(&refs).unwrap();
        std::fs::write(refs.join("main"), "snapshot").unwrap();
        for file_name in file_names {
            let path = snapshot_path(cache, file_name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
    }

    /// A token lookup that fails as an unreadable token file does: the token
    /// path names a folder.
    fn unreadable_token_file(root: &Path) -> (PathBuf, impl FnOnce() -> Result<Option<String>>) {
        let token_path = root.join("token-folder");
        std::fs::create_dir_all(&token_path).unwrap();
        let lookup_path = token_path.clone();
        (token_path, move || {
            crate::semantic::hugging_face::token::read_token_file(&lookup_path)
        })
    }

    const MODEL_FILES: [&str; 2] = ["tokenizer.json", "onnx/model.onnx"];

    #[test]
    fn model_files_on_disk_load_without_reading_token() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("hub");
        place_in_cache(&cache, &MODEL_FILES);
        let (_, token) = unreadable_token_file(root.path());

        let paths = download_missing_model_files(&cache, &model_code(), &MODEL_FILES, token, false)
            .expect("an unreadable token file fails a load whose files are on disk");

        assert_eq!(
            paths["onnx/model.onnx"],
            snapshot_path(&cache, "onnx/model.onnx"),
            "the model file is not read from the cache"
        );
    }

    #[test]
    fn missing_model_file_reports_unreadable_token_file() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("hub");
        place_in_cache(&cache, &["tokenizer.json"]);
        let (token_path, token) = unreadable_token_file(root.path());

        let error = download_missing_model_files(&cache, &model_code(), &MODEL_FILES, token, false)
            .expect_err("a download proceeds past an unreadable token file");

        assert!(
            error
                .to_string()
                .contains(&token_path.display().to_string()),
            "the error does not name the token file: {error}"
        );
    }

    /// Folders under `root`: a Hugging Face cache, and the folder earlier
    /// releases downloaded the model into.
    struct LoadFolders {
        hugging_face_cache: PathBuf,
        old_cache: PathBuf,
    }

    impl LoadFolders {
        fn under(root: &Path) -> Self {
            Self {
                hugging_face_cache: root.join("hub"),
                old_cache: root.join("semantic").join("fastembed"),
            }
        }

        /// The paths of `MODEL_FILES` after a load through `HuggingFaceCache`,
        /// reading the token file only for a download.
        fn load(
            &self,
            token: impl FnOnce() -> Result<Option<String>>,
        ) -> Result<HashMap<&'static str, PathBuf>> {
            HuggingFaceCache::new(self.hugging_face_cache.clone(), self.old_cache.clone())
                .load_with(|hugging_face_cache| {
                    download_missing_model_files(
                        hugging_face_cache,
                        &model_code(),
                        &MODEL_FILES,
                        token,
                        false,
                    )
                })
        }

        fn old_model(&self) -> PathBuf {
            self.old_cache.join(model_folder_name())
        }
    }

    #[test]
    fn old_download_loads_from_hugging_face_cache_without_download() {
        let root = tempfile::tempdir().unwrap();
        let folders = LoadFolders::under(root.path());
        place_in_cache(&folders.old_cache, &MODEL_FILES);
        let (_, token) = unreadable_token_file(root.path());

        let paths = folders
            .load(token)
            .expect("loading an old download reads the token file");

        assert_eq!(
            paths["onnx/model.onnx"],
            snapshot_path(&folders.hugging_face_cache, "onnx/model.onnx"),
            "the model file is not read from the Hugging Face cache"
        );
        assert!(
            !folders.old_model().exists(),
            "the old model folder remains after the load"
        );
    }

    #[test]
    fn load_from_hugging_face_cache_deletes_old_copy() {
        let root = tempfile::tempdir().unwrap();
        let folders = LoadFolders::under(root.path());
        place_in_cache(&folders.hugging_face_cache, &MODEL_FILES);
        let downloaded = snapshot_path(&folders.hugging_face_cache, "onnx/model.onnx");
        std::fs::write(&downloaded, "downloaded").unwrap();
        place_in_cache(&folders.old_cache, &MODEL_FILES);

        folders.load(|| Ok(None)).unwrap();

        assert!(
            !folders.old_model().exists(),
            "the old model folder remains after a load from the Hugging Face cache"
        );
        assert_eq!(
            std::fs::read_to_string(&downloaded).unwrap(),
            "downloaded",
            "the Hugging Face cache's model file is changed by the load"
        );
    }

    #[test]
    fn load_from_hugging_face_cache_deletes_emptied_old_cache_folder() {
        let root = tempfile::tempdir().unwrap();
        let folders = LoadFolders::under(root.path());
        place_in_cache(&folders.hugging_face_cache, &MODEL_FILES);
        place_in_cache(&folders.old_cache, &MODEL_FILES);

        folders.load(|| Ok(None)).unwrap();

        assert!(
            !folders.old_cache.exists(),
            "the emptied old cache folder remains after a load from the Hugging Face cache"
        );
    }

    #[test]
    fn failed_load_keeps_old_copy() {
        let root = tempfile::tempdir().unwrap();
        let folders = LoadFolders::under(root.path());
        place_in_cache(&folders.hugging_face_cache, &["tokenizer.json"]);
        place_in_cache(&folders.old_cache, &MODEL_FILES);
        let (_, token) = unreadable_token_file(root.path());

        folders
            .load(token)
            .expect_err("a load missing a model file and the token succeeds");

        assert!(
            snapshot_path(&folders.old_cache, "onnx/model.onnx").is_file(),
            "the old model folder is deleted after a failed load"
        );
    }

    #[test]
    fn model_folder_is_named_for_hugging_face_repository() {
        assert_eq!(
            model_folder_name(),
            "models--Xenova--bge-small-en-v1.5",
            "the model folder name differs from the one the hf CLI uses"
        );
    }

    #[cfg(feature = "release-dynamic-ort")]
    #[test]
    fn finds_runtime_beside_binary() {
        let install = tempfile::tempdir().unwrap();
        let runtime = install.path().join("libonnxruntime.so");
        std::fs::write(&runtime, b"").unwrap();

        assert_eq!(resolve_runtime(install.path()).unwrap(), runtime);
    }

    #[cfg(feature = "release-dynamic-ort")]
    #[test]
    fn finds_runtime_bundled_under_lib() {
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
    fn names_directory_when_no_runtime_is_bundled() {
        let install = tempfile::tempdir().unwrap();

        let error = resolve_runtime(install.path()).unwrap_err();

        assert!(error.contains("ONNX Runtime was not found"));
        assert!(error.contains(&install.path().display().to_string()));
    }
}
