//! Claude Code's project folder names: the directory a session ran in, with
//! every character other than an ASCII letter, digit or `-` replaced by `-`.

use std::path::{Path, PathBuf};

/// The name of the project folder Claude Code keeps the sessions recorded in
/// `path` in.
pub(crate) fn convert_path_to_project_dir_name(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// A path the project folder name `encoded` could have come from, for a
/// session that recorded no cwd. The encoding is lossy: `/`, `_` and `.` all
/// became `-`, so a `-` inside a folder name decodes as a separator.
pub(super) fn decode_project_dir_name_to_path(encoded: &str) -> PathBuf {
    PathBuf::from(decode_with_double_dash_as(encoded, "__"))
}

/// Decode with a specific replacement for double dashes
fn decode_with_double_dash_as(encoded: &str, double_dash_replacement: &str) -> String {
    let mut result = String::with_capacity(encoded.len());
    let mut chars = encoded.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '-' {
            let mut count = 1;
            while chars.peek() == Some(&'-') {
                chars.next();
                count += 1;
            }

            match count {
                1 => result.push('/'),
                2 => result.push_str(double_dash_replacement),
                n => {
                    result.push('/');
                    for _ in 0..((n - 1) / 2) {
                        result.push_str(double_dash_replacement);
                    }
                    if (n - 1) % 2 == 1 {
                        result.push('/');
                    }
                }
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// The encoded worktree marker that appears in Claude project directory names.
///
/// workmux creates worktrees at `<project>/.worktrees/<branch>/` (or
/// `<project>__worktrees/<branch>/` on older setups). Both encode to
/// `--worktrees-` because `.`, `_`, and `/` all become `-` in Claude's
/// encoding scheme.
const WORKTREE_MARKER: &str = "--worktrees-";

/// The encoded project root of an encoded project directory name: everything
/// before a worktree marker (`--worktrees-`), or the whole name without one.
fn encoded_project_root(encoded: &str) -> &str {
    encoded
        .split_once(WORKTREE_MARKER)
        .map_or(encoded, |(root, _)| root)
}

/// True when two encoded project directory names share an encoded project
/// root, that is, when stripping any workmux `--worktrees-<branch>` suffix
/// yields the same string.
pub(super) fn is_same_project(a: &str, b: &str) -> bool {
    encoded_project_root(a) == encoded_project_root(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    // === Encoding tests ===

    #[test]
    fn converts_various_separators_and_punctuation() {
        let path = Path::new("/Users/raine/code/workmux/.worktrees/uncommitted");
        let converted = convert_path_to_project_dir_name(path);
        assert_eq!(
            converted,
            "-Users-raine-code-workmux--worktrees-uncommitted"
        );
    }

    #[test]
    fn preserves_alphanumeric_and_existing_dashes() {
        let path = Path::new("/tmp/foo-Bar123");
        let converted = convert_path_to_project_dir_name(path);
        assert_eq!(converted, "-tmp-foo-Bar123");
    }

    #[test]
    fn encodes_worktree_with_double_underscore() {
        let path = Path::new("/Users/raine/code/claude-history__worktrees/claude-search");
        let converted = convert_path_to_project_dir_name(path);
        assert_eq!(
            converted,
            "-Users-raine-code-claude-history--worktrees-claude-search"
        );
    }

    #[test]
    fn encodes_hidden_directory() {
        let path = Path::new("/Users/raine/dotfiles/.config/karabiner");
        let converted = convert_path_to_project_dir_name(path);
        assert_eq!(converted, "-Users-raine-dotfiles--config-karabiner");
    }

    // === Fallback decode tests (decode_with_double_dash_as) ===

    #[test]
    fn decode_with_double_dash_as_underscore() {
        let encoded = "-Users-raine-code-project--worktrees-feature";
        let decoded = decode_with_double_dash_as(encoded, "__");
        assert_eq!(decoded, "/Users/raine/code/project__worktrees/feature");
    }

    #[test]
    fn decode_with_double_dash_as_hidden_dir() {
        let encoded = "-Users-raine-dotfiles--config-karabiner";
        let decoded = decode_with_double_dash_as(encoded, "/.");
        assert_eq!(decoded, "/Users/raine/dotfiles/.config/karabiner");
    }

    #[test]
    fn decode_preserves_dashes_in_folder_names_in_fallback() {
        // Note: The fallback decode can't distinguish dashes in folder names
        // from path separators - this is expected behavior
        let encoded = "-Users-raine-code-claude-history";
        let decoded = decode_with_double_dash_as(encoded, "__");
        // This incorrectly decodes to /Users/raine/code/claude/history
        // because single dashes are treated as path separators
        assert_eq!(decoded, "/Users/raine/code/claude/history");
    }

    // === Worktree path structure tests ===

    #[test]
    fn worktree_encoded_pattern() {
        // Verify the encoding pattern for worktrees
        let path = Path::new("/Users/raine/code/WalkingMate__worktrees/template-engine");
        let encoded = convert_path_to_project_dir_name(path);
        assert_eq!(
            encoded,
            "-Users-raine-code-WalkingMate--worktrees-template-engine"
        );

        // The --worktrees- pattern should be detectable
        assert!(encoded.contains("--worktrees-"));
    }

    #[test]
    fn extract_project_name_before_worktrees() {
        let encoded = "-Users-raine-code-WalkingMate--worktrees-template-engine";

        // Find the worktree marker
        let wt_pos = encoded.find("--worktrees-").unwrap();

        // Extract the part before --worktrees
        let before_wt = &encoded[..wt_pos];
        assert_eq!(before_wt, "-Users-raine-code-WalkingMate");

        // When decoded with filesystem check, this should give us WalkingMate as the project name
        // For fallback, it decodes to a path ending in WalkingMate
        let decoded = decode_with_double_dash_as(before_wt, "__");
        assert_eq!(decoded, "/Users/raine/code/WalkingMate");
    }

    // === Project root and same-project matching tests ===

    #[test]
    fn encoded_project_root_strips_worktree_suffix() {
        assert_eq!(
            encoded_project_root("-Users-raine-code-project--worktrees-branch"),
            "-Users-raine-code-project"
        );
    }

    #[test]
    fn encoded_project_root_returns_full_name_without_worktree() {
        assert_eq!(
            encoded_project_root("-Users-raine-code-project"),
            "-Users-raine-code-project"
        );
    }

    #[test]
    fn is_same_project_matches_main_and_worktree() {
        let main = "-Users-raine-code-project";
        let worktree = "-Users-raine-code-project--worktrees-fix-search";
        assert!(is_same_project(main, worktree));
        assert!(is_same_project(worktree, main));
    }

    #[test]
    fn is_same_project_matches_two_worktrees() {
        let wt1 = "-Users-raine-code-project--worktrees-branch-a";
        let wt2 = "-Users-raine-code-project--worktrees-branch-b";
        assert!(is_same_project(wt1, wt2));
    }

    #[test]
    fn is_same_project_matches_identical() {
        let name = "-Users-raine-code-project";
        assert!(is_same_project(name, name));
    }

    #[test]
    fn is_same_project_rejects_different_projects() {
        let a = "-Users-raine-code-project-a";
        let b = "-Users-raine-code-project-b";
        assert!(!is_same_project(a, b));
    }

    #[test]
    fn is_same_project_hidden_worktrees() {
        // .worktrees encodes to --worktrees- (dot becomes dash)
        let main = convert_path_to_project_dir_name(Path::new("/Users/raine/code/myproject"));
        let worktree = convert_path_to_project_dir_name(Path::new(
            "/Users/raine/code/myproject/.worktrees/feature",
        ));
        assert!(is_same_project(&main, &worktree));
    }

    #[test]
    fn is_same_project_double_underscore_worktrees() {
        // __worktrees also encodes to --worktrees-
        let main = convert_path_to_project_dir_name(Path::new("/Users/raine/code/myproject"));
        let worktree = convert_path_to_project_dir_name(Path::new(
            "/Users/raine/code/myproject__worktrees/feature",
        ));
        assert!(is_same_project(&main, &worktree));
    }
}
