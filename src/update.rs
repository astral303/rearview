use crate::error::{AppError, Result};
use std::path::Path;
use std::process::Command;

const REPO: &str = "astral303/rearview";
const BIN_NAME: &str = crate::APP_NAME;
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Map OS/arch to the release artifact suffix used in GitHub releases.
fn platform_suffix() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("darwin-arm64"),
        ("macos", "x86_64") => Ok("darwin-amd64"),
        ("linux", "x86_64") => Ok("linux-amd64"),
        ("windows", "x86_64") => Ok("windows-amd64"),
        (os, arch) => Err(AppError::UpdateError(format!(
            "Unsupported platform: {os}/{arch}"
        ))),
    }
}

/// Check if the binary is managed by Homebrew.
fn is_homebrew_install(exe_path: &Path) -> bool {
    let path_str = exe_path.to_string_lossy();
    path_str.contains("/Cellar/")
}

/// Check if the binary is managed by Scoop.
///
/// Scoop installs to `<root>/apps/<app>/<version>/<app>.exe` and shims that
/// path onto `PATH`. The root moves with `SCOOP`, so the two directories above
/// the version identify the install; `current` is a junction to the version in
/// use, which `canonicalize` has already resolved by the time this runs.
fn is_scoop_install(exe_path: &Path) -> bool {
    let components: Vec<String> = exe_path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    let mut above_the_file = components.iter().rev().skip(2);
    matches!(
        (above_the_file.next(), above_the_file.next()),
        (Some(app), Some(apps)) if app == BIN_NAME && apps == "apps"
    )
}

/// Fetch the latest release tag from GitHub API using curl.
fn fetch_latest_version() -> Result<String> {
    let output = Command::new("curl")
        .args([
            "-sSf",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        ])
        .output()
        .map_err(|e| AppError::UpdateError(format!("Failed to run curl: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::UpdateError(format!(
            "Failed to fetch latest release: {}",
            stderr.trim()
        )));
    }

    let body: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| AppError::UpdateError(format!("Failed to parse GitHub API response: {e}")))?;

    let tag = body["tag_name"]
        .as_str()
        .ok_or_else(|| AppError::UpdateError("No tag_name in GitHub API response".to_string()))?;

    Ok(tag.strip_prefix('v').unwrap_or(tag).to_string())
}

/// Download a URL to a file path using curl.
fn download(url: &str, dest: &Path) -> Result<()> {
    let status = Command::new("curl")
        .args([
            "-sSLf",
            "--connect-timeout",
            "10",
            "--max-time",
            "120",
            "-o",
        ])
        .arg(dest)
        .arg(url)
        .status()
        .map_err(|e| AppError::UpdateError(format!("Failed to run curl: {e}")))?;

    if !status.success() {
        return Err(AppError::UpdateError(format!("Download failed: {url}")));
    }
    Ok(())
}

/// Extract a tar.gz archive into a directory.
fn extract_tar(archive: &Path, dest: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .status()
        .map_err(|e| AppError::UpdateError(format!("Failed to run tar: {e}")))?;

    if !status.success() {
        return Err(AppError::UpdateError(
            "Failed to extract archive".to_string(),
        ));
    }
    Ok(())
}

/// Compute the SHA-256 hash of a file, hex-encoded.
fn sha256_of(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let mut file = std::fs::File::open(path)
        .map_err(|e| AppError::UpdateError(format!("Failed to open downloaded file: {e}")))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|e| AppError::UpdateError(format!("Failed to read downloaded file: {e}")))?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Verify SHA-256 checksum of a file against the expected checksum line.
fn verify_checksum(file: &Path, expected_line: &str) -> Result<()> {
    let expected_hash = expected_line
        .split_whitespace()
        .next()
        .ok_or_else(|| AppError::UpdateError("Invalid checksum file format".to_string()))?;

    let actual_hash = sha256_of(file)?;
    if actual_hash != expected_hash {
        return Err(AppError::UpdateError(format!(
            "Checksum mismatch!\n  Expected: {expected_hash}\n  Got:      {actual_hash}"
        )));
    }
    Ok(())
}

