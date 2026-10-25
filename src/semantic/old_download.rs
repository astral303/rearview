//! Moves the semantic model an earlier release downloaded into
//! `<rearview cache>/semantic/fastembed` into the Hugging Face cache, so it is
//! not downloaded again.

use crate::error::{AppError, Result};
use std::io;
use std::path::{Path, PathBuf};

/// The old model folder after a move.
#[derive(Debug, PartialEq)]
pub(crate) enum OldModelFolder {
    Gone,
    /// The Hugging Face cache contains the model folder too. Delete this one
    /// after the model has loaded from the Hugging Face cache.
    DeleteAfterLoad(PathBuf),
}

impl OldModelFolder {
    /// Delete an old model folder left for deletion, then `<rearview
    /// cache>/semantic/fastembed` if it is empty. Call after the model has
    /// loaded from `hugging_face_cache`.
    pub(crate) fn delete_after_load(self, hugging_face_cache: &Path) -> Result<()> {
        let Self::DeleteAfterLoad(old_model) = self else {
            return Ok(());
        };
        let error = |error: io::Error| {
            AppError::ConfigError(format!(
                "Could not delete the semantic model in {} after loading it from {}: {error}",
                old_model.display(),
                hugging_face_cache.display()
            ))
        };
        std::fs::remove_dir_all(&old_model).map_err(error)?;
        delete_old_cache_if_empty(&old_model).map_err(error)
    }
}

/// Move the model folder from `old_cache` into `hugging_face_cache`, then
/// delete `old_cache` if it is empty.
pub(crate) fn move_old_download(
    old_cache: &Path,
    hugging_face_cache: &Path,
) -> Result<OldModelFolder> {
    let model_folder_name = crate::semantic::fastembed::model_folder_name()?;
    ModelFolderMove {
        old_model: old_cache.join(&model_folder_name),
        target: hugging_face_cache.join(&model_folder_name),
    }
    .run()
}

struct ModelFolderMove {
    old_model: PathBuf,
    target: PathBuf,
}

impl ModelFolderMove {
    fn run(&self) -> Result<OldModelFolder> {
        if let Some(outcome) = self.outcome_on_disk()? {
            return Ok(outcome);
        }
        if let Some(hugging_face_cache) = self.target.parent() {
            std::fs::create_dir_all(hugging_face_cache).map_err(|error| self.error(error))?;
        }
        if let Err(error) = std::fs::rename(&self.old_model, &self.target) {
            return self.after_failed_rename(error);
        }
        delete_old_cache_if_empty(&self.old_model).map_err(|error| self.error(error))?;
        Ok(OldModelFolder::Gone)
    }

    /// The step after a failed rename, decided by what is on disk now:
    /// another rearview may have moved the old model folder, or another tool
    /// may have downloaded the model.
    fn after_failed_rename(&self, error: io::Error) -> Result<OldModelFolder> {
        if let Some(outcome) = self.outcome_on_disk()? {
            return Ok(outcome);
        }
        if error.kind() != io::ErrorKind::CrossesDevices {
            return Err(self.error(error));
        }
        if let Err(error) = copy_across_volumes(&self.old_model, &self.target) {
            return self.outcome_on_disk()?.ok_or_else(|| self.error(error));
        }
        delete_old_cache_if_empty(&self.old_model).map_err(|error| self.error(error))?;
        Ok(OldModelFolder::Gone)
    }

    /// The outcome without a move: `None` while the old model folder exists
    /// and the Hugging Face cache lacks the model folder.
    fn outcome_on_disk(&self) -> Result<Option<OldModelFolder>> {
        if !self.exists(&self.old_model)? {
            return Ok(Some(OldModelFolder::Gone));
        }
        if !self.exists(&self.target)? {
            return Ok(None);
        }
        let canonical =
            |path: &Path| std::fs::canonicalize(path).map_err(|error| self.error(error));
        if canonical(&self.old_model)? == canonical(&self.target)? {
            return Ok(Some(OldModelFolder::Gone));
        }
        Ok(Some(OldModelFolder::DeleteAfterLoad(
            self.old_model.clone(),
        )))
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        path.try_exists().map_err(|error| self.error(error))
    }

    fn error(&self, error: io::Error) -> AppError {
        AppError::ConfigError(format!(
            "Could not move the semantic model from {} to {}: {error}",
            self.old_model.display(),
            self.target.display()
        ))
    }
}

/// Delete the folder containing `old_model`, `<rearview
/// cache>/semantic/fastembed`, if it is empty.
fn delete_old_cache_if_empty(old_model: &Path) -> io::Result<()> {
    let Some(old_cache) = old_model.parent() else {
        return Ok(());
    };
    match std::fs::remove_dir(old_cache) {
        Err(error)
            if !matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Err(error)
        }
        _ => Ok(()),
    }
}

