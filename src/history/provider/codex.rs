//! Codex sessions, stored as dated rollout files under `~/.codex/sessions/`
//! and listed in `~/.codex/state_5.sqlite`.

use super::sqlite::{self, SESSION_DATABASE_CANNOT_BE_READ, SchemaPin};
use super::subagents::SubagentForest;
use super::walk::{SessionFiles, Transcripts};
use super::{
    Deleted, DiscoveredSessions, IgnoredSessions, RefNamespaces, ResolvedSession, SessionCache,
    SessionLaunch, SessionLauncher, SessionProvider, SessionRoot, SessionStorage, SessionStub,
    SessionTitle, SourceLabels, walk,
};
use crate::cli::DebugLevel;
use crate::error::{AppError, Result};
use crate::history::format::codex::{RolloutFileName, ThreadKind};
use crate::history::format::{self, SessionFormat, codex};
use crate::history::{Conversation, Source, parser};
use chrono::{SecondsFormat, Utc};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::Duration;

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
    /// So delete removes every file of the thread and of the threads beneath
    /// it — the reviews Codex ran on it included — then all of their index
    /// records. Only the sessions tree names the rollouts an undo
    /// superseded, so delete walks it whether or not the database is present.
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
        let transcripts = walk::transcripts_at_depth(sessions_root, codex::SESSIONS_TREE_DEPTH)?;
        let subagents = CodexThreadIndex::under_tree(
            sessions_root,
            Some(&transcripts),
            sqlite::DEFAULT_BUSY_TIMEOUT,
        )?
        .threads_beneath(&thread_id);
        let doomed = subagents
            .iter()
            .cloned()
            .chain([thread_id.clone()])
            .collect::<HashSet<_>>();
        let mut stored_copies = remove_rollouts_of(&doomed, &transcripts.plain, &thread_id)?;
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
    fn resolve_session_id(&self, session_id: &str) -> Result<Option<ResolvedSession>> {
        if !crate::search::is_uuid(session_id) {
            return Ok(None);
        }
        for root in CodexStorage.roots()? {
            let index = CodexThreadIndex::under(&root)?;
            if let Some(stub) = index.stub_of(&root, session_id) {
                return Ok(Some(ResolvedSession { root, stub }));
            }
        }
        Ok(None)
    }
}

/// Every thread under a sessions root: which threads are sessions, which are
/// sub-agents of which, and which Codex ran for itself. Read from Codex's
/// state database when it is present, else from the header of each thread's
/// newest rollout; both classify by the same `source` JSON, so they answer
/// alike.
pub(crate) struct CodexThreadIndex {
    /// The rollout of every thread, skipped ones included.
    rollout_of: HashMap<String, PathBuf>,
    /// Every thread and its parent, skipped ones included. A skipped
    /// `threads` row records no parent; [`Self::threads_beneath`] reads it.
    parent_of: Vec<(String, Option<String>)>,
    /// The threads Codex ran for itself.
    skipped: BTreeSet<String>,
    /// The threads that list, with skipped threads and everything beneath
    /// them removed: what discovery and session-id lookup answer from.
    listed: SubagentForest<String>,
    /// Rollouts Codex compressed to `.jsonl.zst`: counted for the user,
    /// never read.
    compressed_count: usize,
}

/// One thread as a `threads` row or a rollout header records it.
struct IndexedThread {
    thread_id: String,
    rollout: PathBuf,
    kind: ThreadKind,
}

impl CodexThreadIndex {
    /// Reads the database when its file exists and fails when it cannot be
    /// used; reads the rollout headers only when the file is absent, so the
    /// list cannot differ between launches.
    pub(crate) fn under(root: &SessionRoot) -> Result<Self> {
        Self::under_tree(&root.path, None, sqlite::DEFAULT_BUSY_TIMEOUT)
    }

    /// [`Self::under`] for the tree at `sessions_tree`, waiting
    /// `busy_timeout` for a lock on the database. `walked` is the tree's
    /// transcripts when the caller has already walked it, so a read of the
    /// headers does not walk it a second time.
    fn under_tree(
        sessions_tree: &Path,
        walked: Option<&Transcripts>,
        busy_timeout: Duration,
    ) -> Result<Self> {
        let database =
            state_database_beside_sessions_tree(sessions_tree).filter(|database| database.exists());
        if let Some(database) = database {
            return Self::from_state_database(&database, sessions_tree, busy_timeout);
        }
        Ok(match walked {
            Some(transcripts) => Self::from_rollout_headers(transcripts),
            None => Self::from_rollout_headers(&walk::transcripts_at_depth(
                sessions_tree,
                codex::SESSIONS_TREE_DEPTH,
            )?),
        })
    }

    /// One first-line read per rollout. Only well-named rollouts are
    /// threads, and an undo leaves several files per thread; the newest is
    /// the one Codex itself resumes, so it is the only one read. A rollout
    /// whose header cannot be read still indexes as a session with no
    /// parent, so the load records whatever it holds against its fingerprint
    /// rather than reading it again on every run.
    fn from_rollout_headers(transcripts: &Transcripts) -> Self {
        let threads = codex::newest_rollouts_per_thread(&transcripts.plain)
            .into_iter()
            .filter_map(|rollout| {
                let thread_id = RolloutFileName::parse_path(&rollout)?.thread_id.to_owned();
                let kind = codex::thread_kind_of(&rollout).unwrap_or(ThreadKind::Session);
                Some(IndexedThread {
                    thread_id,
                    rollout,
                    kind,
                })
            })
            .collect();
        Self::from_threads(threads, transcripts.compressed_count)
    }

