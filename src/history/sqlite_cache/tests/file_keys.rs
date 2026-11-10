//! A cached session named by a path other than the one the load that wrote
//! it built: the canonical path and, on a file system that ignores case, a
//! name in another case print the copy the root's load cached.

use crate::history::environment::test_support::{
    load_rows_in, test_environment, waiting_operation,
};
use crate::history::provider::walk::SystemCanonicaliser;
use crate::history::provider::walk::test_support::CountingCanonicaliser;
use crate::history::provider::{ParseWatch, SessionRoot, SessionStorage, WatchedStorage};
use crate::history::{Environment, Source};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[test]
fn canonical_path_prints_copy_cached_through_another_spelling_of_root() {
    let directory = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let (canonical, alias) = canonical_and_alias(directory.path());
    let (environment, row_path) = cached_claude_session(&alias, base.path());

    let named = canonical.join(row_path.strip_prefix(&alias).unwrap());

    assert!(
        !printed(&environment, &named).is_empty(),
        "the canonical path prints the copy written through the root's other spelling"
    );
}

/// A targeted read finds a listed session among discovery's files by the
/// named path below the root: a path a load built canonicalises only the
/// root, as each discovery does, and a path named through `..`
/// canonicalises its directory once besides.
#[test]
fn targeted_read_finds_named_session_by_its_path_below_root() {
    let directory = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let storage = claude_session_with_two_subagents(directory.path());
    let canonicaliser = Arc::new(CountingCanonicaliser::default());
    let environment = test_environment(vec![storage], Some(base.path()))
        .with_canonicaliser(canonicaliser.clone());
    let row = load_rows_in(&environment, storage).pop().unwrap();
    let root = directory.path().join("projects");
    let through_parent = root
        .join("project")
        .join("..")
        .join("project")
        .join("session.jsonl");

    for (named, expected) in [
        (row.path.clone(), vec![root.clone()]),
        (
            through_parent.clone(),
            vec![through_parent.parent().unwrap().to_path_buf(), root.clone()],
        ),
    ] {
        let before = canonicaliser.canonicalised().len();
        let outcome = crate::history::provider::cache_sessions(
            &environment,
            Source::Claude,
            &[named.as_path()],
            false,
            &waiting_operation(&environment),
            crate::history::load_meter::AgentMeter::UNMETERED,
        )
        .unwrap()
        .pop()
        .unwrap();

        assert!(outcome.is_listed(), "{} reads as listed", named.display());
        let mut since = canonicaliser.canonicalised()[before..].to_vec();
        since.sort();
        let mut expected = expected;
        expected.sort();
        assert_eq!(
            since,
            expected,
            "the read of {} canonicalises each directory once",
            named.display()
        );
    }
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn file_named_in_another_case_prints_its_copy() {
    let directory = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let canonical = SystemCanonicaliser::shared()
        .canonicalise(directory.path())
        .unwrap();
    let (environment, row_path) = cached_claude_session(&canonical, base.path());
    let name = row_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_uppercase();

    assert!(
        !printed(&environment, &row_path.with_file_name(name)).is_empty(),
        "a locator in upper case prints the copy"
    );
}

/// The canonical path of a directory in `directory`, and a spelling of it
/// that is not canonical: through a symlink on Unix, and on Windows without
/// the verbatim `\\?\` prefix.
fn canonical_and_alias(directory: &Path) -> (PathBuf, PathBuf) {
    #[cfg(unix)]
    {
        let target = directory.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let alias = directory.join("alias");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        (
            SystemCanonicaliser::shared().canonicalise(&target).unwrap(),
            alias,
        )
    }
    #[cfg(windows)]
    {
        let canonical = SystemCanonicaliser::shared()
            .canonicalise(directory)
            .unwrap();
        let alias = PathBuf::from(
            canonical
                .to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_owned(),
        );
        assert_ne!(canonical, alias, "the canonical path is verbatim");
        (canonical, alias)
    }
}

/// The environment of a Claude Code session with two sub-agents, written
/// under `directory`'s `projects` root and cached under `base` by a load,
/// with the path of the session's row.
fn cached_claude_session(directory: &Path, base: &Path) -> (Environment, PathBuf) {
    let storage = claude_session_with_two_subagents(directory);
    let environment = test_environment(vec![storage], Some(base));
    let mut rows = load_rows_in(&environment, storage);
    assert_eq!(rows.len(), 1, "the load lists the session");
    let row = rows.pop().unwrap();
    assert_eq!(row.subagents.len(), 2, "the session lists both sub-agents");
    (environment, row.path)
}

/// The entries the terminal printout reads of the session at `named`.
fn printed(environment: &Environment, named: &Path) -> Vec<crate::log_entry::LogEntry> {
    crate::history::printed_log_entries(
        environment,
        Source::Claude,
        named,
        false,
        &mut std::io::sink(),
    )
    .unwrap()
    .entries
}

/// The storage of a Claude Code session with two sub-agents, written under
/// `directory`'s `projects` root.
fn claude_session_with_two_subagents(directory: &Path) -> &'static dyn SessionStorage {
    let root = directory.join("projects");
    let transcript = root.join("project").join("session.jsonl");
    write_user_message(&transcript, "a question");
    for agent in ["a", "b"] {
        let subagent = transcript
            .with_extension("")
            .join("subagents")
            .join(format!("agent-{agent}.jsonl"));
        write_user_message(&subagent, "a task");
    }
    Box::leak(Box::new(WatchedStorage {
        inner: Source::Claude.provider().storage(),
        roots: vec![SessionRoot::new(root)],
        watch: ParseWatch::default(),
    }))
}

fn write_user_message(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let record = serde_json::json!({
        "type": "user",
        "timestamp": "2026-10-01T12:00:00Z",
        "message": {"role": "user", "content": text},
    });
    std::fs::write(path, format!("{record}\n")).unwrap();
}