fn install_support_files(extract_dir: &Path, current_exe: &Path) -> Result<()> {
    let exe_dir = current_exe
        .parent()
        .ok_or_else(|| AppError::UpdateError("Could not determine binary directory".to_string()))?;
    let lib_dir = extract_dir.join("lib");
    if !lib_dir.exists() {
        return Ok(());
    }

    let dest_lib_dir = exe_dir.join("lib");
    std::fs::create_dir_all(&dest_lib_dir)
        .map_err(|e| AppError::UpdateError(format!("Failed to create library directory: {e}")))?;
    for entry in std::fs::read_dir(&lib_dir)
        .map_err(|e| AppError::UpdateError(format!("Failed to read library directory: {e}")))?
    {
        let entry = entry
            .map_err(|e| AppError::UpdateError(format!("Failed to read library entry: {e}")))?;
        let file_type = entry
            .file_type()
            .map_err(|e| AppError::UpdateError(format!("Failed to inspect library entry: {e}")))?;
        if file_type.is_file() {
            std::fs::copy(entry.path(), dest_lib_dir.join(entry.file_name()))
                .map_err(|e| AppError::UpdateError(format!("Failed to install library: {e}")))?;
        }
    }

    create_runtime_symlink(exe_dir, "libonnxruntime.so")?;
    create_runtime_symlink(exe_dir, "libonnxruntime.dylib")?;
    Ok(())
}

#[cfg(unix)]
fn create_runtime_symlink(exe_dir: &Path, name: &str) -> Result<()> {
    use std::os::unix::fs::symlink;

    let target = Path::new("lib").join(name);
    let link = exe_dir.join(name);
    let _ = std::fs::remove_file(&link);
    if exe_dir.join(&target).exists() {
        symlink(&target, &link).map_err(|e| {
            AppError::UpdateError(format!("Failed to install library symlink: {e}"))
        })?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_runtime_symlink(_exe_dir: &Path, _name: &str) -> Result<()> {
    Ok(())
}

/// Replace the current binary with the new one, with rollback on failure.
fn replace_binary(new_binary: &Path, current_exe: &Path) -> Result<()> {
    let exe_dir = current_exe
        .parent()
        .ok_or_else(|| AppError::UpdateError("Could not determine binary directory".to_string()))?;

    // A previous update on Windows cannot delete the running image it moved
    // aside, so clear that leftover before staging the next one.
    let backup = exe_dir.join(format!(".{BIN_NAME}.old"));
    let _ = std::fs::remove_file(&backup);

    // Copy to destination directory to avoid EXDEV (cross-device rename)
    let staged = exe_dir.join(format!(".{BIN_NAME}.new"));
    std::fs::copy(new_binary, &staged)
        .map_err(|e| AppError::UpdateError(format!("Failed to copy new binary: {e}")))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| AppError::UpdateError(format!("Failed to set permissions: {e}")))?;
    }

    // Rename current -> .old, then staged -> current
    std::fs::rename(current_exe, &backup)
        .map_err(|e| AppError::UpdateError(format!("Failed to move current binary aside: {e}")))?;

    if let Err(e) = std::fs::rename(&staged, current_exe) {
        // Rollback: restore the original
        let _ = std::fs::rename(&backup, current_exe);
        return Err(AppError::UpdateError(format!(
            "Failed to install new binary (rolled back): {e}"
        )));
    }

    // Cleanup
    let _ = std::fs::remove_file(&backup);
    Ok(())
}

fn do_update(
    pb: &indicatif::ProgressBar,
    artifact_name: &str,
    current_exe: &Path,
) -> Result<String> {
    let latest_version = fetch_latest_version()?;

    if latest_version == CURRENT_VERSION {
        return Ok(format!("Already up to date (v{CURRENT_VERSION})"));
    }

    pb.set_message(format!("Downloading v{latest_version}..."));

    let tmp = tempfile::tempdir()
        .map_err(|e| AppError::UpdateError(format!("Failed to create temp directory: {e}")))?;
    let tar_path = tmp.path().join(format!("{artifact_name}.tar.gz"));
    let sha_path = tmp.path().join(format!("{artifact_name}.sha256"));

    let base_url = format!("https://github.com/{REPO}/releases/download/v{latest_version}");

    download(&format!("{base_url}/{artifact_name}.tar.gz"), &tar_path)?;
    download(&format!("{base_url}/{artifact_name}.sha256"), &sha_path)?;

    pb.set_message("Verifying checksum...");
    let sha_content = std::fs::read_to_string(&sha_path)
        .map_err(|e| AppError::UpdateError(format!("Failed to read checksum file: {e}")))?;
    verify_checksum(&tar_path, &sha_content)?;

    pb.set_message("Installing...");
    let extract_dir = tmp.path().join("extract");
    std::fs::create_dir(&extract_dir)
        .map_err(|e| AppError::UpdateError(format!("Failed to create extract dir: {e}")))?;
    extract_tar(&tar_path, &extract_dir)?;

    let binary_file_name = format!("{BIN_NAME}{}", std::env::consts::EXE_SUFFIX);
    let new_binary = extract_dir.join(&binary_file_name);
    if !new_binary.exists() {
        return Err(AppError::UpdateError(format!(
            "Extracted archive does not contain '{binary_file_name}' binary"
        )));
    }

    replace_binary(&new_binary, current_exe)?;
    install_support_files(&extract_dir, current_exe)?;

    Ok(format!(
        "Updated {BIN_NAME} v{CURRENT_VERSION} -> v{latest_version}"
    ))
}