    /// Two queries of Codex's own record, opening no rollout. An edge in
    /// `thread_spawn_edges` decides over `source`: one corpus row reads
    /// `unknown` with an edge. A skipped row's parent is in no column, so
    /// delete reads it from the header. A database none of whose rows names
    /// a rollout under `sessions_tree`, plain or compressed, describes
    /// another tree, so it is unusable.
    fn from_state_database(
        database: &Path,
        sessions_tree: &Path,
        busy_timeout: Duration,
    ) -> Result<Self> {
        let connection = sqlite::open_session_list(database, busy_timeout)?;
        let cannot_be_read = |error: rusqlite::Error| {
            sqlite::unusable_database(database, SESSION_DATABASE_CANNOT_BE_READ, &error)
        };
        let parent_of_subagent = parent_of_subagent(&connection).map_err(cannot_be_read)?;
        let rows = thread_rows(&connection).map_err(cannot_be_read)?;
        let row_count = rows.len();
        let mut threads = Vec::new();
        let mut compressed_count = 0;
        for row in rows {
            let Some(rollout) = rerooted_rollout(sessions_tree, &row.rollout_path) else {
                continue;
            };
            if rollout.is_file() {
                let kind = thread_kind_of_row(&row, &parent_of_subagent);
                threads.push(IndexedThread {
                    thread_id: row.id,
                    rollout,
                    kind,
                });
            } else if rollout.with_extension("jsonl.zst").is_file() {
                compressed_count += 1;
            }
        }
        if row_count > 0 && threads.is_empty() && compressed_count == 0 {
            return Err(AppError::SessionListUnreadable {
                reason: SESSION_DATABASE_NAMES_NO_SESSION_FILE,
                detail: format!(
                    "{}: no threads row names a rollout under {}",
                    database.display(),
                    sessions_tree.display()
                ),
            });
        }
        Ok(Self::from_threads(threads, compressed_count))
    }

    fn from_threads(threads: Vec<IndexedThread>, compressed_count: usize) -> Self {
        let mut rollout_of = HashMap::new();
        let mut parent_of = Vec::new();
        let mut skipped = BTreeSet::new();
        for thread in threads {
            if thread.kind.is_skipped() {
                skipped.insert(thread.thread_id.clone());
            }
            let parent = thread.kind.parent_thread_id().map(str::to_owned);
            parent_of.push((thread.thread_id.clone(), parent));
            rollout_of.insert(thread.thread_id, thread.rollout);
        }
        let listed =
            SubagentForest::new(parent_of.iter().cloned()).without(skipped.iter().cloned());
        Self {
            rollout_of,
            parent_of,
            skipped,
            listed,
            compressed_count,
        }
    }

    /// The sessions under `root` as discovery reports them.
    fn discovered(&self, root: &SessionRoot) -> DiscoveredSessions {
        let sessions = self
            .listed
            .sessions()
            .into_iter()
            .map(|(thread_id, subagents)| self.session_files(&thread_id, &subagents))
            .collect();
        DiscoveredSessions {
            stubs: walk::session_stubs(root, sessions),
            ignored: vec![IgnoredSessions {
                count: self.compressed_count,
                reason: COMPRESSED_SESSIONS_UNSUPPORTED,
            }],
            skipped: self.parent_of.len() - self.listed.len(),
        }
    }

    /// The stub `thread_id` would list under, or `None` for a thread the
    /// index does not list. A sub-agent thread answers with a stub of its
    /// own carrying the threads beneath it.
    fn stub_of(&self, root: &SessionRoot, thread_id: &str) -> Option<SessionStub> {
        let thread_id = thread_id.to_owned();
        if !self.listed.contains(&thread_id) {
            return None;
        }
        let subagents = self.listed.subagents_of(&thread_id);
        let files = self.session_files(&thread_id, &subagents);
        walk::session_stubs(root, vec![files]).into_iter().next()
    }

    /// Every thread beneath `thread_id`, nested ones and the threads Codex
    /// ran for itself included: what a delete removes with it. A skipped
    /// `threads` row records no parent, so the rollout header of each
    /// skipped thread indexed without one is read here, by delete alone.
    fn threads_beneath(&self, thread_id: &str) -> Vec<String> {
        let every_thread = SubagentForest::new(self.parent_of.iter().map(|(id, parent)| {
            let parent = match parent {
                None if self.skipped.contains(id) => codex::thread_kind_of(&self.rollout_of[id])
                    .and_then(|header| header.parent_thread_id().map(str::to_owned)),
                parent => parent.clone(),
            };
            (id.clone(), parent)
        }));
        every_thread.subagents_of(&thread_id.to_owned())
    }

