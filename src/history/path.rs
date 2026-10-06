//! Short project names from paths, and the project-folder match
//! `agent.exclude_projects` applies to a session's parent folder name.

use std::path::Path;

/// Format a path into a short display name.
///
/// For worktree paths like `/Users/raine/code/claude-history__worktrees/claude-search`,
/// returns `claude-history/claude-search` to show both the main project and worktree name.
///
/// For regular paths, returns just the folder name.
pub fn format_short_name_from_path(path: &Path) -> String {
    let path_str = path.to_string_lossy();

    // Check for worktree pattern in the path
    if let Some(wt_pos) = path_str
        .find("__worktrees/")
        .or_else(|| path_str.find("/.worktrees/"))
    {
        let is_hidden = path_str[wt_pos..].starts_with("/.");
        let separator_len = if is_hidden {
            "/.worktrees/".len()
        } else {
            "__worktrees/".len()
        };

        // Get main project (folder before __worktrees)
        let before = &path_str[..wt_pos];
        let main_project = Path::new(before)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Get worktree name (folder after __worktrees/)
        let after = &path_str[wt_pos + separator_len..];
        let worktree = after.split('/').next().unwrap_or("");

        if !main_project.is_empty() && !worktree.is_empty() {
            return format!("{}/{}", main_project, worktree);
        }
    }

    // Not a worktree, just return the folder name
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path_str.into_owned())
}

/// The encoded worktree marker that appears in Claude project directory names.
///
/// workmux creates worktrees at `<project>/.worktrees/<branch>/` (or
/// `<project>__worktrees/<branch>/` on older setups). Both encode to
/// `--worktrees-` because `.`, `_`, and `/` all become `-` in Claude's
/// encoding scheme.
const WORKTREE_MARKER: &str = "--worktrees-";

/// Extract the encoded project root from an encoded project directory name.
///
/// If the encoded name contains a worktree marker (`--worktrees-`), returns
/// everything before it (the project root portion). Otherwise returns the
/// full encoded name as-is.
///
/// # Examples
/// ```
/// # use rearview::history::path::encoded_project_root;
/// assert_eq!(
///     encoded_project_root("-Users-raine-code-project--worktrees-branch"),
///     "-Users-raine-code-project"
/// );
/// assert_eq!(
///     encoded_project_root("-Users-raine-code-project"),
///     "-Users-raine-code-project"
/// );
/// ```
pub fn encoded_project_root(encoded: &str) -> &str {
    encoded
        .split_once(WORKTREE_MARKER)
        .map_or(encoded, |(root, _)| root)
}

/// Check if two encoded project directory names belong to the same project.
///
/// Two names are considered part of the same project if they share the same
/// encoded project root (i.e., stripping any workmux `--worktrees-<branch>`
/// suffix yields the same string).
pub fn is_same_project(a: &str, b: &str) -> bool {
    encoded_project_root(a) == encoded_project_root(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    // === Worktree path structure tests ===

    #[test]
    fn extract_worktree_name_from_encoded() {
        let encoded = "-Users-raine-code-WalkingMate--worktrees-template-engine";

        // Find the worktree marker
        let wt_pos = encoded.find("--worktrees-").unwrap();

        // Extract worktree name (everything after --worktrees-)
        let worktree_name = &encoded[wt_pos + "--worktrees-".len()..];
        assert_eq!(worktree_name, "template-engine");
    }

    // === format_project_short_name tests (worktree display) ===

    #[test]
    fn format_short_name_extracts_worktree_pattern() {
        // Test the worktree pattern detection in decoded paths
        let path = "/Users/raine/code/WalkingMate__worktrees/template-engine";

        // Check for worktree pattern
        assert!(path.contains("__worktrees/"));

        // Extract main project
        let wt_pos = path.find("__worktrees/").unwrap();
        let before = &path[..wt_pos];
        let main_project = Path::new(before)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap();
        assert_eq!(main_project, "WalkingMate");

        // Extract worktree name
        let after = &path[wt_pos + "__worktrees/".len()..];
        let worktree = after.split('/').next().unwrap();
        assert_eq!(worktree, "template-engine");

        // Combined display
        let display = format!("{}/{}", main_project, worktree);
        assert_eq!(display, "WalkingMate/template-engine");
    }

    #[test]
    fn format_short_name_hidden_worktrees() {
        // Test .worktrees pattern (hidden worktrees folder)
        let path = "/Users/raine/code/workmux/.worktrees/uncommitted";

        // Check for hidden worktree pattern
        assert!(path.contains("/.worktrees/"));

        let wt_pos = path.find("/.worktrees/").unwrap();
        let before = &path[..wt_pos];
        let main_project = Path::new(before)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap();
        assert_eq!(main_project, "workmux");

        let after = &path[wt_pos + "/.worktrees/".len()..];
        let worktree = after.split('/').next().unwrap();
        assert_eq!(worktree, "uncommitted");
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
}
