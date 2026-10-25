//! The login token a download of the semantic model sends, found as the `hf`
//! CLI finds it.

use super::HuggingFaceEnvironment;
use crate::error::{AppError, Result};
use std::path::{Path, PathBuf};

/// The `hf` CLI's login token, for downloading the model.
pub fn hugging_face_token() -> Result<Option<String>> {
    resolve_token(&HuggingFaceEnvironment::of_process())
}

/// The login token as the `hf` CLI finds it: none while
/// `HF_HUB_DISABLE_IMPLICIT_TOKEN` is true; else `HF_TOKEN`, else
/// `HUGGING_FACE_HUB_TOKEN`; else the file at `HF_TOKEN_PATH`, else `token` in
/// the `hf` CLI's home folder. A blank token counts as missing, and so does a
/// missing file.
pub(super) fn resolve_token(environment: &HuggingFaceEnvironment<'_>) -> Result<Option<String>> {
    if environment.is_true("HF_HUB_DISABLE_IMPLICIT_TOKEN") {
        return Ok(None);
    }
    if let Some((name, value)) = ["HF_TOKEN", "HUGGING_FACE_HUB_TOKEN"]
        .into_iter()
        .find_map(|name| Some((name, environment.non_empty(name)?)))
    {
        let value = value
            .into_string()
            .map_err(|_| AppError::ConfigError(format!("{name} is not valid Unicode")))?;
        if let Some(token) = cleaned_token(&value) {
            return Ok(Some(token));
        }
    }
    let token_path = environment
        .expanded("HF_TOKEN_PATH")
        .or_else(|| environment.in_configured_hugging_face_home("token"))
        .or_else(|| hf_hub_default_home(environment).map(|home| home.join("token")));
    match token_path {
        Some(token_path) => read_token_file(&token_path),
        None => Ok(None),
    }
}

