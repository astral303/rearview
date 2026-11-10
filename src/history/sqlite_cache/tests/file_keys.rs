//! Lookups of a cached session by a spelling of its files other than
//! discovery's: the canonical path a named file resolves to, and on a file
//! system that ignores case, a name in another case.

use crate::history::environment::test_support::{current_copy, load_rows, test_environment};
use crate::history::provider::{ParseWatch, SessionRoot, SessionStorage, WatchedStorage};
use crate::history::sqlite_cache::{find_cached_copy_without_waiting, find_cached_session_waiting};
use crate::history::{CacheBase, Conversation, InteractiveCache, Source};
use std::path::{Path, PathBuf};

#[test]
fn root_reached_through_another_spelling_finds_copy_by_canonical_paths() {
    let directory = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let (canonical, alias) = canonical_and_alias(directory.path());

    let row = cached_claude_session(&alias, base.path());

    assert!(
        row.subagents.iter().all(|path| path.starts_with(&alias)),
        "the restored sub-agents keep the root's spelling: {:?}",
        row.subagents
    );
    let canonical_of = |path: &Path| canonical.join(path.strip_prefix(&alias).unwrap());
    let subagents: Vec<PathBuf> = row
        .subagents
        .iter()
        .map(|path| canonical_of(path))
        .collect();
    let copy = find_cached_session_waiting(
        &cache_base(base.path()),
        Source::Claude,
        &canonical_of(&row.path),
        &subagents,
    )
    .unwrap();
    assert!(
        copy.is_some(),
        "the canonical paths find the copy written through the root's other spelling"
    );
}

#[test]
fn user_named_file_finds_copy_its_root_cached() {
    let directory = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let row = cached_claude_session(directory.path(), base.path());
    let environment = test_environment(Vec::new(), Some(base.path()));
    let named = environment.canonicaliser().canonicalise(&row.path).unwrap();

    let entries = current_copy(crate::history::display_log_entries_interactively(
        &InteractiveCache::at(cache_base(base.path())),
        Source::Claude,
        &named,
        &row.subagents,
    ));

    assert!(
        !entries.entries.is_empty(),
        "the viewer of `rearview <file>` reads the copy the root's load cached"
    );
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn file_named_in_another_case_finds_copy() {
    let directory = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(directory.path()).unwrap();
    let row = cached_claude_session(&canonical, base.path());
    let shouted = |path: &Path| {
        let name = path.file_name().unwrap().to_string_lossy().to_uppercase();
        path.with_file_name(name)
    };
    let subagents: Vec<PathBuf> = row.subagents.iter().map(|path| shouted(path)).collect();

    let copy = find_cached_session_waiting(
        &cache_base(base.path()),
        Source::Claude,
        &shouted(&row.path),
        &subagents,
    )
    .unwrap();
    let any_copy = find_cached_copy_without_waiting(
        &cache_base(base.path()),
        Source::Claude,
        &shouted(&row.path),
    )
    .unwrap();

    assert!(
        copy.is_some(),
        "file names in upper case find the copy and match its sub-agents"
    );
    assert!(any_copy.is_some(), "a locator in upper case finds the copy");
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
        (std::fs::canonicalize(&target).unwrap(), alias)
    }
    #[cfg(windows)]
    {
        let canonical = std::fs::canonicalize(directory).unwrap();
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

/// The row of a Claude Code session with two sub-agents, written under
/// `directory`'s `projects` root, as a second load restores it from the
/// cache under `base`.
fn cached_claude_session(directory: &Path, base: &Path) -> Conversation {
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
    let storage: &'static dyn SessionStorage = Box::leak(Box::new(WatchedStorage {
        inner: Source::Claude.provider().storage(),
        roots: vec![SessionRoot::new(root)],
        watch: ParseWatch::default(),
    }));
    load_rows(storage, base);
    let mut rows = load_rows(storage, base);
    assert_eq!(rows.len(), 1, "the load lists the session");
    let row = rows.pop().unwrap();
    assert_eq!(row.subagents.len(), 2, "the session lists both sub-agents");
    assert!(
        row.cached_session.is_some(),
        "the second load restores the session"
    );
    row
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

fn cache_base(base: &Path) -> CacheBase {
    test_environment(Vec::new(), Some(base))
        .cache_base()
        .clone()
}
