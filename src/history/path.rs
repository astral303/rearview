//! Short project names from paths, and the project names
//! `tui.exclude_projects` and `agent.exclude_projects` hide.

use super::Conversation;
use std::collections::HashSet;
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

/// The project names `tui.exclude_projects` and `agent.exclude_projects` hide.
/// A name matches the list's project name exactly, and a parent such as
/// `repo` also hides its worktree rows, such as `repo/feature`.
#[derive(Clone, Debug, Default)]
pub struct ExcludedProjects(HashSet<String>);

impl ExcludedProjects {
    /// True when `conversation`'s project name matches. A row without a
    /// project name is never excluded.
    pub fn excludes(&self, conversation: &Conversation) -> bool {
        conversation
            .project_name
            .as_deref()
            .is_some_and(|project_name| self.excludes_name(project_name))
    }

    fn excludes_name(&self, project_name: &str) -> bool {
        self.0.contains(project_name)
            || project_name
                .split_once('/')
                .is_some_and(|(parent, _)| self.0.contains(parent))
    }
}

impl FromIterator<String> for ExcludedProjects {
    fn from_iter<I: IntoIterator<Item = String>>(names: I) -> Self {
        Self(names.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // === Excluded project names ===

    fn row_in_project(project_name: Option<&str>) -> Conversation {
        crate::search::test_fixtures::one_message_conversation(
            "hello",
            chrono::Local::now(),
            None,
            None,
            project_name,
        )
    }

    fn excluded(names: &[&str]) -> ExcludedProjects {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn an_excluded_name_matches_its_project_exactly() {
        let excluded = excluded(&["repo"]);

        assert!(excluded.excludes(&row_in_project(Some("repo"))));
        assert!(!excluded.excludes(&row_in_project(Some("Repo"))));
        assert!(!excluded.excludes(&row_in_project(Some("repo-two"))));
    }

    #[test]
    fn an_excluded_parent_hides_its_worktree_rows() {
        let excluded = excluded(&["repo"]);

        assert!(excluded.excludes(&row_in_project(Some("repo/feature"))));
        assert!(!excluded.excludes(&row_in_project(Some("other/repo"))));
    }

    #[test]
    fn a_row_without_a_project_name_is_not_excluded() {
        assert!(!excluded(&["repo"]).excludes(&row_in_project(None)));
    }
}
