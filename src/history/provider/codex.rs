//! Codex sessions, stored as dated rollout files under `~/.codex/sessions/`.

use super::{
    Deleted, DiscoveredSessions, IgnoredSessions, RefNamespaces, SessionCache, SessionLaunch,
    SessionLauncher, SessionProvider, SessionRoot, SessionStorage, SessionTitle, SourceLabels,
    walk,
};
use crate::cli::DebugLevel;
use crate::error::{AppError, Result};
use crate::history::format::codex::RolloutFileName;
use crate::history::format::{self, SessionFormat, codex};
use crate::history::{Conversation, Source, parser};
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
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

    /// An undo leaves older rollouts of the same thread on disk, and each
    /// sub-agent thread is a rollout of its own. Removing only the newest
    /// file would surface an older one as the thread on the next load;
    /// leaving a sub-agent thread would surface it as a session of its own.
    /// So delete removes every file of the thread and of its sub-agent
    /// threads, then all of their index records.
    fn delete_session(&self, path: &Path) -> Result<Deleted> {
        format::require_owned_transcript(Source::Codex, path)?;
        let Some(thread_id) =
            RolloutFileName::parse_path(path).map(|name| name.thread_id.to_owned())
        else {
            // A stray copy outside Codex's naming: the named thread and its
            // title are not what the user pointed at.
            std::fs::remove_file(path)?;
            return Ok(Deleted::just_the_session());
        };
        let Some(sessions_root) = codex::sessions_tree_of(path) else {
            // Outside a sessions tree there is nowhere to look for the
            // thread's other rollouts or its sub-agent threads.
            std::fs::remove_file(path)?;
            return Ok(Deleted::just_the_session());
        };
        let rollouts = walk::jsonl_files_at_depth(sessions_root, codex::SESSIONS_TREE_DEPTH)?;
        let subagents = codex::subagent_threads(&rollouts, &thread_id);
        let doomed = subagents
            .iter()
            .map(|thread| thread.thread_id.clone())
            .chain([thread_id.clone()])
            .collect::<HashSet<_>>();
        let mut stored_copies = remove_rollouts_of(&doomed, &rollouts, &thread_id)?;
        if path.exists() {
            // The target sat outside the dated tree the walk covers.
            std::fs::remove_file(path)?;
            stored_copies += 1;
        }
        prune_index_records(path, &doomed)?;
        Ok(Deleted {
            stored_copies,
            subagent_sessions: subagents.len(),
        })
    }

    /// A rollout's name carries the thread it records. An undo leaves older
    /// rollouts of that thread behind, and the newest is the one listed. Only
    /// a UUID is searched for: an ordinary query must not walk the tree.
    fn resolve_session_id(&self, session_id: &str) -> Result<Option<PathBuf>> {
        if !crate::search::is_uuid(session_id) {
            return Ok(None);
        }
        for root in CodexStorage.roots()? {
            if let Some(rollout) = newest_rollout_of_thread(&root.path, session_id)? {
                return Ok(Some(rollout));
            }
        }
        Ok(None)
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
            schema_version: 5,
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
    /// the only one listed. Rollouts Codex compressed are counted, not
    /// listed: nothing here decodes them, and with compression on every
    /// rollout older than a week is one, so a history that stops a week back
    /// would otherwise look complete.
    fn discover(&self, root: &SessionRoot) -> Result<DiscoveredSessions> {
        let transcripts = walk::transcripts_at_depth(&root.path, codex::SESSIONS_TREE_DEPTH)?;
        Ok(DiscoveredSessions {
            stubs: walk::file_stubs(root, codex::newest_rollouts_per_thread(&transcripts.plain)),
            ignored: vec![IgnoredSessions {
                count: transcripts.compressed_count,
                reason: COMPRESSED_SESSIONS_UNSUPPORTED,
            }],
        })
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

/// The reason shown for rollouts Codex's `local_thread_store_compression`
/// rewrote to `.jsonl.zst`. Worded in sessions: a rollout is a file, and the
/// user knows sessions.
const COMPRESSED_SESSIONS_UNSUPPORTED: &str = "compressed sessions unsupported";

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

/// The newest rollout under `sessions` that records `thread_id`.
fn newest_rollout_of_thread(sessions: &Path, thread_id: &str) -> Result<Option<PathBuf>> {
    let thread_files = walk::jsonl_files_at_depth(sessions, codex::SESSIONS_TREE_DEPTH)?
        .into_iter()
        .filter(|file| {
            RolloutFileName::parse_path(file).is_some_and(|name| name.thread_id == thread_id)
        })
        .collect::<Vec<_>>();
    Ok(codex::newest_rollouts_per_thread(&thread_files)
        .into_iter()
        .next())
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

/// Remove every rollout of a thread in `doomed`, counting the copies of
/// `thread_id` among them.
fn remove_rollouts_of(
    doomed: &HashSet<String>,
    rollouts: &[PathBuf],
    thread_id: &str,
) -> Result<usize> {
    let mut copies = 0;
    for rollout in rollouts {
        let Some(name) = RolloutFileName::parse_path(rollout) else {
            continue;
        };
        if !doomed.contains(name.thread_id) {
            continue;
        }
        std::fs::remove_file(rollout)?;
        if name.thread_id == thread_id {
            copies += 1;
        }
    }
    Ok(copies)
}

/// Drop every index record naming a thread in `thread_ids`, atomically, so a
/// deleted session's title cannot reattach to a future thread by accident.
fn prune_index_records(path: &Path, thread_ids: &HashSet<String>) -> Result<()> {
    let Some(index) = codex::session_index_path(path) else {
        return Ok(());
    };
    super::retain_index_records(&index, |record| {
        record
            .get("id")
            .and_then(Value::as_str)
            .is_none_or(|id| !thread_ids.contains(id))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::provider::RootOrigin;
    use std::ffi::OsStr as StdOsStr;

    const THREAD: &str = "019f0000-0000-7000-8000-00000000000a";
    const SUBAGENT_THREAD: &str = "019f0000-0000-7000-8000-00000000000b";
    const OTHER_THREAD: &str = "019f0000-0000-7000-8000-00000000000c";
    const NESTED_SUBAGENT_THREAD: &str = "019f0000-0000-7000-8000-00000000000d";

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex")
            .join(name)
    }

    /// The path Codex files a rollout under: the day of `stamp`, then
    /// `rollout-<stamp>-<ids>.jsonl`.
    fn rollout_path(home: &Path, stamp: &str, ids: &str) -> PathBuf {
        let day = stamp[..10].replace('-', "/");
        let directory = home.join("sessions").join(day);
        std::fs::create_dir_all(&directory).unwrap();
        directory.join(format!("rollout-{stamp}-{ids}.jsonl"))
    }

    /// Copies the rollout fixture to where Codex would file it and returns
    /// the transcript path.
    fn write_rollout(home: &Path, stamp: &str, ids: &str) -> PathBuf {
        let path = rollout_path(home, stamp, ids);
        std::fs::copy(fixture("rollout.jsonl"), &path).unwrap();
        path
    }

    /// Writes a rollout of the thread `ids` names, run as a sub-agent of
    /// `parent`, where Codex would file it, and returns the transcript path.
    fn write_subagent_rollout(home: &Path, stamp: &str, ids: &str, parent: &str) -> PathBuf {
        let path = rollout_path(home, stamp, ids);
        let thread_id = ids.split('_').next().unwrap();
        codex::test_support::write_subagent_rollout(&path, thread_id, parent);
        path
    }

    /// Writes a rollout of the thread `ids` names as Codex's compression
    /// leaves it, where Codex would file it. Its content is never decoded.
    fn write_compressed_rollout(home: &Path, stamp: &str, ids: &str) {
        let path = rollout_path(home, stamp, ids).with_extension("jsonl.zst");
        std::fs::write(path, "never decoded").unwrap();
    }

    /// Writes `session_index.jsonl` beside the sessions tree with one record
    /// per `(thread id, name)` pair.
    fn write_index(home: &Path, names: &[(&str, &str)]) {
        let records = names
            .iter()
            .map(|(id, name)| {
                json!({"id": id, "thread_name": name, "updated_at": "2026-08-19T10:00:00Z"})
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(home.join("session_index.jsonl"), format!("{records}\n")).unwrap();
    }

    fn discovered_names(home: &Path) -> Vec<String> {
        let root = SessionRoot::new(home.join("sessions"));
        CodexStorage
            .discover(&root)
            .unwrap()
            .stubs
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
        write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let reverted = format!("{THREAD}_{OTHER_THREAD}");
        write_rollout(home.path(), "2026-08-19T10-00-00", &reverted);

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
        write_rollout(home.path(), "2026-08-19T10-00-00", &older);
        write_rollout(home.path(), "2026-08-19T10-00-00", &newer);

        assert_eq!(
            discovered_names(home.path()),
            vec![format!("rollout-2026-08-19T10-00-00-{newer}.jsonl")]
        );
    }

    /// Codex's `local_thread_store_compression` rewrites rollouts older than a
    /// week to `.jsonl.zst`. Nothing here decodes them, so the list reports
    /// how many it ignored rather than showing a history that stops a week
    /// back as complete.
    #[test]
    fn compressed_rollouts_are_counted_for_the_user_and_not_listed() {
        let home = tempfile::tempdir().unwrap();
        let root = SessionRoot::new(home.path().join("sessions"));
        let ignored = |count| {
            vec![IgnoredSessions {
                count,
                reason: COMPRESSED_SESSIONS_UNSUPPORTED,
            }]
        };
        write_rollout(home.path(), "2026-08-19T10-00-00", THREAD);
        assert_eq!(CodexStorage.discover(&root).unwrap().ignored, ignored(0));

        write_compressed_rollout(home.path(), "2026-08-01T10-00-00", OTHER_THREAD);
        let discovered = CodexStorage.discover(&root).unwrap();
        assert_eq!(discovered.ignored, ignored(1));
        assert_eq!(
            discovered.stubs.len(),
            1,
            "a compressed rollout is not a session to load"
        );

        write_compressed_rollout(home.path(), "2026-08-02T10-00-00", NESTED_SUBAGENT_THREAD);
        assert_eq!(CodexStorage.discover(&root).unwrap().ignored, ignored(2));
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
        let transcript = write_rollout(home.path(), "2026-08-19T10-00-00", THREAD);

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
        let original = write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let newest = write_rollout(
            home.path(),
            "2026-08-19T10-00-00",
            &format!("{THREAD}_{OTHER_THREAD}"),
        );
        let unrelated = write_rollout(home.path(), "2026-08-19T11-00-00", OTHER_THREAD);
        std::fs::write(
            home.path().join("session_index.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"id": THREAD, "thread_name": "doomed", "updated_at": "2026-08-19T10:00:00Z"}),
                json!({"id": OTHER_THREAD, "thread_name": "kept", "updated_at": "2026-08-19T11:00:00Z"}),
            ),
        )
        .unwrap();

        let deleted = CodexProvider.delete_session(&newest).unwrap();

        assert_eq!(
            deleted,
            Deleted {
                stored_copies: 2,
                subagent_sessions: 0,
            }
        );
        assert!(!newest.exists());
        assert!(!original.exists(), "an older revert would resurface");
        assert!(unrelated.exists());
        let index = std::fs::read_to_string(home.path().join("session_index.jsonl")).unwrap();
        assert!(!index.contains("doomed"));
        assert!(index.contains("kept"));
    }

    /// A sub-agent thread is a rollout of its own. Left behind, it would list
    /// as a session the user never started, under an id they never saw.
    #[test]
    fn delete_removes_every_subagent_thread_with_the_session() {
        let home = tempfile::tempdir().unwrap();
        let parent = write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let subagent =
            write_subagent_rollout(home.path(), "2026-08-18T09-10-00", SUBAGENT_THREAD, THREAD);
        let nested = write_subagent_rollout(
            home.path(),
            "2026-08-19T10-00-00",
            NESTED_SUBAGENT_THREAD,
            SUBAGENT_THREAD,
        );
        let unrelated = write_rollout(home.path(), "2026-08-19T11-00-00", OTHER_THREAD);
        write_index(
            home.path(),
            &[
                (THREAD, "parent"),
                (SUBAGENT_THREAD, "sub-agent"),
                (NESTED_SUBAGENT_THREAD, "nested sub-agent"),
                (OTHER_THREAD, "kept"),
            ],
        );

        let deleted = CodexProvider.delete_session(&parent).unwrap();

        assert_eq!(
            deleted,
            Deleted {
                stored_copies: 1,
                subagent_sessions: 2,
            }
        );
        assert!(!parent.exists());
        assert!(!subagent.exists());
        assert!(
            !nested.exists(),
            "a sub-agent's own sub-agent is still the session's"
        );
        assert!(unrelated.exists());
        assert_eq!(
            codex::index_titles(&home.path().join("session_index.jsonl")),
            HashMap::from([(OTHER_THREAD.to_owned(), "kept".to_owned())])
        );
    }

    /// Copies count for the thread itself; a sub-agent thread counts once
    /// however many rollouts an undo left it, and every one of them is deleted.
    #[test]
    fn delete_counts_the_threads_copies_apart_from_its_subagent_threads() {
        let home = tempfile::tempdir().unwrap();
        let original = write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let reverted = write_rollout(
            home.path(),
            "2026-08-19T10-00-00",
            &format!("{THREAD}_{OTHER_THREAD}"),
        );
        let subagent =
            write_subagent_rollout(home.path(), "2026-08-18T09-10-00", SUBAGENT_THREAD, THREAD);
        let subagent_reverted = write_subagent_rollout(
            home.path(),
            "2026-08-19T10-10-00",
            &format!("{SUBAGENT_THREAD}_{NESTED_SUBAGENT_THREAD}"),
            THREAD,
        );

        let deleted = CodexProvider.delete_session(&reverted).unwrap();

        assert_eq!(
            deleted,
            Deleted {
                stored_copies: 2,
                subagent_sessions: 1,
            }
        );
        for file in [original, reverted, subagent, subagent_reverted] {
            assert!(!file.exists(), "{} survived", file.display());
        }
    }

    #[test]
    fn deleting_a_subagent_thread_removes_its_own_subagents_and_leaves_its_parent() {
        let home = tempfile::tempdir().unwrap();
        let parent = write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let subagent =
            write_subagent_rollout(home.path(), "2026-08-18T09-10-00", SUBAGENT_THREAD, THREAD);
        let nested = write_subagent_rollout(
            home.path(),
            "2026-08-19T10-00-00",
            NESTED_SUBAGENT_THREAD,
            SUBAGENT_THREAD,
        );

        let deleted = CodexProvider.delete_session(&subagent).unwrap();

        assert_eq!(
            deleted,
            Deleted {
                stored_copies: 1,
                subagent_sessions: 1,
            }
        );
        assert!(parent.exists());
        assert!(!subagent.exists());
        assert!(!nested.exists());
    }

    /// A header can name any thread as its parent, one of its own sub-agents
    /// included; the walk must not follow such a chain for ever.
    #[test]
    fn a_parent_chain_that_loops_back_on_the_thread_terminates() {
        let home = tempfile::tempdir().unwrap();
        let parent = write_subagent_rollout(
            home.path(),
            "2026-08-18T09-00-00",
            THREAD,
            NESTED_SUBAGENT_THREAD,
        );
        let subagent =
            write_subagent_rollout(home.path(), "2026-08-18T09-10-00", SUBAGENT_THREAD, THREAD);
        let nested = write_subagent_rollout(
            home.path(),
            "2026-08-19T10-00-00",
            NESTED_SUBAGENT_THREAD,
            SUBAGENT_THREAD,
        );

        let deleted = CodexProvider.delete_session(&parent).unwrap();

        assert_eq!(
            deleted,
            Deleted {
                stored_copies: 1,
                subagent_sessions: 2,
            }
        );
        for file in [parent, subagent, nested] {
            assert!(!file.exists(), "{} survived", file.display());
        }
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
        let transcript = write_rollout(home.path(), "2026-08-19T10-00-00", THREAD);
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

    /// A pasted thread id names no file on disk: the rollout holding it is
    /// named for a timestamp and the ids together.
    #[test]
    fn a_thread_id_resolves_to_its_newest_rollout() {
        let home = tempfile::tempdir().unwrap();
        write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let newest = write_rollout(home.path(), "2026-08-19T10-00-00", THREAD);

        assert_eq!(
            newest_rollout_of_thread(&home.path().join("sessions"), THREAD).unwrap(),
            Some(newest)
        );
    }

    #[test]
    fn a_thread_id_codex_never_recorded_resolves_to_nothing() {
        let home = tempfile::tempdir().unwrap();
        write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);

        assert_eq!(
            newest_rollout_of_thread(&home.path().join("sessions"), OTHER_THREAD).unwrap(),
            None
        );
    }

    #[test]
    fn a_query_that_is_not_a_uuid_resolves_without_walking_the_tree() {
        assert_eq!(CodexProvider.resolve_session_id("rollout").unwrap(), None);
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