/// The token in the file at `token_path`; `None` when the file is missing or
/// the token in it is blank.
pub(crate) fn read_token_file(token_path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(token_path) {
        Ok(contents) => Ok(cleaned_token(&contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::ConfigError(format!(
            "Could not read the Hugging Face token file {}: {error}",
            token_path.display()
        ))),
    }
}

/// `token` without line breaks and surrounding whitespace, as
/// `huggingface_hub` cleans it; `None` when nothing is left.
fn cleaned_token(token: &str) -> Option<String> {
    let token = token.replace(['\r', '\n'], "");
    Some(token.trim().to_string()).filter(|token| !token.is_empty())
}

/// The folder containing hf-hub's default cache.
fn hf_hub_default_home(environment: &HuggingFaceEnvironment<'_>) -> Option<PathBuf> {
    let hf_hub_default = environment.hf_hub_default.as_deref()?;
    hf_hub_default.parent().map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::super::fixture::{HuggingFaceFixture, write_token};
    use std::path::PathBuf;

    /// A token file at each place the token can come from: `HF_TOKEN_PATH`,
    /// `HF_HOME`, `XDG_CACHE_HOME` and hf-hub's default home folder.
    fn with_every_token_file() -> HuggingFaceFixture {
        let fixture = HuggingFaceFixture::without_variables();
        let token_path = fixture.path("token-path").join("token");
        let hf_home = fixture.path("hf-home");
        let xdg_cache_home = fixture.path("xdg-cache-home");
        write_token(&token_path, "token-path-token");
        write_token(&hf_home.join("token"), "hf-home-token");
        write_token(
            &xdg_cache_home.join("huggingface").join("token"),
            "xdg-cache-home-token",
        );
        write_token(
            &fixture.hf_hub_default_home().join("token"),
            "hf-hub-default-token",
        );
        fixture
            .with("HF_TOKEN_PATH", token_path)
            .with("HF_HOME", hf_home)
            .with("XDG_CACHE_HOME", xdg_cache_home)
    }

    #[test]
    fn token_prefers_hf_token_to_token_files() {
        let fixture = with_every_token_file().with("HF_TOKEN", " hf-token\n");

        assert_eq!(
            fixture.token().unwrap().as_deref(),
            Some("hf-token"),
            "HF_TOKEN, trimmed, is not chosen before the token files"
        );
    }

    /// huggingface_hub takes `HF_TOKEN or HUGGING_FACE_HUB_TOKEN`, then trims
    /// it: a blank `HF_TOKEN` is chosen and trims to nothing.
    #[test]
    fn token_reads_token_file_after_blank_hf_token() {
        let fixture = with_every_token_file()
            .with("HF_TOKEN", " \r\n")
            .with("HUGGING_FACE_HUB_TOKEN", "hub-token");

        assert_eq!(
            fixture.token().unwrap().as_deref(),
            Some("token-path-token"),
            "the token file is not read after a blank HF_TOKEN"
        );
    }

    #[test]
    fn token_reads_hf_token_without_token_file() {
        let fixture = HuggingFaceFixture::without_variables().with("HF_TOKEN", "hf-token");

        assert_eq!(
            fixture.token().unwrap().as_deref(),
            Some("hf-token"),
            "HF_TOKEN alone is not the token"
        );
    }

    #[test]
    fn token_reads_hugging_face_hub_token_without_token_file() {
        let fixture =
            HuggingFaceFixture::without_variables().with("HUGGING_FACE_HUB_TOKEN", " hub-token\n");

        assert_eq!(
            fixture.token().unwrap().as_deref(),
            Some("hub-token"),
            "HUGGING_FACE_HUB_TOKEN alone, trimmed, is not the token"
        );
    }

    #[test]
    fn token_prefers_hf_token_to_hugging_face_hub_token() {
        let fixture = HuggingFaceFixture::without_variables()
            .with("HF_TOKEN", "hf-token")
            .with("HUGGING_FACE_HUB_TOKEN", "hub-token");

        assert_eq!(
            fixture.token().unwrap().as_deref(),
            Some("hf-token"),
            "HUGGING_FACE_HUB_TOKEN is used while HF_TOKEN is set"
        );
    }

    #[test]
    fn token_reads_hugging_face_hub_token_after_empty_hf_token() {
        let fixture = with_every_token_file()
            .with("HF_TOKEN", "")
            .with("HUGGING_FACE_HUB_TOKEN", "hub-token");

        assert_eq!(
            fixture.token().unwrap().as_deref(),
            Some("hub-token"),
            "an empty HF_TOKEN hides HUGGING_FACE_HUB_TOKEN"
        );
    }

    /// Fixtures whose token would be read: from `HF_TOKEN`, and from an
    /// unreadable token file that fails the lookup when read.
    fn with_token_to_disable() -> [HuggingFaceFixture; 2] {
        let token_file_folder = HuggingFaceFixture::without_variables();
        let folder = token_file_folder.path("token-folder");
        std::fs::create_dir_all(&folder).unwrap();
        [
            with_every_token_file().with("HF_TOKEN", "hf-token"),
            token_file_folder.with("HF_TOKEN_PATH", folder),
        ]
    }

    #[test]
    fn token_disabled_by_hf_hub_disable_implicit_token_is_missing() {
        for value in ["1", "ON", "yes", "True"] {
            for fixture in with_token_to_disable() {
                let fixture = fixture.with("HF_HUB_DISABLE_IMPLICIT_TOKEN", value);

                assert_eq!(
                    fixture.token().ok(),
                    Some(None),
                    "a token is read with HF_HUB_DISABLE_IMPLICIT_TOKEN={value}"
                );
            }
        }
    }

    #[test]
    fn token_doesnt_follow_other_hf_hub_disable_implicit_token_values() {
        let fixture = HuggingFaceFixture::without_variables()
            .with("HF_TOKEN", "hf-token")
            .with("HF_HUB_DISABLE_IMPLICIT_TOKEN", "0");

        assert_eq!(
            fixture.token().unwrap().as_deref(),
            Some("hf-token"),
            "HF_HUB_DISABLE_IMPLICIT_TOKEN=0 leaves out the token"
        );
    }

    #[test]
    fn token_prefers_hf_token_path_to_hf_home() {
        let fixture = with_every_token_file();

        assert_eq!(
            fixture.token().unwrap().as_deref(),
            Some("token-path-token"),
            "the file at HF_TOKEN_PATH, trimmed, is not chosen before the home folders' token files"
        );
    }

    #[test]
    fn token_prefers_hf_home_to_xdg_cache_home() {
        let fixture = with_every_token_file().without(&["HF_TOKEN_PATH"]);

        assert_eq!(
            fixture.token().unwrap().as_deref(),
            Some("hf-home-token"),
            "HF_HOME's token file is not chosen before XDG_CACHE_HOME's"
        );
    }

    #[test]
    fn token_prefers_xdg_cache_home_to_hf_hub_default() {
        let fixture = with_every_token_file().without(&["HF_TOKEN_PATH", "HF_HOME"]);

        assert_eq!(
            fixture.token().unwrap().as_deref(),
            Some("xdg-cache-home-token"),
            "XDG_CACHE_HOME's token file is not chosen before hf-hub's default home folder's"
        );
    }

    #[test]
    fn token_falls_back_to_hf_hub_default_home_folder() {
        let fixture =
            with_every_token_file().without(&["HF_TOKEN_PATH", "HF_HOME", "XDG_CACHE_HOME"]);

        assert_eq!(
            fixture.token().unwrap().as_deref(),
            Some("hf-hub-default-token"),
            "the token file beside hf-hub's default cache is not read when every variable is unset"
        );
    }

    #[test]
    fn token_without_home_folder_or_variables_is_missing() {
        let fixture = HuggingFaceFixture::without_variables().without_home_folder();
        write_token(
            &fixture
                .path("current")
                .join(".cache")
                .join("huggingface")
                .join("token"),
            "current-directory-token",
        );

        assert_eq!(
            fixture.token().unwrap(),
            None,
            "a token is read without a home folder or a variable placing it"
        );
    }

    #[test]
    fn token_expands_hf_token_path() {
        let tilde_variable = if cfg!(windows) {
            "~\\%NAME%"
        } else {
            "~/$NAME"
        };
        let fixture = HuggingFaceFixture::without_variables()
            .with("NAME", "token-file")
            .with("HF_TOKEN_PATH", tilde_variable);
        write_token(
            &PathBuf::from(format!(
                "{}{}token-file",
                fixture.home().display(),
                std::path::MAIN_SEPARATOR
            )),
            "expanded-token",
        );

        assert_eq!(
            fixture.token().unwrap().as_deref(),
            Some("expanded-token"),
            "HF_TOKEN_PATH's tilde and variable are not expanded"
        );
    }

    #[test]
    fn token_file_missing_is_missing_token() {
        let fixture = HuggingFaceFixture::without_variables();
        let hf_home = fixture.path("hf-home");
        let fixture = fixture.with("HF_HOME", hf_home);

        assert_eq!(
            fixture.token().unwrap(),
            None,
            "a missing token file yields a token"
        );
    }

    #[test]
    fn token_path_naming_folder_is_reported() {
        let fixture = HuggingFaceFixture::without_variables();
        let folder = fixture.path("token-folder");
        std::fs::create_dir_all(&folder).unwrap();
        let fixture = fixture.with("HF_TOKEN_PATH", folder.clone());

        let error = fixture
            .token()
            .expect_err("a token path naming a folder is read as a missing token");

        assert!(
            error.to_string().contains(&folder.display().to_string()),
            "the token read error does not name the path: {error}"
        );
    }
}