    /// `thread_id` and `subagents` come from one of the forests, and every
    /// thread indexed there was indexed with its rollout.
    fn session_files(&self, thread_id: &str, subagents: &[String]) -> SessionFiles {
        SessionFiles {
            transcript: self.rollout_of[thread_id].clone(),
            subagents: subagents
                .iter()
                .map(|id| self.rollout_of[id.as_str()].clone())
                .collect(),
        }
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
            schema_version: 7,
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

    /// A session is a thread that runs under no other, or under one with no
    /// rollout; the threads beneath it are its sub-agent transcripts, as
    /// [`CodexThreadIndex`] reads them. Rollouts Codex compressed are
    /// counted, not listed: nothing here decodes them, and with compression
    /// on every rollout older than a week is one, so a history that stops a
    /// week back would otherwise look complete.
    fn discover(&self, root: &SessionRoot) -> Result<DiscoveredSessions> {
        Ok(CodexThreadIndex::under(root)?.discovered(root))
    }

    fn parse_session(
        &self,
        stub: &SessionStub,
        root: &SessionRoot,
        debug_level: Option<DebugLevel>,
    ) -> Result<Option<Conversation>> {
        if let Some(database) = state_database_beside_sessions_tree(&root.path) {
            SCHEMA_PIN.warn_when_schema_outruns_reader(&database, debug_level);
        }
        parser::process_session_file(stub, &codex::CODEX_ROLLOUT, debug_level)
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

const STATE_DATABASE_FILENAME: &str = "state_5.sqlite";

fn state_database_beside_sessions_tree(sessions_tree: &Path) -> Option<PathBuf> {
    sessions_tree
        .parent()
        .map(|home| home.join(STATE_DATABASE_FILENAME))
}

/// The phrase the list shows, after the shared ones in [`sqlite`], for a
/// database that describes another sessions tree, followed by `: sessions
/// not loaded`.
const SESSION_DATABASE_NAMES_NO_SESSION_FILE: &str = "session database names no session file";

/// The pin: the newest `_sqlx_migrations` version this release was developed
/// against, `projects recency`. Move it forward after reading the migrations
/// beyond it for changes to the two queried tables.
const NEWEST_VERIFIED_MIGRATION: i64 = 52;

/// The newest migration applied beyond [`NEWEST_VERIFIED_MIGRATION`], or
/// `None` while the reader is current.
fn newest_unverified_migration(connection: &Connection) -> Option<String> {
    connection
        .query_row(
            "SELECT MAX(version) FROM _sqlx_migrations WHERE version > ?1",
            [NEWEST_VERIFIED_MIGRATION],
            |row| row.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
        .map(|version| version.to_string())
}

static SCHEMA_PIN: SchemaPin = SchemaPin {
    newest_verified: &NEWEST_VERIFIED_MIGRATION,
    newest_unverified: newest_unverified_migration,
    reported: Once::new(),
};

/// One row of `threads`, in the columns this reader reads.
struct ThreadRow {
    id: String,
    rollout_path: String,
    source: String,
}

fn thread_rows(connection: &Connection) -> rusqlite::Result<Vec<ThreadRow>> {
    let mut statement = connection.prepare("SELECT id, rollout_path, source FROM threads")?;
    let rows = statement.query_map([], |row| {
        Ok(ThreadRow {
            id: row.get(0)?,
            rollout_path: row.get(1)?,
            source: row.get(2)?,
        })
    })?;
    rows.collect()
}

/// The parent of each sub-agent thread, from `thread_spawn_edges`, keyed by
/// the sub-agent.
fn parent_of_subagent(connection: &Connection) -> rusqlite::Result<HashMap<String, String>> {
    let mut statement =
        connection.prepare("SELECT parent_thread_id, child_thread_id FROM thread_spawn_edges")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(1)?, row.get::<_, String>(0)?))
    })?;
    rows.collect()
}

fn thread_kind_of_row(row: &ThreadRow, parent_of_subagent: &HashMap<String, String>) -> ThreadKind {
    match parent_of_subagent.get(&row.id) {
        Some(parent_thread_id) => ThreadKind::Subagent {
            parent_thread_id: parent_thread_id.clone(),
        },
        None => {
            let source = serde_json::from_str::<Value>(&row.source).ok();
            codex::thread_kind_of_source(source.as_ref(), None)
        }
    }
}

/// `rollout_path` re-rooted onto `sessions_tree`: the column is absolute,
/// and a copied Codex home holds the same files under a new root. `None`
/// for a path outside a sessions tree, at a depth the walk does not read,
/// or not named as a rollout.
fn rerooted_rollout(sessions_tree: &Path, rollout_path: &str) -> Option<PathBuf> {
    let within_tree = path_within_sessions_tree(Path::new(rollout_path))?;
    if within_tree.components().count() != codex::SESSIONS_TREE_DEPTH + 1 {
        return None;
    }
    let rollout = sessions_tree.join(within_tree);
    RolloutFileName::parse_path(&rollout)
        .is_some()
        .then_some(rollout)
}

