//! The `hf` CLI's environment variables, read as the `hf` CLI reads them, so
//! rearview downloads the semantic model into the Hugging Face cache the `hf`
//! CLI uses and sends its login token.

pub mod cache;
pub mod token;

use crate::semantic::path_expansion::{VariableLookup, expand_user_and_variables};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// `HF_ENDPOINT`, the server to download the model from; `None` when it is
/// unset, empty or not valid Unicode.
pub fn hugging_face_endpoint() -> Option<String> {
    HuggingFaceEnvironment::of_process()
        .non_empty("HF_ENDPOINT")?
        .into_string()
        .ok()
}

/// hf-hub's cache folder while `HF_HOME` is unset. Call only when a home
/// folder exists; hf-hub panics without one.
fn hf_hub_default_cache() -> PathBuf {
    hf_hub::Cache::default().path().clone()
}

/// Environment inputs that place the Hugging Face cache and the login token.
struct HuggingFaceEnvironment<'a> {
    /// Looks up an environment variable's raw value by name.
    lookup_variable: VariableLookup<'a>,
    /// Replaces a leading `~` in a variable's value.
    home: Option<PathBuf>,
    /// hf-hub's cache folder while `HF_HOME` is unset; `None` without a home
    /// folder.
    hf_hub_default: Option<PathBuf>,
    /// Contains `.cache/huggingface/hub` when nothing else places the
    /// Hugging Face cache.
    current_dir: PathBuf,
}

impl HuggingFaceEnvironment<'static> {
    fn of_process() -> Self {
        let home = home::home_dir();
        Self {
            lookup_variable: &process_variable,
            hf_hub_default: home.as_ref().map(|_| hf_hub_default_cache()),
            home,
            current_dir: PathBuf::from("."),
        }
    }
}

impl HuggingFaceEnvironment<'_> {
    /// The variable's value; `None` when it is unset or empty.
    fn non_empty(&self, name: &str) -> Option<OsString> {
        (self.lookup_variable)(name).filter(|value| !value.is_empty())
    }

    /// True when the variable is `1`, `ON`, `YES` or `TRUE` in any case, as
    /// `huggingface_hub`'s `_is_true` reads it.
    fn is_true(&self, name: &str) -> bool {
        self.non_empty(name).is_some_and(|value| {
            ["1", "ON", "YES", "TRUE"]
                .iter()
                .any(|true_value| value.eq_ignore_ascii_case(true_value))
        })
    }

    /// The variable's value with `~` and variables expanded as Python expands
    /// them; `None` when it is unset or empty.
    fn expanded(&self, name: &str) -> Option<PathBuf> {
        self.non_empty(name).map(|value| self.expand(&value))
    }

    fn expand(&self, value: &OsStr) -> PathBuf {
        expand_user_and_variables(value, self.home.as_deref(), self.lookup_variable)
    }

    /// `entry` in `$HF_HOME`, else in `$XDG_CACHE_HOME/huggingface`. Each
    /// joined path is expanded, then expanded again with `entry`, as
    /// `huggingface_hub/constants.py` composes them.
    fn in_configured_hugging_face_home(&self, entry: &str) -> Option<PathBuf> {
        let hugging_face_home = self.expanded("HF_HOME").or_else(|| {
            let xdg_cache_home = PathBuf::from(self.non_empty("XDG_CACHE_HOME")?);
            Some(self.expand(xdg_cache_home.join("huggingface").as_os_str()))
        })?;
        Some(self.expand(hugging_face_home.join(entry).as_os_str()))
    }
}

fn process_variable(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

#[cfg(test)]
mod fixture {
    use super::HuggingFaceEnvironment;
    use crate::error::Result;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    /// Variables by name over the empty temporary folder `root`.
    pub(super) struct HuggingFaceFixture {
        root: tempfile::TempDir,
        variables: HashMap<&'static str, OsString>,
        has_home_folder: bool,
    }

    impl HuggingFaceFixture {
        /// Every folder variable but `HF_HOME` set to its own folder.
        pub(super) fn with_every_variable() -> Self {
            let fixture = Self::without_variables();
            [
                "FASTEMBED_CACHE_DIR",
                "HF_HUB_CACHE",
                "HUGGINGFACE_HUB_CACHE",
                "XDG_CACHE_HOME",
            ]
            .into_iter()
            .fold(fixture, |fixture, name| {
                let folder = fixture.path(name);
                fixture.with(name, folder)
            })
        }

        pub(super) fn without_variables() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
                variables: HashMap::new(),
                has_home_folder: true,
            }
        }

        pub(super) fn without(mut self, names: &[&'static str]) -> Self {
            for name in names {
                self.variables.remove(name);
            }
            self
        }

        pub(super) fn with(mut self, name: &'static str, value: impl Into<OsString>) -> Self {
            self.variables.insert(name, value.into());
            self
        }

        pub(super) fn without_home_folder(mut self) -> Self {
            self.has_home_folder = false;
            self
        }

        pub(super) fn path(&self, name: &str) -> PathBuf {
            self.root.path().join(name)
        }

        pub(super) fn home(&self) -> PathBuf {
            self.path("home")
        }

        /// The folder containing hf-hub's default cache, as
        /// `~/.cache/huggingface` contains `hub`.
        pub(super) fn hf_hub_default_home(&self) -> PathBuf {
            self.path("hf-hub-default")
        }

        pub(super) fn hf_hub_default(&self) -> PathBuf {
            self.hf_hub_default_home().join("hub")
        }

        fn in_environment<T>(&self, operation: impl FnOnce(&HuggingFaceEnvironment<'_>) -> T) -> T {
            let variable = |name: &str| self.variables.get(name).cloned();
            operation(&HuggingFaceEnvironment {
                lookup_variable: &variable,
                home: self.has_home_folder.then(|| self.home()),
                hf_hub_default: self.has_home_folder.then(|| self.hf_hub_default()),
                current_dir: self.path("current"),
            })
        }

        pub(super) fn resolve(&self) -> PathBuf {
            self.in_environment(super::cache::resolve_hugging_face_cache)
        }

        pub(super) fn token(&self) -> Result<Option<String>> {
            self.in_environment(super::token::resolve_token)
        }
    }

    /// Writes `token` to `path` with a line ending, as a text editor saves it.
    pub(super) fn write_token(path: &Path, token: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("{token}\r\n")).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{HuggingFaceFixture, write_token};
    use std::path::PathBuf;

    /// `huggingface_hub` expands `HF_HOME`, or `$XDG_CACHE_HOME/huggingface`,
    /// and expands each path it joins to that again: a variable whose value
    /// starts with `~` is expanded fully.
    #[test]
    fn joined_hugging_face_home_paths_expand_again() {
        let cases = [
            ("HF_HOME", PathBuf::from("models")),
            (
                "XDG_CACHE_HOME",
                PathBuf::from("models").join("huggingface"),
            ),
        ];
        for (name, hugging_face_home) in cases {
            let fixture = HuggingFaceFixture::without_variables()
                .with("ROOT", "~/models")
                .with(name, "$ROOT");
            let expected_home = fixture.home().join(&hugging_face_home);
            write_token(&expected_home.join("token"), "expanded-home-token");

            assert_eq!(
                fixture.resolve(),
                expected_home.join("hub"),
                "{name}=$ROOT with ROOT=~/models does not place the Hugging Face cache under the home folder"
            );
            assert_eq!(
                fixture.token().unwrap().as_deref(),
                Some("expanded-home-token"),
                "{name}=$ROOT with ROOT=~/models does not place the token file under the home folder"
            );
        }
    }
}
