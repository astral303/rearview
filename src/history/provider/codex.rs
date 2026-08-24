//! Codex sessions, stored as dated rollout files under `~/.codex/sessions/`.

use super::{
    RefNamespaces, SessionCache, SessionLaunch, SessionLauncher, SessionProvider, SessionRoot,
    SessionStorage, SessionStub, SessionTitle, SourceLabels, walk,
};
use crate::cli::DebugLevel;
use crate::error::{AppError, Result};
use crate::history::format::codex::RolloutFileName;
use crate::history::format::{self, SessionFormat, codex};
use crate::history::{Conversation, Source, parser};
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct CodexProvider;

impl SessionProvider for CodexProvider {
    fn source(&self) -> Source {
        Source::Codex
    }

    fn labels(&self) -> SourceLabels {
        SourceLabels {
            name: "codex",
            list: "CDX",
            display: "Codex",
        }
    }

    fn ref_namespaces(&self) -> RefNamespaces {
        RefNamespaces {
            conversation: Some("agent-codex-v1"),
            project: "agent-codex-project-v1",
        }
    }

    fn storage(&self) -> Option<&dyn SessionStorage> {
        Some(&CodexStorage)
    }

    fn format(&self) -> Option<&dyn SessionFormat> {
        Some(&codex::CODEX_ROLLOUT)
    }

    fn launcher(&self) -> &dyn SessionLauncher {
        &CodexLauncher
    }

    /// A Codex title lives in the session index, not the transcript, so a
    /// rename appends an index record and leaves the rollout untouched.
    fn rename_session(&self, path: &Path, title: &str) -> Result<()> {
        format::require_owned_transcript(Source::Codex, path)?;
        append_index_record(path, &thread_id_of(path)?, title)
    }

    /// An undo leaves older rollouts of the same thread on disk. Removing only
    /// the newest would surface one of them as the thread on the next load, so
    /// delete takes every file the thread owns, then its index records.
    fn delete_session(&self, path: &Path) -> Result<()> {
        format::require_owned_transcript(Source::Codex, path)?;
        let Some(thread_id) =
            RolloutFileName::parse_path(path).map(|name| name.thread_id.to_owned())
        else {
            // A stray copy outside Codex's naming: the named thread and its
            // title are not what the user pointed at.
            std::fs::remove_file(path)?;
            return Ok(());
        };
        for file in thread_rollout_files(path, &thread_id)? {
            std::fs::remove_file(file)?;
        }
        if path.exists() {
            // The target sat outside the dated tree the walk covers.
            std::fs::remove_file(path)?;
        }
        prune_index_records(path, &thread_id)
    }
}

struct CodexStorage;

impl SessionStorage for CodexStorage {
    fn source(&self) -> Source {
        Source::Codex
    }

    fn cache(&self) -> SessionCache {
        SessionCache {
            directory: "codex",
            magic: *b"CXHIST01",
            schema_version: 4,
        }
    }

    fn roots(&self) -> Result<Vec<SessionRoot>> {
        let home = home::home_dir().ok_or_else(|| {
            AppError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Could not determine home directory",
            ))
        })?;
        Ok(vec![sessions_root_from(
            std::env::var("CODEX_HOME").ok().as_deref(),
            &home,
        )])
    }

    /// Only well-named rollouts are transcripts, and an undo leaves several
    /// files per thread; the newest is the one Codex itself resumes, so it is
    /// the only one listed.
    fn discover(&self, root: &SessionRoot) -> Result<Vec<SessionStub>> {
        let rollouts = walk::jsonl_files_at_depth(&root.path, codex::SESSIONS_TREE_DEPTH)?;
        Ok(walk::file_stubs(
            root,
            codex::newest_rollouts_per_thread(rollouts),
        ))
    }

    fn parse_session(
        &self,
        path: PathBuf,
        _root: &SessionRoot,
        modified: Option<SystemTime>,
        debug_level: Option<DebugLevel>,
    ) -> Result<Option<Conversation>> {
        parser::process_session_file(path, &codex::CODEX_ROLLOUT, modified, debug_level)
    }

    /// Every rollout is parsed in full, however large: the biggest sessions
    /// are the most valuable to search, and skipping one would delist it.
    fn max_session_bytes(&self) -> Option<u64> {
        None
    }

    /// Thread names live in `session_index.jsonl`, so a rename never touches
    /// the rollout the cache validates against.
    fn external_titles(&self, root: &SessionRoot) -> HashMap<String, SessionTitle> {
        let Some(index) = codex::index_beside_sessions_tree(&root.path) else {
            return HashMap::new();
        };
        codex::index_titles(&index)
            .into_iter()
            .map(|(id, name)| (id, SessionTitle::Custom(name)))
            .collect()
    }
}