/// `path` relative to the sessions tree holding it,
/// `YYYY/MM/DD/rollout-….jsonl`, or `None` outside one.
fn path_within_sessions_tree(path: &Path) -> Option<&Path> {
    path.strip_prefix(codex::sessions_tree_of(path)?).ok()
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
    use crate::history::provider::sqlite::{
        SESSION_DATABASE_CANNOT_BE_OPENED, SESSION_DATABASE_LOCKED,
    };
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

    fn sessions_root(home: &Path) -> SessionRoot {
        SessionRoot::new(home.join("sessions")).in_agent_tree()
    }

    fn resolved_stub(home: &Path, thread_id: &str) -> Option<SessionStub> {
        let root = sessions_root(home);
        CodexThreadIndex::under(&root)
            .unwrap()
            .stub_of(&root, thread_id)
    }

    /// A pasted thread id names no file on disk: the rollout holding it is
    /// named for a timestamp and the ids together.
    #[test]
    fn a_thread_id_resolves_to_its_newest_rollout() {
        let home = tempfile::tempdir().unwrap();
        write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let newest = write_rollout(home.path(), "2026-08-19T10-00-00", THREAD);

        let stub = resolved_stub(home.path(), THREAD).unwrap();

        assert_eq!(stub.locator, newest);
        assert!(stub.subagents.is_empty());
    }

    #[test]
    fn a_thread_id_codex_never_recorded_resolves_to_nothing() {
        let home = tempfile::tempdir().unwrap();
        write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);

        assert_eq!(resolved_stub(home.path(), OTHER_THREAD), None);
    }

    /// The one exception to every filter the list applies: a sub-agent
    /// thread's id opens the thread as a session of its own, with the threads
    /// beneath it, while a Guardian review's id opens nothing.
    #[test]
    fn a_sub_agent_thread_id_resolves_to_a_stub_of_its_own() {
        let home = tempfile::tempdir().unwrap();
        write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let subagent =
            write_subagent_rollout(home.path(), "2026-08-18T09-10-00", SUBAGENT_THREAD, THREAD);
        let nested = write_subagent_rollout(
            home.path(),
            "2026-08-19T10-00-00",
            NESTED_SUBAGENT_THREAD,
            SUBAGENT_THREAD,
        );
        let review = rollout_path(home.path(), "2026-08-19T11-00-00", OTHER_THREAD);
        codex::test_support::write_guardian_rollout(&review, OTHER_THREAD, THREAD);

        let stub = resolved_stub(home.path(), SUBAGENT_THREAD).unwrap();

        assert_eq!(stub.locator, subagent);
        assert_eq!(stub.subagents, vec![nested]);
        assert_eq!(
            resolved_stub(home.path(), OTHER_THREAD),
            None,
            "a Guardian review's id resolves to nothing"
        );
    }

    /// One stub per session, its sub-agent threads named on it, nested ones
    /// flattened; a rollout an undo superseded is not among them.
    #[test]
    fn discovery_names_each_sessions_sub_agent_threads() {
        let home = tempfile::tempdir().unwrap();
        let parent = write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        write_subagent_rollout(home.path(), "2026-08-18T09-10-00", SUBAGENT_THREAD, THREAD);
        let subagent_newest = write_subagent_rollout(
            home.path(),
            "2026-08-18T09-20-00",
            &format!("{SUBAGENT_THREAD}_019f0000-0000-7000-8000-000000000001"),
            THREAD,
        );
        let nested = write_subagent_rollout(
            home.path(),
            "2026-08-19T10-00-00",
            NESTED_SUBAGENT_THREAD,
            SUBAGENT_THREAD,
        );
        let other = write_rollout(home.path(), "2026-08-19T11-00-00", OTHER_THREAD);

        let discovered = CodexStorage.discover(&sessions_root(home.path())).unwrap();

        let mut stubs = discovered.stubs;
        stubs.sort_by(|left, right| left.locator.cmp(&right.locator));
        assert_eq!(
            stubs
                .iter()
                .map(|stub| (stub.locator.clone(), stub.subagents.clone()))
                .collect::<Vec<_>>(),
            vec![(parent, vec![subagent_newest, nested]), (other, vec![]),]
        );
        assert_eq!(discovered.skipped, 0);
    }

    /// A Guardian review restates the thread it reviewed; listing it or
    /// reading it into its parent would count that conversation twice.
    #[test]
    fn a_guardian_review_is_neither_listed_nor_read_into_its_parent() {
        use crate::history::cache::SessionCacheStore;
        use crate::history::provider::load_sessions_with_cache;

        let home = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        let parent = write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let review = rollout_path(home.path(), "2026-08-19T11-00-00", OTHER_THREAD);
        std::fs::copy(fixture("guardian.jsonl"), &review).unwrap();
        let root = sessions_root(home.path());

        let discovered = CodexStorage.discover(&root).unwrap();
        assert_eq!(
            discovered
                .stubs
                .iter()
                .map(|stub| stub.locator.clone())
                .collect::<Vec<_>>(),
            vec![parent]
        );
        assert!(discovered.stubs[0].subagents.is_empty());
        assert_eq!(discovered.skipped, 1);
        assert_eq!(
            discovered.ignored,
            vec![IgnoredSessions {
                count: 0,
                reason: COMPRESSED_SESSIONS_UNSUPPORTED,
            }],
            "a skipped review is not reported as an ignored session"
        );

        let storage = crate::history::provider::storage::RootedStorage {
            inner: CodexStorage,
            root,
        };
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let listed = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert_eq!(listed.len(), 1);
        let mut parent_alone = discovered.stubs[0].clone();
        parent_alone.subagents.clear();
        let parent_alone = parser::process_session_file(&parent_alone, &codex::CODEX_ROLLOUT, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            listed[0].total_tokens, parent_alone.total_tokens,
            "the review's tokens are not counted"
        );
        assert!(
            !listed[0]
                .agent_search_text
                .contains("GUARDIAN_REVIEW_SENTINEL")
        );
    }

    /// A `thread_spawn` rollout whose parent is not on disk is still the only
    /// record of its conversation, so it lists as a session.
    #[test]
    fn a_sub_agent_thread_whose_parent_is_absent_lists_as_a_session() {
        let home = tempfile::tempdir().unwrap();
        let orphan =
            write_subagent_rollout(home.path(), "2026-08-18T09-10-00", SUBAGENT_THREAD, THREAD);
        let nested = write_subagent_rollout(
            home.path(),
            "2026-08-19T10-00-00",
            NESTED_SUBAGENT_THREAD,
            SUBAGENT_THREAD,
        );

        let discovered = CodexStorage.discover(&sessions_root(home.path())).unwrap();

        assert_eq!(discovered.stubs.len(), 1);
        assert_eq!(discovered.stubs[0].locator, orphan);
        assert_eq!(discovered.stubs[0].subagents, vec![nested]);
    }

    /// A sub-agent thread keeps writing after its parent's last record; the
    /// row must pick that up without the parent's file changing.
    #[test]
    fn a_sub_agent_thread_that_grows_after_its_parent_invalidates_the_row() {
        use crate::history::cache::SessionCacheStore;
        use crate::history::provider::load_sessions_with_cache;

        let home = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let subagent = rollout_path(home.path(), "2026-08-18T09-10-00", SUBAGENT_THREAD);
        std::fs::copy(fixture("subagent.jsonl"), &subagent).unwrap();
        let storage = crate::history::provider::storage::RootedStorage {
            inner: CodexStorage,
            root: sessions_root(home.path()),
        };
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let first = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert_eq!(first.len(), 1);
        assert!(
            first[0]
                .agent_search_text
                .contains("child answer searchable")
        );
        assert!(
            !first[0]
                .agent_search_text
                .contains("later sub-agent answer")
        );

        let mut grown = std::fs::OpenOptions::new()
            .append(true)
            .open(&subagent)
            .unwrap();
        writeln!(
            grown,
            "{}",
            concat!(
                "{\"timestamp\":\"2026-08-02T10:00:07.000Z\",\"type\":\"response_item\",",
                "\"payload\":{\"type\":\"message\",\"role\":\"assistant\",",
                "\"content\":[{\"type\":\"output_text\",\"text\":\"later sub-agent answer\"}]}}",
            )
        )
        .unwrap();
        drop(grown);

        let second = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert!(
            second[0]
                .agent_search_text
                .contains("later sub-agent answer"),
            "the fingerprint spans the sub-agent, so its growth reparses the session"
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

    const GUARDIAN_THREAD: &str = "019f0000-0000-7000-8000-00000000000f";
    const GUARDIAN_SOURCE: &str = r#"{"subagent":{"other":"guardian"}}"#;

    /// The `source` column of a `thread_spawn` row, as Codex 0.150 and later
    /// write it.
    fn thread_spawn_source(parent_thread_id: &str) -> String {
        json!({"subagent": {"thread_spawn": {"parent_thread_id": parent_thread_id, "depth": 1}}})
            .to_string()
    }

    /// Codex's `state_5.sqlite`, restricted to the tables this provider
    /// reads, transcribed from a database at the migration the reader is
    /// pinned at, less the two `REFERENCES` clauses naming tables that are
    /// not transcribed. Transcribed rather than approximated: the schema
    /// moves with every Codex release.
    mod state_database {
        use super::super::{
            NEWEST_VERIFIED_MIGRATION, STATE_DATABASE_FILENAME, path_within_sessions_tree,
        };
        use crate::history::format::codex::RolloutFileName;
        use rusqlite::Connection;
        use std::path::{Path, PathBuf};

        pub(super) fn create(home: &Path) -> Connection {
            let connection = Connection::open(home.join(STATE_DATABASE_FILENAME)).unwrap();
            connection
                .pragma_update(None, "journal_mode", "WAL")
                .unwrap();
            connection
                .execute_batch(
                    r#"
                    CREATE TABLE _sqlx_migrations (
                        version BIGINT PRIMARY KEY,
                        description TEXT NOT NULL,
                        installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                        success BOOLEAN NOT NULL,
                        checksum BLOB NOT NULL,
                        execution_time BIGINT NOT NULL
                    );
                    CREATE TABLE threads (
                        id TEXT PRIMARY KEY,
                        rollout_path TEXT NOT NULL,
                        created_at INTEGER NOT NULL,
                        updated_at INTEGER NOT NULL,
                        source TEXT NOT NULL,
                        model_provider TEXT NOT NULL,
                        cwd TEXT NOT NULL,
                        title TEXT NOT NULL,
                        sandbox_policy TEXT NOT NULL,
                        approval_mode TEXT NOT NULL,
                        tokens_used INTEGER NOT NULL DEFAULT 0,
                        has_user_event INTEGER NOT NULL DEFAULT 0,
                        archived INTEGER NOT NULL DEFAULT 0,
                        archived_at INTEGER,
                        git_sha TEXT,
                        git_branch TEXT,
                        git_origin_url TEXT,
                        cli_version TEXT NOT NULL DEFAULT '',
                        first_user_message TEXT NOT NULL DEFAULT '',
                        agent_nickname TEXT,
                        agent_role TEXT,
                        memory_mode TEXT NOT NULL DEFAULT 'enabled',
                        model TEXT,
                        reasoning_effort TEXT,
                        agent_path TEXT,
                        created_at_ms INTEGER,
                        updated_at_ms INTEGER,
                        thread_source TEXT,
                        preview TEXT NOT NULL DEFAULT '',
                        recency_at INTEGER NOT NULL DEFAULT 0,
                        recency_at_ms INTEGER NOT NULL DEFAULT 0,
                        history_mode TEXT NOT NULL DEFAULT 'legacy',
                        name TEXT,
                        is_pinned INTEGER NOT NULL DEFAULT 0,
                        thread_section_id TEXT,
                        section_position INTEGER,
                        section_entered_at_ms INTEGER,
                        project_id TEXT
                    );
                    CREATE TABLE thread_spawn_edges (
                        parent_thread_id TEXT NOT NULL,
                        child_thread_id TEXT NOT NULL PRIMARY KEY,
                        status TEXT NOT NULL
                    );
                    "#,
                )
                .unwrap();
            insert_migration(&connection, NEWEST_VERIFIED_MIGRATION, "projects recency");
            connection
        }

        pub(super) fn insert_migration(connection: &Connection, version: i64, description: &str) {
            connection
                .execute(
                    "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
                     VALUES (?1, ?2, 1, X'', 0)",
                    rusqlite::params![version, description],
                )
                .unwrap();
        }

        /// A `threads` row for the thread `rollout` is named for, with
        /// `rollout_path` naming the file under another machine's Codex
        /// home, as a copied home's database does.
        pub(super) fn insert_thread(connection: &Connection, rollout: &Path, source: &str) {
            let thread_id = RolloutFileName::parse_path(rollout)
                .unwrap()
                .thread_id
                .to_owned();
            let filed_elsewhere = PathBuf::from("/elsewhere/.codex/sessions")
                .join(path_within_sessions_tree(rollout).unwrap());
            connection
                .execute(
                    "INSERT INTO threads (id, rollout_path, created_at, updated_at, source,
                                          model_provider, cwd, title, sandbox_policy, approval_mode)
                     VALUES (?1, ?2, 0, 0, ?3, 'openai', '/tmp/project', '', 'read-only', 'never')",
                    rusqlite::params![thread_id, filed_elsewhere.to_string_lossy(), source],
                )
                .unwrap();
        }

        pub(super) fn insert_edge(
            connection: &Connection,
            parent_thread_id: &str,
            child_thread_id: &str,
        ) {
            connection
                .execute(
                    "INSERT INTO thread_spawn_edges (parent_thread_id, child_thread_id, status)
                     VALUES (?1, ?2, 'open')",
                    [parent_thread_id, child_thread_id],
                )
                .unwrap();
        }
    }

    /// Each listed session's transcript with its sub-agent transcripts, in
    /// transcript order.
    fn listed_sessions(discovered: &DiscoveredSessions) -> Vec<(PathBuf, Vec<PathBuf>)> {
        let mut sessions = discovered
            .stubs
            .iter()
            .map(|stub| (stub.locator.clone(), stub.subagents.clone()))
            .collect::<Vec<_>>();
        sessions.sort();
        sessions
    }

    fn discover(home: &Path) -> DiscoveredSessions {
        CodexStorage.discover(&sessions_root(home)).unwrap()
    }

    /// A session, its sub-agent thread, a nested sub-agent and a Guardian
    /// review of the session, each with a `threads` row and the sub-agents
    /// with edges. The sub-agent rollouts carry session headers, so the
    /// rollout headers would list three sessions.
    struct IndexedFamily {
        thread: PathBuf,
        subagent: PathBuf,
        nested: PathBuf,
    }

    fn indexed_family(home: &Path) -> IndexedFamily {
        let thread = write_rollout(home, "2026-08-18T09-00-00", THREAD);
        let subagent = write_rollout(home, "2026-08-18T09-10-00", SUBAGENT_THREAD);
        let nested = write_rollout(home, "2026-08-19T10-00-00", NESTED_SUBAGENT_THREAD);
        let review = rollout_path(home, "2026-08-19T11-00-00", GUARDIAN_THREAD);
        codex::test_support::write_guardian_rollout(&review, GUARDIAN_THREAD, THREAD);
        let connection = state_database::create(home);
        state_database::insert_thread(&connection, &thread, "cli");
        state_database::insert_thread(&connection, &subagent, &thread_spawn_source(THREAD));
        state_database::insert_thread(&connection, &nested, &thread_spawn_source(SUBAGENT_THREAD));
        state_database::insert_thread(&connection, &review, GUARDIAN_SOURCE);
        state_database::insert_edge(&connection, THREAD, SUBAGENT_THREAD);
        state_database::insert_edge(&connection, SUBAGENT_THREAD, NESTED_SUBAGENT_THREAD);
        IndexedFamily {
            thread,
            subagent,
            nested,
        }
    }

    #[test]
    fn the_database_lists_each_session_with_its_sub_agent_threads() {
        let home = tempfile::tempdir().unwrap();
        let family = indexed_family(home.path());

        let discovered = discover(home.path());

        assert_eq!(
            listed_sessions(&discovered),
            vec![(family.thread, vec![family.subagent, family.nested])],
            "an absolute rollout_path re-roots onto the sessions tree"
        );
        assert_eq!(discovered.skipped, 1, "the Guardian row is skipped");
        assert_eq!(
            discovered.ignored,
            vec![IgnoredSessions {
                count: 0,
                reason: COMPRESSED_SESSIONS_UNSUPPORTED,
            }]
        );
    }

    /// The same exception the rollout headers make: a sub-agent thread's ID
    /// opens the thread as a session of its own, a Guardian review's opens
    /// nothing.
    #[test]
    fn a_thread_id_resolves_through_the_database() {
        let home = tempfile::tempdir().unwrap();
        let family = indexed_family(home.path());

        let by_id = resolved_stub(home.path(), SUBAGENT_THREAD).unwrap();

        assert_eq!(by_id.locator, family.subagent);
        assert_eq!(by_id.subagents, vec![family.nested]);
        assert_eq!(
            resolved_stub(home.path(), GUARDIAN_THREAD),
            None,
            "a Guardian review's ID resolves to nothing"
        );
    }

    /// One corpus row reads `unknown` with an edge: the edge decides.
    #[test]
    fn a_row_an_edge_names_as_a_sub_agent_is_one_whatever_its_source_holds() {
        let home = tempfile::tempdir().unwrap();
        let parent = write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let unknown = write_rollout(home.path(), "2026-08-18T09-10-00", SUBAGENT_THREAD);
        let interactive = write_rollout(home.path(), "2026-08-18T09-20-00", OTHER_THREAD);
        let connection = state_database::create(home.path());
        state_database::insert_thread(&connection, &parent, "cli");
        state_database::insert_thread(&connection, &unknown, "unknown");
        state_database::insert_thread(&connection, &interactive, "cli");
        state_database::insert_edge(&connection, THREAD, SUBAGENT_THREAD);
        state_database::insert_edge(&connection, THREAD, OTHER_THREAD);
        drop(connection);

        assert_eq!(
            listed_sessions(&discover(home.path())),
            vec![(parent, vec![unknown, interactive])]
        );
    }

    /// A `thread_spawn` row is the only record of its conversation when its
    /// parent has no row, or when no edge names it: it lists as a session.
    #[test]
    fn a_thread_spawn_row_with_no_parent_row_or_no_edge_is_a_session() {
        let home = tempfile::tempdir().unwrap();
        let orphan =
            write_subagent_rollout(home.path(), "2026-08-18T09-10-00", SUBAGENT_THREAD, THREAD);
        let nested = write_subagent_rollout(
            home.path(),
            "2026-08-19T10-00-00",
            NESTED_SUBAGENT_THREAD,
            SUBAGENT_THREAD,
        );
        let edgeless =
            write_subagent_rollout(home.path(), "2026-08-19T11-00-00", OTHER_THREAD, THREAD);
        let connection = state_database::create(home.path());
        state_database::insert_thread(&connection, &orphan, &thread_spawn_source(THREAD));
        state_database::insert_thread(&connection, &nested, &thread_spawn_source(SUBAGENT_THREAD));
        state_database::insert_thread(&connection, &edgeless, &thread_spawn_source(THREAD));
        state_database::insert_edge(&connection, THREAD, SUBAGENT_THREAD);
        state_database::insert_edge(&connection, SUBAGENT_THREAD, NESTED_SUBAGENT_THREAD);
        drop(connection);

        assert_eq!(
            listed_sessions(&discover(home.path())),
            vec![(orphan, vec![nested]), (edgeless, vec![])]
        );
    }

    #[test]
    fn a_row_whose_rollout_is_absent_is_dropped_and_a_compressed_one_is_counted() {
        let home = tempfile::tempdir().unwrap();
        let parent = write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let gone = rollout_path(home.path(), "2026-08-19T10-00-00", OTHER_THREAD);
        write_compressed_rollout(home.path(), "2026-08-01T10-00-00", SUBAGENT_THREAD);
        let compressed = rollout_path(home.path(), "2026-08-01T10-00-00", SUBAGENT_THREAD);
        let connection = state_database::create(home.path());
        for rollout in [&parent, &gone, &compressed] {
            state_database::insert_thread(&connection, rollout, "cli");
        }
        drop(connection);

        let discovered = discover(home.path());

        assert_eq!(listed_sessions(&discovered), vec![(parent, vec![])]);
        assert_eq!(
            discovered.ignored,
            vec![IgnoredSessions {
                count: 1,
                reason: COMPRESSED_SESSIONS_UNSUPPORTED,
            }]
        );
        assert_eq!(discovered.skipped, 0);
    }

    #[test]
    fn a_rollout_with_no_threads_row_is_not_listed() {
        let home = tempfile::tempdir().unwrap();
        let listed = write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        write_rollout(home.path(), "2026-08-19T10-00-00", OTHER_THREAD);
        let connection = state_database::create(home.path());
        state_database::insert_thread(&connection, &listed, "cli");
        drop(connection);

        assert_eq!(
            listed_sessions(&discover(home.path())),
            vec![(listed, vec![])]
        );
    }

    /// The reason and detail of the failure to list `home`.
    fn discover_failure(home: &Path) -> (&'static str, String) {
        match CodexStorage.discover(&sessions_root(home)) {
            Err(AppError::SessionListUnreadable { reason, detail }) => (reason, detail),
            other => panic!("expected an unusable database, got {other:?}"),
        }
    }

    /// An older Codex, or a copied tree, has no database; the headers are
    /// the list then.
    #[test]
    fn an_absent_database_lists_from_the_rollout_headers() {
        let home = tempfile::tempdir().unwrap();
        let parent = write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let subagent =
            write_subagent_rollout(home.path(), "2026-08-18T09-10-00", SUBAGENT_THREAD, THREAD);
        let review = rollout_path(home.path(), "2026-08-19T11-00-00", GUARDIAN_THREAD);
        codex::test_support::write_guardian_rollout(&review, GUARDIAN_THREAD, THREAD);

        let discovered = discover(home.path());

        assert_eq!(listed_sessions(&discovered), vec![(parent, vec![subagent])]);
        assert_eq!(discovered.skipped, 1);
    }

    /// A present database is the list, or the load fails: reading the
    /// headers whenever the database could not be used would list different
    /// sessions between launches.
    #[test]
    fn a_present_database_that_cannot_be_opened_or_read_fails_the_load() {
        let home = tempfile::tempdir().unwrap();
        write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let database = home.path().join(STATE_DATABASE_FILENAME);

        std::fs::create_dir(&database).unwrap();
        let (reason, detail) = discover_failure(home.path());
        assert_eq!(
            reason, SESSION_DATABASE_CANNOT_BE_OPENED,
            "a directory in the database's place"
        );
        assert!(detail.contains(STATE_DATABASE_FILENAME), "{detail}");

        std::fs::remove_dir(&database).unwrap();
        std::fs::write(&database, "not sqlite at all").unwrap();
        assert_eq!(
            discover_failure(home.path()).0,
            SESSION_DATABASE_CANNOT_BE_READ,
            "a file that is not a database"
        );

        std::fs::remove_file(&database).unwrap();
        Connection::open(&database)
            .unwrap()
            .execute_batch("CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY)")
            .unwrap();
        assert_eq!(
            discover_failure(home.path()).0,
            SESSION_DATABASE_CANNOT_BE_READ,
            "a database without the tables"
        );
    }

    /// Codex holds the database while it writes. The reader waits the busy
    /// timeout for the lock, then fails as locked rather than reading the
    /// headers.
    #[test]
    fn a_locked_database_fails_the_load_as_locked_after_the_busy_wait() {
        let home = tempfile::tempdir().unwrap();
        let rollout = write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let connection = state_database::create(home.path());
        state_database::insert_thread(&connection, &rollout, "cli");
        drop(connection);
        let writer = Connection::open(home.path().join(STATE_DATABASE_FILENAME)).unwrap();
        writer
            .pragma_update(None, "locking_mode", "EXCLUSIVE")
            .unwrap();
        writer
            .execute("UPDATE threads SET title = 'held'", [])
            .unwrap();

        let failure = CodexThreadIndex::under_tree(
            &home.path().join("sessions"),
            None,
            Duration::from_millis(50),
        );

        match failure {
            Err(AppError::SessionListUnreadable { reason, .. }) => {
                assert_eq!(reason, SESSION_DATABASE_LOCKED);
            }
            Err(other) => panic!("expected a locked database, got {other}"),
            Ok(index) => panic!(
                "expected a locked database, got an index of {} threads",
                index.parent_of.len()
            ),
        }
    }

    /// A database whose rows all name absent rollouts describes another
    /// tree, so the load fails.
    #[test]
    fn a_database_whose_rows_all_name_missing_files_fails_the_load() {
        let home = tempfile::tempdir().unwrap();
        write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let gone = rollout_path(home.path(), "2026-08-19T10-00-00", OTHER_THREAD);
        let connection = state_database::create(home.path());
        state_database::insert_thread(&connection, &gone, "cli");
        drop(connection);

        assert_eq!(
            discover_failure(home.path()).0,
            SESSION_DATABASE_NAMES_NO_SESSION_FILE
        );
    }

    #[test]
    fn a_database_beyond_the_pin_warns_of_the_newer_migration_and_is_still_read() {
        let home = tempfile::tempdir().unwrap();
        let parent = write_rollout(home.path(), "2026-08-18T09-00-00", THREAD);
        let subagent = write_rollout(home.path(), "2026-08-18T09-10-00", SUBAGENT_THREAD);
        let connection = state_database::create(home.path());
        state_database::insert_thread(&connection, &parent, "cli");
        state_database::insert_thread(&connection, &subagent, &thread_spawn_source(THREAD));
        state_database::insert_edge(&connection, THREAD, SUBAGENT_THREAD);
        assert_eq!(
            newest_unverified_migration(&connection),
            None,
            "a database at the pin has nothing to report"
        );

        let newer = NEWEST_VERIFIED_MIGRATION + 1;
        state_database::insert_migration(&connection, newer, "from the future");
        assert_eq!(
            newest_unverified_migration(&connection),
            Some(newer.to_string())
        );
        assert_eq!(
            listed_sessions(&discover(home.path())),
            vec![(parent, vec![subagent])],
            "the sub-agent rollout carries a session header, so only the database lists one session"
        );

        connection
            .execute("DROP TABLE _sqlx_migrations", [])
            .unwrap();
        assert_eq!(
            newest_unverified_migration(&connection),
            None,
            "a database from before the journal is older than the pin, not newer"
        );
    }

    /// The rollouts a delete of `thread` removes, and one it leaves.
    struct DeletableFamily {
        /// The thread's newest rollout, the one a `threads` row names.
        thread: PathBuf,
        /// A rollout of the thread an undo superseded; the tree alone
        /// names it.
        superseded_rollout: PathBuf,
        subagent: PathBuf,
        nested: PathBuf,
        /// A Guardian review of the thread.
        review: PathBuf,
        unrelated: PathBuf,
    }

    fn deletable_family(home: &Path) -> DeletableFamily {
        let review = rollout_path(home, "2026-08-19T11-00-00", GUARDIAN_THREAD);
        codex::test_support::write_guardian_rollout(&review, GUARDIAN_THREAD, THREAD);
        DeletableFamily {
            superseded_rollout: write_rollout(home, "2026-08-18T09-00-00", THREAD),
            thread: write_rollout(
                home,
                "2026-08-19T10-00-00",
                &format!("{THREAD}_{OTHER_THREAD}"),
            ),
            subagent: write_subagent_rollout(home, "2026-08-18T09-10-00", SUBAGENT_THREAD, THREAD),
            nested: write_subagent_rollout(
                home,
                "2026-08-19T10-00-00",
                NESTED_SUBAGENT_THREAD,
                SUBAGENT_THREAD,
            ),
            review,
            unrelated: write_rollout(home, "2026-08-19T12-00-00", OTHER_THREAD),
        }
    }

    /// Every rollout left under `home`, relative to it.
    fn surviving_rollouts(home: &Path) -> Vec<PathBuf> {
        walk::jsonl_files_at_depth(&home.join("sessions"), codex::SESSIONS_TREE_DEPTH)
            .unwrap()
            .into_iter()
            .map(|rollout| rollout.strip_prefix(home).unwrap().to_path_buf())
            .collect()
    }

    /// Delete reads the threads beneath the target from the same index the
    /// list does, so a present database it cannot read stops the delete
    /// before any file is removed.
    #[test]
    fn delete_with_a_present_database_it_cannot_read_removes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let family = deletable_family(home.path());
        std::fs::write(
            home.path().join(STATE_DATABASE_FILENAME),
            "not sqlite at all",
        )
        .unwrap();
        let before = surviving_rollouts(home.path());

        let failure = CodexProvider.delete_session(&family.thread).unwrap_err();

        assert!(
            matches!(
                failure,
                AppError::SessionListUnreadable {
                    reason: SESSION_DATABASE_CANNOT_BE_READ,
                    ..
                }
            ),
            "{failure}"
        );
        assert_eq!(surviving_rollouts(home.path()), before);
    }

    /// The database names one rollout per thread and no parent for a
    /// Guardian review; a delete through it must still remove the superseded
    /// rollout and the review, as a delete through the rollout headers does.
    #[test]
    fn delete_through_the_database_removes_the_same_files_as_delete_through_the_headers() {
        let with_database = tempfile::tempdir().unwrap();
        let headers_only = tempfile::tempdir().unwrap();
        let indexed = deletable_family(with_database.path());
        let walked = deletable_family(headers_only.path());
        let connection = state_database::create(with_database.path());
        state_database::insert_thread(&connection, &indexed.thread, "cli");
        state_database::insert_thread(&connection, &indexed.subagent, &thread_spawn_source(THREAD));
        state_database::insert_thread(
            &connection,
            &indexed.nested,
            &thread_spawn_source(SUBAGENT_THREAD),
        );
        state_database::insert_thread(&connection, &indexed.review, GUARDIAN_SOURCE);
        state_database::insert_thread(&connection, &indexed.unrelated, "cli");
        state_database::insert_edge(&connection, THREAD, SUBAGENT_THREAD);
        state_database::insert_edge(&connection, SUBAGENT_THREAD, NESTED_SUBAGENT_THREAD);
        drop(connection);

        let deleted_through_database = CodexProvider.delete_session(&indexed.thread).unwrap();
        let deleted_through_headers = CodexProvider.delete_session(&walked.thread).unwrap();

        assert_eq!(deleted_through_database, deleted_through_headers);
        assert_eq!(
            deleted_through_database,
            Deleted {
                stored_copies: 2,
                subagent_sessions: 3,
            }
        );
        assert!(
            !indexed.superseded_rollout.exists(),
            "the database names no superseded rollout; the tree does"
        );
        assert_eq!(
            surviving_rollouts(with_database.path()),
            surviving_rollouts(headers_only.path())
        );
        assert_eq!(
            surviving_rollouts(with_database.path()),
            vec![
                indexed
                    .unrelated
                    .strip_prefix(with_database.path())
                    .unwrap()
            ]
        );
    }
}