/// Copy `old_model` to `target` on another volume, then delete `old_model`.
/// The copy goes into a temporary folder renamed to `target` once complete:
/// hf-hub reads only whether a file is present, so a partly copied file must
/// not appear under `target`.
fn copy_across_volumes(old_model: &Path, target: &Path) -> io::Result<()> {
    let temporary = temporary_copy_path(target);
    match std::fs::remove_dir_all(&temporary) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    copy_folder(old_model, &temporary)?;
    if let Err(error) = std::fs::rename(&temporary, target) {
        // The rename failure is the one reported. A later copy deletes the
        // temporary folder first if this deletion fails too.
        let _ = std::fs::remove_dir_all(&temporary);
        return Err(error);
    }
    std::fs::remove_dir_all(old_model)
}

/// The folder beside `target` a copy is written into before it is renamed to
/// `target`.
fn temporary_copy_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".rearview-copy");
    target.with_file_name(name)
}

/// Copy `source` to the new folder `destination`, recreating each symbolic
/// link with its own target, relative targets included.
fn copy_folder(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::create_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            copy_link(&from, &to, &file_type)?;
        } else if file_type.is_dir() {
            copy_folder(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_link(link: &Path, copy: &Path, _file_type: &std::fs::FileType) -> io::Result<()> {
    std::os::unix::fs::symlink(std::fs::read_link(link)?, copy)
}

#[cfg(windows)]
fn copy_link(link: &Path, copy: &Path, file_type: &std::fs::FileType) -> io::Result<()> {
    use std::os::windows::fs::{FileTypeExt, symlink_dir, symlink_file};
    let target = std::fs::read_link(link)?;
    if file_type.is_symlink_dir() {
        symlink_dir(target, copy)
    } else {
        symlink_file(target, copy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn model_folder_name() -> String {
        crate::semantic::fastembed::model_folder_name().unwrap()
    }

    /// The blob a snapshot file links to, relative to the snapshot file, with
    /// the platform's separator: Windows does not resolve a link target
    /// written with `/`.
    fn blob_from_snapshot() -> PathBuf {
        ["..", "..", "..", "blobs", "model-blob"].iter().collect()
    }

    /// Writes a model folder into `cache` as fastembed's download leaves it:
    /// `refs/main`, a blob, and a snapshot whose `onnx/model.onnx` links to
    /// the blob with a relative target, where the platform lets the test
    /// create a symbolic link. Returns the model folder and whether the link
    /// was created.
    fn write_model_folder(cache: &Path) -> (PathBuf, bool) {
        let model = cache.join(model_folder_name());
        fs::create_dir_all(model.join("refs")).unwrap();
        fs::write(model.join("refs").join("main"), "snapshot").unwrap();
        fs::create_dir_all(model.join("blobs")).unwrap();
        fs::write(model.join("blobs").join("model-blob"), "model bytes").unwrap();
        let snapshot = model.join("snapshots").join("snapshot");
        fs::create_dir_all(snapshot.join("onnx")).unwrap();
        fs::write(snapshot.join("tokenizer.json"), "tokenizer").unwrap();
        let model_file = snapshot.join("onnx").join("model.onnx");
        let linked = create_file_link(&blob_from_snapshot(), &model_file);
        if !linked {
            fs::write(&model_file, "model bytes").unwrap();
        }
        (model, linked)
    }

    #[cfg(unix)]
    fn create_file_link(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    /// Windows creates a symbolic link only with Developer Mode or the
    /// privilege to create one.
    #[cfg(windows)]
    fn create_file_link(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }

    /// Fails unless `model` contains the files `write_model_folder` writes,
    /// the model file a link with its relative target when `linked`.
    fn assert_model_folder_intact(model: &Path, linked: bool) {
        let snapshot = model.join("snapshots").join("snapshot");
        assert_eq!(
            fs::read_to_string(snapshot.join("tokenizer.json"))
                .ok()
                .as_deref(),
            Some("tokenizer"),
            "the tokenizer file in {} differs from the old copy's",
            model.display()
        );
        let model_file = snapshot.join("onnx").join("model.onnx");
        assert_eq!(
            fs::read_to_string(&model_file).ok().as_deref(),
            Some("model bytes"),
            "the model file in {} differs from the old copy's",
            model.display()
        );
        if linked {
            assert_eq!(
                fs::read_link(&model_file).ok(),
                Some(blob_from_snapshot()),
                "the model file's link target is not the old copy's relative target"
            );
        }
    }

    struct Folders {
        root: tempfile::TempDir,
    }

    impl Folders {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
            }
        }

        fn old_cache(&self) -> PathBuf {
            self.root.path().join("semantic").join("fastembed")
        }

        fn hugging_face_cache(&self) -> PathBuf {
            self.root.path().join("huggingface").join("hub")
        }

        fn old_model(&self) -> PathBuf {
            self.old_cache().join(model_folder_name())
        }

        fn target(&self) -> PathBuf {
            self.hugging_face_cache().join(model_folder_name())
        }

        fn move_old_download(&self) -> Result<OldModelFolder> {
            move_old_download(&self.old_cache(), &self.hugging_face_cache())
        }

        /// `ModelFolderMove::after_failed_rename` with a rename error that is
        /// not a crossing of volumes.
        fn after_failed_rename(&self) -> Result<OldModelFolder> {
            ModelFolderMove {
                old_model: self.old_model(),
                target: self.target(),
            }
            .after_failed_rename(io::Error::other("rename failed"))
        }
    }

    #[test]
    fn move_renames_old_model_folder_into_hugging_face_cache() {
        let folders = Folders::new();
        let (old_model, linked) = write_model_folder(&folders.old_cache());

        let old_copy = folders.move_old_download().unwrap();

        assert_eq!(
            old_copy,
            OldModelFolder::Gone,
            "a moved model folder is left for deletion"
        );
        assert_model_folder_intact(&folders.target(), linked);
        assert!(
            !old_model.exists(),
            "the old model folder remains after the move"
        );
        assert!(
            !folders.old_cache().exists(),
            "the empty old cache folder remains after the move"
        );
    }

    #[test]
    fn move_keeps_old_cache_folder_containing_other_files() {
        let folders = Folders::new();
        write_model_folder(&folders.old_cache());
        let other_file = folders.old_cache().join("other.txt");
        fs::write(&other_file, "other").unwrap();

        folders.move_old_download().unwrap();

        assert!(
            other_file.is_file(),
            "a file beside the old model folder is deleted by the move"
        );
    }

    #[test]
    fn move_leaves_old_copy_when_hugging_face_cache_contains_model_folder() {
        let folders = Folders::new();
        let (old_model, _) = write_model_folder(&folders.old_cache());
        fs::create_dir_all(folders.target()).unwrap();
        fs::write(folders.target().join("downloaded.txt"), "downloaded").unwrap();

        let old_copy = folders.move_old_download().unwrap();

        assert_eq!(
            old_copy,
            OldModelFolder::DeleteAfterLoad(old_model.clone()),
            "the old model folder is not left for deletion after the load"
        );
        assert!(
            old_model.is_dir(),
            "the old model folder is deleted before the load"
        );
        assert_eq!(
            fs::read_dir(folders.target()).unwrap().count(),
            1,
            "the Hugging Face cache's model folder is changed"
        );
    }

    #[test]
    fn move_without_old_download_doesnt_create_hugging_face_cache() {
        let folders = Folders::new();

        let old_copy = folders.move_old_download().unwrap();

        assert_eq!(
            old_copy,
            OldModelFolder::Gone,
            "a missing old model folder is left for deletion"
        );
        assert!(
            !folders.hugging_face_cache().exists(),
            "the Hugging Face cache is created without a model folder to move"
        );
    }

    #[test]
    fn move_doesnt_delete_model_folder_when_old_cache_is_hugging_face_cache() {
        let folders = Folders::new();
        let (model, linked) = write_model_folder(&folders.old_cache());

        let old_copy = move_old_download(&folders.old_cache(), &folders.old_cache()).unwrap();

        assert_eq!(
            old_copy,
            OldModelFolder::Gone,
            "the Hugging Face cache's own model folder is left for deletion"
        );
        assert_model_folder_intact(&model, linked);
    }

    #[test]
    fn copy_across_volumes_replaces_leftover_temporary_folder() {
        let folders = Folders::new();
        let (old_model, linked) = write_model_folder(&folders.old_cache());
        let leftover = temporary_copy_path(&folders.target());
        fs::create_dir_all(&leftover).unwrap();
        fs::write(leftover.join("partial.bin"), "partial").unwrap();

        copy_across_volumes(&old_model, &folders.target()).unwrap();

        assert_model_folder_intact(&folders.target(), linked);
        assert!(
            !folders.target().join("partial.bin").exists(),
            "the interrupted copy's file is in the model folder"
        );
        assert!(
            !leftover.exists(),
            "the temporary folder remains after the copy"
        );
        assert!(
            !old_model.exists(),
            "the old model folder remains after the copy"
        );
    }

    #[test]
    fn failed_rename_after_old_model_folder_moved_away_continues_without_old_copy() {
        let folders = Folders::new();
        write_model_folder(&folders.hugging_face_cache());

        let old_copy = folders.after_failed_rename().unwrap();

        assert_eq!(
            old_copy,
            OldModelFolder::Gone,
            "a moved-away old model folder is left for deletion"
        );
    }

    #[test]
    fn failed_rename_after_model_folder_appeared_leaves_old_copy_for_deletion() {
        let folders = Folders::new();
        let (old_model, _) = write_model_folder(&folders.old_cache());
        fs::create_dir_all(folders.target()).unwrap();

        let old_copy = folders.after_failed_rename().unwrap();

        assert_eq!(
            old_copy,
            OldModelFolder::DeleteAfterLoad(old_model.clone()),
            "the old model folder is not left for deletion after the load"
        );
        assert!(
            old_model.is_dir(),
            "the old model folder is deleted before the load"
        );
    }

    #[test]
    fn failed_rename_otherwise_names_both_folders() {
        let folders = Folders::new();
        write_model_folder(&folders.old_cache());

        let error = folders
            .after_failed_rename()
            .expect_err("a failed rename without a race or another volume is not reported");

        let message = error.to_string();
        assert!(
            message.contains(&folders.old_model().display().to_string())
                && message.contains(&folders.target().display().to_string()),
            "the move error does not name both folders: {message}"
        );
    }
}