fn sessions_root_from(codex_home: Option<&str>, home: &Path) -> SessionRoot {
    let base = codex_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    SessionRoot::new(base.join("sessions")).in_agent_tree()
}

/// Codex resumes and forks by thread id, not by path. The id is in the
/// filename, which Codex itself treats as authoritative when resolving a
/// thread to a file.
struct CodexLauncher;

impl SessionLauncher for CodexLauncher {
    fn resume_command(&self, launch: &SessionLaunch) -> Result<std::process::Command> {
        let mut command = std::process::Command::new("codex");
        command.arg("resume").arg(thread_id_of(launch.path)?);
        if let Some(project_path) = launch.project_path {
            command.current_dir(project_path);
        }
        Ok(command)
    }

    /// A fork runs where the user is now, not where the original session ran:
    /// the branch belongs to the current project.
    fn fork_command(&self, launch: &SessionLaunch) -> Result<std::process::Command> {
        let mut command = std::process::Command::new("codex");
        command.arg("fork").arg(thread_id_of(launch.path)?);
        Ok(command)
    }
}

fn thread_id_of(path: &Path) -> Result<String> {
    RolloutFileName::parse_path(path)
        .map(|name| name.thread_id.to_owned())
        .ok_or_else(|| {
            AppError::ConfigError(format!(
                "{} is not named as a Codex rollout",
                path.display()
            ))
        })
}

/// Every rollout file of `thread_id` in the sessions tree holding `path`, or
/// just `path` when no `sessions` ancestor scopes the walk.
fn thread_rollout_files(path: &Path, thread_id: &str) -> Result<Vec<PathBuf>> {
    let Some(sessions_root) = codex::sessions_tree_of(path) else {
        return Ok(vec![path.to_path_buf()]);
    };
    Ok(
        walk::jsonl_files_at_depth(sessions_root, codex::SESSIONS_TREE_DEPTH)?
            .into_iter()
            .filter(|file| {
                RolloutFileName::parse_path(file).is_some_and(|name| name.thread_id == thread_id)
            })
            .collect(),
    )
}