pub fn run() -> Result<()> {
    let current_exe = std::env::current_exe()
        .map_err(|e| AppError::UpdateError(format!("Could not determine executable path: {e}")))?;

    // Guard: package-manager installs, whose records this update would not
    // change (canonicalize to resolve symlinks and Scoop's `current` junction)
    let canonical_exe = std::fs::canonicalize(&current_exe).unwrap_or(current_exe.clone());
    if is_homebrew_install(&canonical_exe) {
        return Err(AppError::UpdateError(format!(
            "{BIN_NAME} is managed by Homebrew. Run `brew upgrade {BIN_NAME}` instead."
        )));
    }
    if is_scoop_install(&canonical_exe) {
        return Err(AppError::UpdateError(format!(
            "{BIN_NAME} is managed by Scoop. Run `scoop update {BIN_NAME}` instead."
        )));
    }

    let platform = platform_suffix()?;
    let artifact_name = format!("{BIN_NAME}-{platform}");

    let pb = indicatif::ProgressBar::new_spinner();
    pb.set_style(
        indicatif::ProgressStyle::default_spinner()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
            .template("{spinner:.blue} {msg}")
            .unwrap(),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(120));
    pb.set_message("Checking for updates...");

    match do_update(&pb, &artifact_name, &canonical_exe) {
        Ok(msg) => {
            pb.finish_with_message(format!("✔ {msg}"));
            Ok(())
        }
        Err(e) => {
            pb.finish_with_message("✘ Update failed".to_string());
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_suffix_current() {
        let suffix = platform_suffix().unwrap();
        assert!(
            [
                "darwin-arm64",
                "darwin-amd64",
                "linux-amd64",
                "windows-amd64"
            ]
            .contains(&suffix)
        );
    }

    #[test]
    fn sha256_of_matches_the_published_digest_of_abc() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("abc.txt");
        std::fs::write(&file, "abc").unwrap();

        assert_eq!(
            sha256_of(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verify_checksum_reads_the_shasum_line_format_and_rejects_a_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rearview-x.tar.gz");
        std::fs::write(&file, "abc").unwrap();
        let line =
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  rearview-x.tar.gz\n";

        verify_checksum(&file, line).unwrap();
        assert!(verify_checksum(&file, "0000  rearview-x.tar.gz").is_err());
    }

    #[test]
    fn test_is_homebrew_cellar() {
        assert!(is_homebrew_install(Path::new(
            "/opt/homebrew/Cellar/rearview/0.1.42/bin/rearview"
        )));
    }

    #[test]
    fn test_is_homebrew_prefix() {
        assert!(is_homebrew_install(Path::new(
            "/usr/local/Cellar/rearview/0.1.42/bin/rearview"
        )));
    }

    #[test]
    fn test_is_not_homebrew_local_bin() {
        assert!(!is_homebrew_install(Path::new("/usr/local/bin/rearview")));
    }

    #[test]
    fn test_is_not_homebrew_home() {
        assert!(!is_homebrew_install(Path::new(
            "/home/user/.local/bin/rearview"
        )));
    }

    #[test]
    fn scoop_app_directory_is_a_scoop_install() {
        assert!(is_scoop_install(Path::new(
            r"C:\Users\dev\scoop\apps\rearview\0.3.0\rearview.exe"
        )));
    }

    /// `SCOOP` moves the root, and Windows paths compare case-insensitively.
    #[test]
    fn a_relocated_scoop_root_is_still_a_scoop_install() {
        assert!(is_scoop_install(Path::new(
            r"D:\Tools\Scoop\Apps\rearview\current\rearview.exe"
        )));
    }

    #[test]
    fn a_plain_directory_is_not_a_scoop_install() {
        assert!(!is_scoop_install(Path::new(
            r"C:\Users\dev\bin\rearview.exe"
        )));
        assert!(!is_scoop_install(Path::new(
            "/home/user/.local/bin/rearview"
        )));
    }

    /// Another app's Scoop directory, in case this binary sits beside one.
    #[test]
    fn another_scoop_app_is_not_this_scoop_install() {
        assert!(!is_scoop_install(Path::new(
            r"C:\Users\dev\scoop\apps\ripgrep\14.1.1\rearview.exe"
        )));
    }
}
