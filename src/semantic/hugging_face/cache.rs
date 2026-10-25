//! The Hugging Face cache: the folder that contains the semantic model's
//! model folder, `models--<owner>--<name>`.

use super::HuggingFaceEnvironment;
use crate::error::Result;
use crate::semantic::old_download::{OldModelFolder, move_old_download};
use std::path::{Path, PathBuf};

/// The Hugging Face cache, and the folder earlier releases downloaded the
/// semantic model into.
pub struct HuggingFaceCache {
    path: PathBuf,
    /// `<rearview cache>/semantic/fastembed`.
    old_cache: PathBuf,
}

impl HuggingFaceCache {
    pub fn locate() -> Self {
        Self::new(
            resolve_hugging_face_cache(&HuggingFaceEnvironment::of_process()),
            crate::semantic::cache::old_model_cache_dir(),
        )
    }

    pub(crate) fn new(path: PathBuf, old_cache: PathBuf) -> Self {
        Self { path, old_cache }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the model with `load` from the Hugging Face cache, after moving an
    /// earlier release's download into it. An old model folder the move
    /// leaves beside the Hugging Face cache's copy is deleted after `load`
    /// succeeds.
    pub fn load_with<T>(&self, load: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
        let old_model_folder = self.move_old_download()?;
        let loaded = load(&self.path)?;
        old_model_folder.delete_after_load(&self.path)?;
        Ok(loaded)
    }

    /// Move an earlier release's download into the Hugging Face cache.
    pub(crate) fn move_old_download(&self) -> Result<OldModelFolder> {
        move_old_download(&self.old_cache, &self.path)
    }

    /// The model folder in the Hugging Face cache, when there.
    pub fn model_on_disk(&self) -> Result<Option<PathBuf>> {
        let model = self
            .path
            .join(crate::semantic::fastembed::model_folder_name()?);
        Ok(Some(model).filter(|model| model.is_dir()))
    }
}

/// The first of these set, an empty variable counting as unset:
/// `FASTEMBED_CACHE_DIR`, then the `hf` CLI's order: `HF_HUB_CACHE`,
/// `HUGGINGFACE_HUB_CACHE`, `$XDG_CACHE_HOME/huggingface/hub` while `HF_HOME`
/// is unset, `$HF_HOME/hub`, hf-hub's default. Without a home folder or any
/// of these variables, `.cache/huggingface/hub` under the current directory.
/// The `hf` CLI's variables are expanded as Python expands them;
/// `FASTEMBED_CACHE_DIR` is not, as fastembed does not expand it.
pub(super) fn resolve_hugging_face_cache(environment: &HuggingFaceEnvironment<'_>) -> PathBuf {
    environment
        .non_empty("FASTEMBED_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| environment.expanded("HF_HUB_CACHE"))
        .or_else(|| environment.expanded("HUGGINGFACE_HUB_CACHE"))
        .or_else(|| {
            environment
                .in_configured_hugging_face_home("hub")
                .or_else(|| environment.hf_hub_default.clone())
        })
        .unwrap_or_else(|| {
            environment
                .current_dir
                .join(".cache")
                .join("huggingface")
                .join("hub")
        })
}

#[cfg(test)]
mod tests {
    use super::super::fixture::HuggingFaceFixture;
    use std::path::PathBuf;

    const BEFORE_XDG_CACHE_HOME: [&str; 3] = [
        "FASTEMBED_CACHE_DIR",
        "HF_HUB_CACHE",
        "HUGGINGFACE_HUB_CACHE",
    ];

    fn hugging_face_cache_under(folder: PathBuf) -> PathBuf {
        folder.join("huggingface").join("hub")
    }

    #[test]
    fn hugging_face_cache_prefers_fastembed_cache_dir() {
        let fixture = HuggingFaceFixture::with_every_variable();

        assert_eq!(
            fixture.resolve(),
            fixture.path("FASTEMBED_CACHE_DIR"),
            "FASTEMBED_CACHE_DIR is not used while the other variables are set"
        );
    }

    #[test]
    fn hugging_face_cache_prefers_hf_hub_cache_to_later_rules() {
        let fixture = HuggingFaceFixture::with_every_variable().without(&["FASTEMBED_CACHE_DIR"]);

        assert_eq!(
            fixture.resolve(),
            fixture.path("HF_HUB_CACHE"),
            "HF_HUB_CACHE is not chosen before the rules after it"
        );
    }

    #[test]
    fn hugging_face_cache_prefers_huggingface_hub_cache_to_later_rules() {
        let fixture = HuggingFaceFixture::with_every_variable()
            .without(&["FASTEMBED_CACHE_DIR", "HF_HUB_CACHE"]);

        assert_eq!(
            fixture.resolve(),
            fixture.path("HUGGINGFACE_HUB_CACHE"),
            "HUGGINGFACE_HUB_CACHE is not chosen before the rules after it"
        );
    }

    #[test]
    fn hugging_face_cache_prefers_xdg_cache_home_to_hf_hub_default() {
        let fixture = HuggingFaceFixture::with_every_variable().without(&BEFORE_XDG_CACHE_HOME);

        assert_eq!(
            fixture.resolve(),
            hugging_face_cache_under(fixture.path("XDG_CACHE_HOME")),
            "XDG_CACHE_HOME is not chosen before hf-hub's default while HF_HOME is unset"
        );
    }

    #[test]
    fn hugging_face_cache_uses_hf_home_over_xdg_cache_home() {
        let fixture = HuggingFaceFixture::with_every_variable().without(&BEFORE_XDG_CACHE_HOME);
        let hf_home = fixture.path("hf-home");
        let fixture = fixture.with("HF_HOME", hf_home);

        assert_eq!(
            fixture.resolve(),
            fixture.path("hf-home").join("hub"),
            "HF_HOME does not place the Hugging Face cache over XDG_CACHE_HOME"
        );
    }

    #[test]
    fn hugging_face_cache_falls_back_to_hf_hub_default() {
        let fixture = HuggingFaceFixture::with_every_variable()
            .without(&BEFORE_XDG_CACHE_HOME)
            .without(&["XDG_CACHE_HOME"]);

        assert_eq!(
            fixture.resolve(),
            fixture.hf_hub_default(),
            "hf-hub's default is not used when every variable is unset"
        );
    }

    #[test]
    fn hugging_face_cache_treats_empty_variables_as_unset() {
        let empty_before_xdg_cache_home =
            BEFORE_XDG_CACHE_HOME.into_iter().chain(["HF_HOME"]).fold(
                HuggingFaceFixture::with_every_variable(),
                |fixture, name| fixture.with(name, ""),
            );

        assert_eq!(
            empty_before_xdg_cache_home.resolve(),
            hugging_face_cache_under(empty_before_xdg_cache_home.path("XDG_CACHE_HOME")),
            "an empty variable before XDG_CACHE_HOME, or an empty HF_HOME, counts as set"
        );
        let every_variable_empty = empty_before_xdg_cache_home.with("XDG_CACHE_HOME", "");
        assert_eq!(
            every_variable_empty.resolve(),
            every_variable_empty.hf_hub_default(),
            "an empty XDG_CACHE_HOME counts as set"
        );
    }

    #[test]
    fn hugging_face_cache_without_home_folder_is_under_current_directory() {
        let fixture = HuggingFaceFixture::with_every_variable()
            .without(&BEFORE_XDG_CACHE_HOME)
            .without(&["XDG_CACHE_HOME"])
            .without_home_folder();

        assert_eq!(
            fixture.resolve(),
            fixture
                .path("current")
                .join(".cache")
                .join("huggingface")
                .join("hub"),
            "without a variable or a home folder, the Hugging Face cache is not under the current directory"
        );
    }

    #[test]
    fn hugging_face_cache_expands_hugging_face_variables() {
        let tilde_variable = if cfg!(windows) {
            "~\\%NAME%"
        } else {
            "~/$NAME"
        };
        let cases = [
            ("HF_HUB_CACHE", PathBuf::from("models")),
            ("HUGGINGFACE_HUB_CACHE", PathBuf::from("models")),
            ("HF_HOME", PathBuf::from("models").join("hub")),
            (
                "XDG_CACHE_HOME",
                hugging_face_cache_under(PathBuf::from("models")),
            ),
        ];
        for (name, under_home) in cases {
            let fixture = HuggingFaceFixture::with_every_variable()
                .without(&BEFORE_XDG_CACHE_HOME)
                .without(&["XDG_CACHE_HOME"])
                .with("NAME", "models")
                .with(name, tilde_variable);

            assert_eq!(
                fixture.resolve(),
                PathBuf::from(format!(
                    "{}{}{}",
                    fixture.home().display(),
                    std::path::MAIN_SEPARATOR,
                    under_home.display()
                )),
                "{name}'s tilde and variable are not expanded"
            );
        }
    }

    #[test]
    fn hugging_face_cache_doesnt_expand_fastembed_cache_dir() {
        let fixture = HuggingFaceFixture::with_every_variable()
            .with("NAME", "models")
            .with("FASTEMBED_CACHE_DIR", "~/$NAME");

        assert_eq!(
            fixture.resolve(),
            PathBuf::from("~/$NAME"),
            "FASTEMBED_CACHE_DIR is expanded; fastembed does not expand it"
        );
    }
}
