//! The parity record pins `load_model` to fastembed's `TextEmbedding::try_new`
//! for one fastembed version: the loader inputs rearview derives, and
//! `try_new`'s embeddings of two passages. The regular test run checks the
//! record against `Cargo.lock` and the derived inputs without loading the
//! model; the ignored test refreshes the record after a fastembed upgrade.

use super::*;
use serde::{Deserialize, Serialize};

const RECORD_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/semantic/fastembed_parity.json"
);

const REFRESH_COMMAND: &str = "cargo test --lib -- --ignored \
     semantic::fastembed::parity_record::load_model_embeds_as_fastembeds_loader_does";

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct ParityRecord {
    fastembed_version: String,
    loader_inputs: RecordedLoaderInputs,
    passages: Vec<String>,
    /// `try_new`'s embedding of each passage.
    embeddings: Vec<Vec<f32>>,
}

/// `LoaderInputs` serialised. Fastembed types without a serialised form are
/// recorded as their `Debug` text.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct RecordedLoaderInputs {
    model_code: String,
    model_file: String,
    additional_files: Vec<String>,
    tokenizer_files: Vec<String>,
    pooling: String,
    quantization: String,
    output_key: String,
    max_length: usize,
}

impl From<LoaderInputs> for RecordedLoaderInputs {
    fn from(inputs: LoaderInputs) -> Self {
        Self {
            model_code: inputs.model_code,
            model_file: inputs.model_file,
            additional_files: inputs.additional_files,
            tokenizer_files: inputs.tokenizer_files.map(str::to_string).to_vec(),
            pooling: format!("{:?}", inputs.pooling),
            quantization: format!("{:?}", inputs.quantization),
            output_key: format!("{:?}", inputs.output_key),
            max_length: inputs.init_options.max_length,
        }
    }
}

/// The inputs `load_model` passes fastembed, as the record serialises them.
fn recorded_loader_inputs() -> RecordedLoaderInputs {
    LoaderInputs::of_semantic_model().unwrap().into()
}

/// A short passage, and one longer than the model's maximum length, so
/// truncation is compared too.
fn passages() -> Vec<String> {
    vec![
        "how do I configure the retry budget for the ingest pipeline".to_string(),
        "the ingest pipeline retries a failed batch after a backoff ".repeat(100),
    ]
}

fn locked_fastembed_version() -> String {
    #[derive(Deserialize)]
    struct Lockfile {
        package: Vec<LockedPackage>,
    }
    #[derive(Deserialize)]
    struct LockedPackage {
        name: String,
        version: String,
    }
    let lockfile_path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock");
    let lockfile: Lockfile =
        toml::from_str(&std::fs::read_to_string(lockfile_path).expect("Cargo.lock is readable"))
            .expect("Cargo.lock parses");
    lockfile
        .package
        .into_iter()
        .find(|package| package.name == "fastembed")
        .expect("Cargo.lock pins fastembed")
        .version
}

#[test]
fn parity_record_matches_locked_fastembed_and_derived_loader_inputs() {
    let record_text = std::fs::read_to_string(RECORD_PATH).unwrap_or_else(|error| {
        panic!("the parity record {RECORD_PATH} is unreadable ({error}); create it with `{REFRESH_COMMAND}`")
    });
    let record: ParityRecord =
        serde_json::from_str(&record_text).expect("the parity record parses");

    assert_eq!(
        record.fastembed_version,
        locked_fastembed_version(),
        "the parity record is for another fastembed version; refresh it with `{REFRESH_COMMAND}` and review its diff"
    );
    assert_eq!(
        record.loader_inputs,
        recorded_loader_inputs(),
        "load_model's inputs differ from the parity record's"
    );
    assert_eq!(
        record.passages,
        passages(),
        "the parity record's passages differ from the test's; refresh it with `{REFRESH_COMMAND}`"
    );
    assert!(
        record.embeddings.len() == record.passages.len()
            && record
                .embeddings
                .iter()
                .all(|embedding| embedding.len() == MODEL_DIMENSIONS),
        "the parity record does not contain one {MODEL_DIMENSIONS}-value embedding per passage"
    );
}

/// Loads the model through `try_new` and through `load_model`, requires them
/// to embed exactly alike, then rewrites the parity record. Reads the Hugging
/// Face cache semantic search uses, downloading the model into it when it is
/// missing.
#[cfg(not(feature = "release-dynamic-ort"))]
#[test]
#[ignore = "loads the semantic model; run after a fastembed upgrade to refresh the parity record"]
fn load_model_embeds_as_fastembeds_loader_does() {
    let folder = FastembedEmbedder::cache_dir();
    assert!(
        std::env::var_os("HF_HOME").is_none(),
        "HF_HOME is set, so fastembed's loader would read it instead of {}",
        folder.display()
    );
    let passages = passages();
    let mut fastembed_loaded = TextEmbedding::try_new(
        TextInitOptions::new(EMBEDDING_MODEL)
            .with_cache_dir(folder.clone())
            .with_show_download_progress(false),
    )
    .expect("fastembed's loader fails");
    let fastembed_embeddings = fastembed_loaded.embed(passages.clone(), None).unwrap();
    // The model is public; a download sends no login token.
    let mut rearview_loaded =
        load_model(&folder, || Ok(None), false).expect("rearview's loader fails");
    let rearview_embeddings = rearview_loaded.embed(passages.clone(), None).unwrap();

    assert_eq!(
        rearview_embeddings, fastembed_embeddings,
        "rearview's loader embeds a short or an over-limit passage differently from fastembed's"
    );
    let record = ParityRecord {
        fastembed_version: locked_fastembed_version(),
        loader_inputs: recorded_loader_inputs(),
        passages,
        embeddings: fastembed_embeddings,
    };
    let record_text = serde_json::to_string_pretty(&record).unwrap() + "\n";
    std::fs::write(RECORD_PATH, record_text).expect("the parity record is writable");
}