/// Append `{id, thread_name, updated_at}` to the session index; the newest
/// record for an id is the one Codex and the title lookup read.
fn append_index_record(path: &Path, thread_id: &str, title: &str) -> Result<()> {
    let index = codex::session_index_path(path).ok_or_else(|| {
        AppError::ConfigError(format!(
            "{} is not inside a Codex sessions tree",
            path.display()
        ))
    })?;
    let record = json!({
        "id": thread_id,
        "thread_name": title.replace(['\r', '\n'], " ").trim(),
        "updated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(index)?;
    writeln!(file, "{record}")?;
    Ok(())
}

/// Drop every index record naming `thread_id`, atomically, so a deleted
/// session's title cannot reattach to a future thread by accident.
fn prune_index_records(path: &Path, thread_id: &str) -> Result<()> {
    let Some(index) = codex::session_index_path(path) else {
        return Ok(());
    };
    super::retain_index_records(&index, |record| {
        record.get("id").and_then(Value::as_str) != Some(thread_id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::provider::RootOrigin;
    use std::ffi::OsStr as StdOsStr;

    const THREAD: &str = "019f0000-0000-7000-8000-00000000000a";
    const OTHER_THREAD: &str = "019f0000-0000-7000-8000-00000000000c";

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex")
            .join(name)
    }

    /// Copies the rollout fixture to `sessions/<day>/rollout-<stamp>-<ids>.jsonl`
    /// under `home` and returns the transcript path.
    fn write_rollout(home: &Path, day: &str, stamp: &str, ids: &str) -> PathBuf {
        let directory = home.join("sessions").join(day);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("rollout-{stamp}-{ids}.jsonl"));
        std::fs::copy(fixture("rollout.jsonl"), &path).unwrap();
        path
    }

    fn discovered_names(home: &Path) -> Vec<String> {
        let root = SessionRoot::new(home.join("sessions"));
        CodexStorage
            .discover(&root)
            .unwrap()
            .into_iter()
            .map(|stub| {
                stub.locator
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn the_sessions_root_is_codex_home_or_its_override() {
        let home = Path::new("/home/user");
        let default = sessions_root_from(None, home);
        assert_eq!(default.path, home.join(".codex/sessions"));
        assert_eq!(
            default.origin(),
            RootOrigin::AgentTree,
            "CODEX_HOME moves the whole home, so the root is always Codex's own tree"
        );
        assert_eq!(
            sessions_root_from(Some("/opt/codex"), home).path,
            Path::new("/opt/codex").join("sessions")
        );
        assert_eq!(
            sessions_root_from(Some(""), home).path,
            home.join(".codex/sessions"),
            "an empty override means unset"
        );
    }

    /// Codex resumes a reverted thread into its newest file; listing a
    /// superseded one would offer a row whose resume opens different content.
    #[test]
    fn a_reverted_thread_is_discovered_once_as_its_newest_rollout() {
        let home = tempfile::tempdir().unwrap();
        write_rollout(home.path(), "2026/08/18", "2026-08-18T09-00-00", THREAD);
        let reverted = format!("{THREAD}_{OTHER_THREAD}");
        write_rollout(home.path(), "2026/08/19", "2026-08-19T10-00-00", &reverted);

        assert_eq!(
            discovered_names(home.path()),
            vec![format!("rollout-2026-08-19T10-00-00-{reverted}.jsonl")]
        );
    }

    /// Rollout names carry seconds only; the UUIDv7 rollout id breaks the tie,
    /// the same way Codex picks the file it resumes.
    #[test]
    fn rollouts_in_the_same_second_resolve_by_rollout_id() {
        let home = tempfile::tempdir().unwrap();
        let older = format!("{THREAD}_019f0000-0000-7000-8000-000000000001");
        let newer = format!("{THREAD}_019f0000-0000-7000-8000-000000000002");
        write_rollout(home.path(), "2026/08/19", "2026-08-19T10-00-00", &older);
        write_rollout(home.path(), "2026/08/19", "2026-08-19T10-00-00", &newer);

        assert_eq!(
            discovered_names(home.path()),
            vec![format!("rollout-2026-08-19T10-00-00-{newer}.jsonl")]
        );
    }

    #[test]
    fn files_not_named_as_rollouts_are_not_codex_transcripts() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join("sessions/2026/08/19");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::copy(fixture("rollout.jsonl"), day.join("backup.jsonl")).unwrap();
        std::fs::copy(
            fixture("rollout.jsonl"),
            day.join("rollout-2026-08-19T10-00-00-not-a-uuid.jsonl"),
        )
        .unwrap();

        assert!(discovered_names(home.path()).is_empty());
    }

    #[test]
    fn resume_and_fork_take_the_thread_id_from_the_filename() {
        let path = PathBuf::from(format!(
            "/codex/sessions/2026/08/19/rollout-2026-08-19T10-00-00-{THREAD}.jsonl"
        ));
        let project = PathBuf::from("/tmp/project");
        let launch = SessionLaunch {
            path: &path,
            project_path: Some(&project),
            configured_args: &["--claude-only".to_owned()],
        };
        let launcher = Source::Codex.provider().launcher();

        let resume = launcher.resume_command(&launch).unwrap();
        assert_eq!(resume.get_program(), StdOsStr::new("codex"));
        assert_eq!(
            resume.get_args().collect::<Vec<_>>(),
            vec![StdOsStr::new("resume"), StdOsStr::new(THREAD)],
            "codex resumes by thread id and must not inherit Claude's configured arguments"
        );
        assert_eq!(resume.get_current_dir(), Some(project.as_path()));

        let fork = launcher.fork_command(&launch).unwrap();
        assert_eq!(
            fork.get_args().collect::<Vec<_>>(),
            vec![StdOsStr::new("fork"), StdOsStr::new(THREAD)]
        );
        assert_eq!(
            fork.get_current_dir(),
            None,
            "a fork runs in the current directory"
        );

        let stray = PathBuf::from("/elsewhere/copy.jsonl");
        assert!(
            launcher
                .resume_command(&SessionLaunch {
                    path: &stray,
                    project_path: None,
                    configured_args: &[],
                })
                .is_err(),
            "a file without a rollout name has no thread id to resume"
        );
    }

    #[test]
    fn rename_appends_an_index_record_the_title_lookup_reads() {
        let home = tempfile::tempdir().unwrap();
        let transcript = write_rollout(home.path(), "2026/08/19", "2026-08-19T10-00-00", THREAD);

        CodexProvider
            .rename_session(&transcript, "renamed\nCodex")
            .unwrap();

        let index = std::fs::read_to_string(home.path().join("session_index.jsonl")).unwrap();
        let record: Value = serde_json::from_str(index.lines().last().unwrap()).unwrap();
        assert_eq!(record["id"], THREAD);
        assert_eq!(record["thread_name"], "renamed Codex");
        let reparsed = codex::CODEX_ROLLOUT
            .parse_transcript(&transcript)
            .unwrap()
            .unwrap();
        assert_eq!(reparsed.title.as_deref(), Some("renamed Codex"));
    }

    /// An undo leaves older rollouts of the thread on disk; removing only the
    /// newest would surface one of them as the thread on the next load.
    #[test]
    fn delete_removes_every_rollout_of_the_thread_and_its_index_records() {
        let home = tempfile::tempdir().unwrap();
        let original = write_rollout(home.path(), "2026/08/18", "2026-08-18T09-00-00", THREAD);
        let newest = write_rollout(
            home.path(),
            "2026/08/19",
            "2026-08-19T10-00-00",
            &format!("{THREAD}_{OTHER_THREAD}"),
        );
        let unrelated = write_rollout(
            home.path(),
            "2026/08/19",
            "2026-08-19T11-00-00",
            OTHER_THREAD,
        );
        std::fs::write(
            home.path().join("session_index.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"id": THREAD, "thread_name": "doomed", "updated_at": "2026-08-19T10:00:00Z"}),
                json!({"id": OTHER_THREAD, "thread_name": "kept", "updated_at": "2026-08-19T11:00:00Z"}),
            ),
        )
        .unwrap();

        CodexProvider.delete_session(&newest).unwrap();

        assert!(!newest.exists());
        assert!(!original.exists(), "an older revert would resurface");
        assert!(unrelated.exists());
        let index = std::fs::read_to_string(home.path().join("session_index.jsonl")).unwrap();
        assert!(!index.contains("doomed"));
        assert!(index.contains("kept"));
    }

    /// A rename rewrites only the session index, never the rollout, so the
    /// cache's size-and-mtime check cannot see it. Without the title overlay a
    /// warm load would restore the old name from the cache indefinitely.
    #[test]
    fn a_rename_reaches_the_next_load_through_a_warm_cache() {
        use crate::history::cache::SessionCacheStore;
        use crate::history::provider::load_sessions_with_cache;

        let home = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        let transcript = write_rollout(home.path(), "2026/08/19", "2026-08-19T10-00-00", THREAD);
        CodexProvider
            .rename_session(&transcript, "old name")
            .unwrap();

        let storage = crate::history::provider::storage::RootedStorage {
            inner: CodexStorage,
            root: SessionRoot::new(home.path().join("sessions")).in_agent_tree(),
        };
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let first = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert_eq!(first[0].custom_title.as_deref(), Some("old name"));

        CodexProvider
            .rename_session(&transcript, "fresh name")
            .unwrap();

        let second = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert_eq!(second[0].custom_title.as_deref(), Some("fresh name"));
    }

    #[test]
    fn a_file_codex_does_not_own_survives_delete() {
        let directory = tempfile::tempdir().unwrap();
        let pi = directory.path().join("session.jsonl");
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl"),
            &pi,
        )
        .unwrap();

        assert!(CodexProvider.delete_session(&pi).is_err());
        assert!(pi.exists());
    }
}
