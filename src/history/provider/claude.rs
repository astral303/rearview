//! Claude Code sessions, stored under `~/.claude/projects/<encoded-cwd>/`.

mod project_folder;

pub(crate) use project_folder::convert_path_to_project_dir_name;
use project_folder::decode_project_dir_name_to_path;

use super::storage::UnreadableDirectory;
use super::walk::SessionFiles;
use super::{
    Deleted, DiscoveredSessions, RefNamespaces, ResolvedSession, SessionCache, SessionLaunch,
    SessionLauncher, SessionProvider, SessionRoot, SessionStorage, SessionStub, SourceLabels, walk,
};
use crate::agent::refs::{AgentConversationKey, AgentConversationRef};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::{AppError, Result};
use crate::history::format::claude::{
    CLAUDE_TRANSCRIPT, SUBAGENT_FILE_PREFIX, SUBAGENTS_DIR, rename, session_id_of,
};
use crate::history::format::{self, SessionFormat};
use crate::history::{Conversation, Source, Workspace, is_same_project, parser};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

pub struct ClaudeProvider;

impl SessionProvider for ClaudeProvider {
    fn source(&self) -> Source {
        Source::Claude
    }

    fn labels(&self) -> SourceLabels {
        SourceLabels {
            name: "claude",
            list: "CC",
            display: "Claude",
        }
    }

    fn ref_namespaces(&self) -> RefNamespaces {
        RefNamespaces {
            conversation: "agent-v1",
            project: "agent-project-v1",
        }
    }

    fn storage(&self) -> &dyn SessionStorage {
        &ClaudeStorage
    }

    fn format(&self) -> &dyn SessionFormat {
        &CLAUDE_TRANSCRIPT
    }

    fn launcher(&self) -> &dyn SessionLauncher {
        &ClaudeLauncher
    }

    fn rename_session(&self, path: &Path, title: &str) -> Result<()> {
        rename::append_session_rename(path, title)
    }

    /// Claude deletes by session id rather than by path: the same transcript can
    /// exist under several project directories, and all of its copies go, each
    /// with the session directory holding its sub-agent transcripts.
    fn delete_session(&self, path: &Path) -> Result<Deleted> {
        delete_session_under(&projects_root()?.path, path)
    }

    /// Only a UUID is joined to a project directory, since any other query
    /// could be a path.
    fn is_session_id_shape(&self, query: &str) -> bool {
        crate::search::is_uuid(query)
    }

    /// Claude names each transcript by its session id, so the file name is the
    /// lookup.
    fn resolve_session_id(&self, session_id: &str) -> Result<Option<ResolvedSession>> {
        if !self.is_session_id_shape(session_id) {
            return Ok(None);
        }
        resolved_session_under(&projects_root()?, session_id)
    }

    fn session_id_in_locator(&self, locator: &Path) -> Option<String> {
        session_id_of(locator).map(str::to_owned)
    }

    /// Claude's references are filed under the project folder's name, not
    /// under the directory the session recorded.
    fn ref_project(&self, path: &Path, _project_dir: Option<&Path>) -> Option<String> {
        project_folder_name(path).map(str::to_owned)
    }

    /// Claude's references predate the recipe the other agents share: a
    /// digest of the project folder's name and the file name, naming the
    /// session by the UUID the file name holds.
    fn conversation_ref(&self, key: &AgentConversationKey) -> AgentConversationRef {
        let uuid = session_id_of(Path::new(&key.session_filename))
            .filter(|session_id| crate::search::is_uuid(session_id))
            .unwrap_or("none")
            .to_ascii_lowercase();
        AgentConversationRef::from_digest_of(
            &[
                self.ref_namespaces().conversation,
                &key.project_dir_name,
                &key.session_filename,
            ],
            uuid,
        )
    }

    /// Compares project folder names with any `--worktrees-<branch>` suffix
    /// removed, so sessions from the repository's `.worktrees/` or
    /// `__worktrees/` checkouts match its workspace.
    fn is_in_workspace(
        &self,
        workspace: &Workspace,
        path: &Path,
        _project_dir: Option<&Path>,
    ) -> bool {
        project_folder_name(path).is_some_and(|name| {
            is_same_project(
                name,
                &convert_path_to_project_dir_name(workspace.directory()),
            )
        })
    }

    fn workspace_sessions_dir(&self, directory: &Path) -> Result<Option<PathBuf>> {
        Ok(Some(projects_dir_of(directory)?))
    }

    /// Claude Code exports its transcript's file stem.
    fn current_session_env_var(&self) -> Option<&'static str> {
        Some("CLAUDE_CODE_SESSION_ID")
    }
}

struct ClaudeStorage;

impl SessionStorage for ClaudeStorage {
    fn source(&self) -> Source {
        Source::Claude
    }

    fn cache(&self) -> SessionCache {
        SessionCache {
            directory: "claude",
            magic: *b"CLHIST02",
            schema_version: 1,
        }
    }

    fn roots(&self) -> Result<Vec<SessionRoot>> {
        Ok(vec![projects_root()?])
    }

    /// One stub per `<project>/<session-id>.jsonl`, naming the sub-agent
    /// transcripts in the session's `subagents/` directory. Projects come
    /// most recently modified first, and each project's sessions too, so
    /// sessions with one timestamp list in that order.
    ///
    /// An `agent-*.jsonl` beside the sessions is the flat layout Claude once
    /// wrote sub-agent transcripts in. Such a file names its session only in
    /// its own records, so it is skipped.
    fn discover(&self, root: &SessionRoot) -> Result<DiscoveredSessions> {
        Ok(discover_projects(
            root,
            project_directories_newest_first(&root.path)?,
        ))
    }

    /// The session's project is its transcript's own cwd, or for one recorded
    /// without it, the path decoded from the project folder's name.
    fn parse_session(
        &self,
        stub: &SessionStub,
        _root: &SessionRoot,
        debug_level: Option<DebugLevel>,
        on_transcript_read: &(dyn Fn() + Sync),
    ) -> Result<Option<Conversation>> {
        let parse = |path: &Path, modified: Option<SystemTime>| {
            CLAUDE_TRANSCRIPT.parse_conversation(path, modified, debug_level)
        };
        let Some(mut conversation) =
            parser::process_session_with(stub, parse, debug_level, on_transcript_read)?
        else {
            return Ok(None);
        };
        let project_path = conversation.cwd.clone().unwrap_or_else(|| {
            decode_project_dir_name_to_path(project_folder_name(&stub.locator).unwrap_or_default())
        });
        conversation.project_path = Some(project_path);
        Ok(Some(conversation))
    }

    fn remove_superseded_cache(&self, cache_base: &Path) {
        remove_project_cache(
            cache_base,
            std::env::var("CLAUDE_CONFIG_DIR").ok().as_deref(),
        );
    }
}

/// The magic each per-project cache file of releases up to v0.3.1 opens
/// with.
const PROJECT_CACHE_MAGIC: &[u8; 8] = b"CLHIST01";

/// Remove the per-project cache of releases up to v0.3.1, kept in
/// `projects/` under `cache_base`, or in `config-<hash>/projects/` for a
/// `CLAUDE_CONFIG_DIR`. A failure leaves the files for the next load to
/// remove.
fn remove_project_cache(cache_base: &Path, config_dir: Option<&str>) {
    let Some(config_dir) = config_dir else {
        remove_project_cache_files(&cache_base.join("projects"));
        return;
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(config_dir, &mut hasher);
    let config_cache = cache_base.join(format!(
        "config-{:016x}",
        std::hash::Hasher::finish(&hasher)
    ));
    remove_project_cache_files(&config_cache.join("projects"));
    let _ = std::fs::remove_dir(config_cache);
}

/// Delete the cache files and leftover temp files in `directory` that open
/// with [`PROJECT_CACHE_MAGIC`], then `directory` if nothing else is left in
/// it. `REARVIEW_CACHE_DIR` can name a directory whose `projects/` holds the
/// user's own files, and those stay.
fn remove_project_cache_files(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_project_cache_file_name(&path) && opens_with_project_cache_magic(&path) {
            let _ = std::fs::remove_file(path);
        }
    }
    let _ = std::fs::remove_dir(directory);
}

/// A `.bin` cache file, or the temp file a write that stopped mid-way left
/// beside it.
fn is_project_cache_file_name(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "bin")
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(crate::cache_file::TEMP_FILE_PREFIX))
}

fn opens_with_project_cache_magic(path: &Path) -> bool {
    let mut magic = [0; PROJECT_CACHE_MAGIC.len()];
    std::fs::File::open(path)
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut magic))
        .is_ok_and(|()| &magic == PROJECT_CACHE_MAGIC)
}

fn projects_root() -> Result<SessionRoot> {
    let home = home::home_dir().ok_or_else(|| {
        AppError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not determine home directory",
        ))
    })?;
    Ok(projects_root_from(
        std::env::var("CLAUDE_CONFIG_DIR").ok().as_deref(),
        &home,
    ))
}

/// `$CLAUDE_CONFIG_DIR/projects`, or `~/.claude/projects` when the variable
/// is unset or empty.
fn projects_root_from(config_dir: Option<&str>, home: &Path) -> SessionRoot {
    let base = config_dir
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    SessionRoot::new(base.join("projects")).in_agent_tree()
}

/// The project folder Claude keeps the sessions recorded in `directory` in.
fn projects_dir_of(directory: &Path) -> Result<PathBuf> {
    Ok(projects_root()?
        .path
        .join(convert_path_to_project_dir_name(directory)))
}

/// The name of the project folder holding the transcript at `path`.
fn project_folder_name(path: &Path) -> Option<&str> {
    path.parent()?.file_name()?.to_str()
}

/// The sessions in each of `projects`, in that order. A project folder that
/// cannot be listed is reported, and the others still list: on Windows, a
/// folder Claude Code is deleting refuses to be listed until the delete
/// completes.
fn discover_projects(root: &SessionRoot, projects: Vec<PathBuf>) -> DiscoveredSessions {
    let mut discovered = DiscoveredSessions::complete(Vec::new());
    for project in projects {
        let transcripts = match walk::jsonl_files_at_depth(&project, 0) {
            Ok(transcripts) => transcripts,
            Err(error) => {
                discovered.unreadable_directories.push(UnreadableDirectory {
                    path: project,
                    error: error.to_string(),
                });
                continue;
            }
        };
        let mut sessions = Vec::new();
        for transcript in transcripts {
            if is_subagent_transcript(&transcript) {
                discovered.skipped += 1;
                continue;
            }
            let subagents = subagent_transcripts(&transcript, None);
            sessions.push(SessionFiles {
                transcript,
                subagents,
            });
        }
        let mut project_stubs = walk::session_stubs(root, sessions);
        project_stubs.sort_by_key(|stub| std::cmp::Reverse(stub.fingerprint.modified));
        discovered.stubs.extend(project_stubs);
    }
    discovered
}

/// The project folders under `root`, most recently modified first. A root
/// that does not exist holds none.
fn project_directories_newest_first(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut projects: Vec<(SystemTime, PathBuf)> = walk::subdirectories(root)?
        .into_iter()
        .map(|project| {
            let modified = std::fs::metadata(&project)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (modified, project)
        })
        .collect();
    projects.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    Ok(projects.into_iter().map(|(_, project)| project).collect())
}

/// The session `session_id` names under `root`, as discovery lists it: the
/// first copy found, naming the sub-agent transcripts its session directory
/// holds.
fn resolved_session_under(root: &SessionRoot, session_id: &str) -> Result<Option<ResolvedSession>> {
    let Some(transcript) = stored_copies(&root.path, session_id)?.into_iter().next() else {
        return Ok(None);
    };
    let subagents = subagent_transcripts(&transcript, None);
    Ok(walk::session_stubs(
        root,
        vec![SessionFiles {
            transcript,
            subagents,
        }],
    )
    .pop()
    .map(|stub| ResolvedSession {
        root: root.clone(),
        stub,
    }))
}

/// Every `<project>/<session-id>.jsonl` under `root`: more than one when a
/// fork copied the session across projects.
///
/// Claude names the file with the UUID in lowercase, so the probe is
/// lowercased: a file system that matches names by their bytes would miss
/// the file otherwise.
fn stored_copies(root: &Path, session_id: &str) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let filename = format!("{}.jsonl", crate::search::session_id_for_lookup(session_id));
    let mut copies = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let project_dir = entry?.path();
        if !project_dir.is_dir() {
            continue;
        }
        let candidate = project_dir.join(&filename);
        if candidate.exists() {
            copies.push(candidate);
        }
    }
    Ok(copies)
}

/// Delete every copy under `root` of the session `path` names, each with its
/// session directory (`tool-results/`, `subagents/`), and only copies
/// Claude's format reads as its own. A sub-agent transcript copied with the
/// session across projects counts once.
fn delete_session_under(root: &Path, path: &Path) -> Result<Deleted> {
    let session_id = session_id_of(path).unwrap_or_default();
    // Validate format to prevent path traversal
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(AppError::SessionNotFound(session_id.to_owned()));
    }
    format::require_owned_transcript(Source::Claude, path)?;

    let mut copies = Vec::new();
    for copy in stored_copies(root, session_id)? {
        if format::parse_owned_transcript(Source::Claude, &copy)?.is_some() {
            copies.push(copy);
        }
    }
    if copies.is_empty() {
        return Err(AppError::SessionNotFound(session_id.to_owned()));
    }

    let mut subagent_files = HashSet::new();
    for transcript in &copies {
        subagent_files.extend(
            subagent_transcripts(transcript, None)
                .into_iter()
                .filter_map(|subagent| subagent.file_name().map(ToOwned::to_owned)),
        );
        std::fs::remove_file(transcript)?;

        let session_dir = transcript.with_extension("");
        if session_dir.is_dir() {
            std::fs::remove_dir_all(&session_dir)?;
        }
    }

    Ok(Deleted {
        stored_copies: copies.len(),
        subagent_sessions: subagent_files.len(),
    })
}

/// The sub-agent transcripts of the session at `transcript`, as every path
/// that starts from the session names them: the list, session-ID lookup,
/// delete and the agent CLI. A `subagents/` directory that cannot be read
/// leaves the session with none and is reported at warn level, so the
/// session still lists, opens and deletes.
pub(crate) fn subagent_transcripts(
    transcript: &Path,
    debug_level: Option<DebugLevel>,
) -> Vec<PathBuf> {
    read_subagent_transcripts(transcript).unwrap_or_else(|error| {
        debug::warn(
            debug_level,
            &format!(
                "Failed to list the sub-agent transcripts of {}: {error}",
                transcript.display()
            ),
        );
        Vec::new()
    })
}

/// `<project>/<session-id>/subagents/agent-*.jsonl`, sorted so successive
/// runs agree. An absent `subagents/` directory, the usual case, is no
/// sub-agents; one that cannot be read is an error. A nested sub-agent's
/// transcript sits in the same directory, so no walk descends.
fn read_subagent_transcripts(transcript: &Path) -> Result<Vec<PathBuf>> {
    let directory = transcript.with_extension("").join(SUBAGENTS_DIR);
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut transcripts = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if is_subagent_transcript(&path) {
            transcripts.push(path);
        }
    }
    transcripts.sort();
    Ok(transcripts)
}

/// `agent-<id>.jsonl`: a sub-agent transcript under `subagents/`, or, beside
/// the sessions themselves, one in the flat layout Claude used before.
pub(crate) fn is_subagent_transcript(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(SUBAGENT_FILE_PREFIX))
}

struct ClaudeLauncher;

impl SessionLauncher for ClaudeLauncher {
    fn resume_command(&self, launch: &SessionLaunch) -> Result<Command> {
        claude_command(launch, false)
    }

    fn fork_command(&self, launch: &SessionLaunch) -> Result<Command> {
        claude_command(launch, true)
    }
}

/// Claude resumes by conversation id and finds the transcript by the project
/// directory it runs in, so the command is only half the work: when the session
/// lives under a project Claude would not look in, its files are copied to one it
/// would.
fn claude_command(launch: &SessionLaunch, fork_session: bool) -> Result<Command> {
    let conversation_id = session_id_of(launch.path)
        .ok_or_else(|| launch_error("the session's file name is not valid Unicode"))?
        .to_owned();

    let cwd = std::env::current_dir().map_err(|error| {
        AppError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Failed to get current directory: {error}"),
        ))
    })?;

    let conv_projects_dir = conversation_projects_dir(launch.path)?;

    let mut command = Command::new("claude");
    command.args(["--resume", &conversation_id]);
    match resolve_resume_action(launch.path, launch.project_path, &cwd, fork_session)? {
        ResumeAction::CopyToCurrent { cwd_projects_dir } => {
            std::fs::create_dir_all(&cwd_projects_dir).map_err(AppError::Io)?;
            copy_session_files(
                launch.path,
                &conversation_id,
                conv_projects_dir,
                &cwd_projects_dir,
            )?;
            command.args(launch.configured_args);
            command.current_dir(&cwd);
        }
        ResumeAction::Run { current_dir } => {
            if fork_session {
                command.arg("--fork-session");
            }
            command.args(launch.configured_args);
            command.current_dir(current_dir);
        }
    }
    Ok(command)
}

fn conversation_projects_dir(selected_path: &Path) -> Result<&Path> {
    selected_path
        .parent()
        .ok_or_else(|| launch_error("the session's file has no project folder"))
}

fn launch_error(detail: &str) -> AppError {
    AppError::AgentLaunch {
        agent: ClaudeProvider.labels().display,
        detail: detail.to_owned(),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ResumeAction {
    Run { current_dir: PathBuf },
    CopyToCurrent { cwd_projects_dir: PathBuf },
}

fn resolve_resume_action(
    selected_path: &Path,
    project_path: Option<&Path>,
    cwd: &Path,
    fork_session: bool,
) -> Result<ResumeAction> {
    let conv_projects_dir = conversation_projects_dir(selected_path)?;
    let cwd_projects_dir = projects_dir_of(cwd)?;
    let project_dir = project_path.filter(|path| path.exists() && path.is_dir());

    if project_dir.is_none() || (fork_session && cwd_projects_dir != conv_projects_dir) {
        return Ok(ResumeAction::CopyToCurrent { cwd_projects_dir });
    }

    if cwd_projects_dir == conv_projects_dir {
        return Ok(ResumeAction::Run {
            current_dir: cwd.to_path_buf(),
        });
    }

    let project_dir = project_dir.unwrap();
    let project_projects_dir = projects_dir_of(project_dir)?;
    if project_projects_dir == conv_projects_dir {
        Ok(ResumeAction::Run {
            current_dir: project_dir.to_path_buf(),
        })
    } else {
        Ok(ResumeAction::CopyToCurrent { cwd_projects_dir })
    }
}

/// Copy a session into another project directory so a cross-project fork can find
/// it: the transcript, plus the session subdirectory holding tool results and
/// sub-agent transcripts.
///
/// `~/.claude/file-history/<uuid>/` is global rather than per project, and Claude
/// finds it by session id, so it needs no copy.
fn copy_session_files(
    jsonl_path: &Path,
    session_id: &str,
    source_projects_dir: &Path,
    target_projects_dir: &Path,
) -> Result<()> {
    let target_jsonl = target_projects_dir.join(jsonl_path.file_name().unwrap());
    std::fs::copy(jsonl_path, &target_jsonl).map_err(AppError::Io)?;

    let session_dir = source_projects_dir.join(session_id);
    if session_dir.is_dir() {
        copy_dir_recursive(&session_dir, &target_projects_dir.join(session_id))?;
    }

    Ok(())
}

pub(crate) fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination).map_err(AppError::Io)?;
    for entry in std::fs::read_dir(source).map_err(AppError::Io)? {
        let entry = entry.map_err(AppError::Io)?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else {
            std::fs::copy(&source_path, &destination_path).map_err(AppError::Io)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::history::cache::SessionCacheStore;
    use crate::history::provider::contract_tests::{
        Contract, FixtureIds, IdCase, Nesting, OptOut, ProviderFixture,
    };
    use crate::history::provider::load_sessions_with_cache;
    use crate::history::provider::storage::RootedStorage;
    use serde_json::{Value, json};

    /// A two-turn Claude transcript at `path`.
    fn write_transcript(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let user = json!({
            "type": "user", "timestamp": "2026-07-26T06:30:00.000Z",
            "message": {"role": "user", "content": "a question"}
        });
        let assistant = json!({
            "type": "assistant", "timestamp": "2026-07-26T06:30:05.000Z",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "an answer"}]}
        });
        std::fs::write(path, format!("{user}\n{assistant}\n")).unwrap();
    }

    /// Claude's sessions for the provider contracts in `contract_tests`.
    pub(crate) struct ClaudeFixture;

    impl ProviderFixture for ClaudeFixture {
        fn provider(&self) -> &'static dyn SessionProvider {
            Source::Claude.provider()
        }

        fn opt_outs(&self) -> &'static [OptOut] {
            const OPT_OUTS: &[OptOut] = &[OptOut {
                contracts: &[Contract::SubAgentIdLookup],
                reason: "a Claude sub-agent transcript does not resolve by id",
            }];
            OPT_OUTS
        }

        fn ids(&self) -> FixtureIds {
            FixtureIds {
                session: "0f000000-0000-4000-8000-000000000001",
                other: "1e000000-0000-4000-8000-000000000002",
                unknown: "2d000000-0000-4000-8000-000000000003",
                session_in_other_case: "0F000000-0000-4000-8000-000000000001",
                child: Some("a1111111111111111"),
                nested: Nesting::Recorded("b2222222222222222"),
            }
        }

        fn id_case(&self) -> IdCase {
            IdCase::Insensitive
        }

        fn root_under(&self, home: &Path) -> SessionRoot {
            SessionRoot::new(home)
        }

        fn write_session(&self, home: &Path, session: &str) -> PathBuf {
            let transcript = home
                .join("-tmp-claude-project")
                .join(format!("{session}.jsonl"));
            write_transcript(&transcript);
            transcript
        }

        fn write_subagent(&self, home: &Path, session: &str, child: &str) -> PathBuf {
            let transcript = home
                .join("-tmp-claude-project")
                .join(session)
                .join("subagents")
                .join(format!("agent-{child}.jsonl"));
            write_transcript(&transcript);
            transcript
        }

        /// A nested sub-agent's transcript sits in the same `subagents/`
        /// directory as its parent's.
        fn write_nested_subagent(
            &self,
            home: &Path,
            session: &str,
            _parent: &str,
            child: &str,
        ) -> PathBuf {
            self.write_subagent(home, session, child)
        }

        /// The `agentId` a sub-agent transcript is named after.
        fn subagent_id(&self, _parent: &str, child: &str) -> String {
            child.to_owned()
        }

        fn resolve_under(&self, home: &Path, id: &str) -> Result<Option<ResolvedSession>> {
            resolved_session_under(&self.root_under(home), id)
        }

        fn delete_under(&self, home: &Path, locator: &Path) -> Result<Deleted> {
            delete_session_under(home, locator)
        }

        fn roots_from(&self, override_dir: Option<&str>, home: &Path) -> Vec<PathBuf> {
            vec![projects_root_from(override_dir, home).path]
        }

        /// Claude names a transcript by its session id inside a project
        /// directory.
        fn foreign_transcript(&self, directory: &Path) -> PathBuf {
            let path = directory
                .join("-tmp-claude-project")
                .join(format!("{}.jsonl", self.ids().session));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl"),
                &path,
            )
            .unwrap();
            path
        }
    }

    /// A session's directory holds `tool-results/` far more often than
    /// `subagents/`, and `subagents/` holds the sidecars beside the
    /// transcripts.
    #[test]
    fn subagent_transcripts_are_the_agent_jsonl_files_under_the_sessions_subagents_dir() {
        let project = tempfile::tempdir().unwrap();
        let transcript = project
            .path()
            .join("7b2f3c1e-4a5d-4e6f-8a9b-0c1d2e3f4a5b.jsonl");
        std::fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();
        assert_eq!(
            read_subagent_transcripts(&transcript).unwrap(),
            Vec::<PathBuf>::new(),
            "a session with no directory has none"
        );

        let session_dir = project.path().join("7b2f3c1e-4a5d-4e6f-8a9b-0c1d2e3f4a5b");
        std::fs::create_dir_all(session_dir.join("tool-results")).unwrap();
        assert_eq!(
            read_subagent_transcripts(&transcript).unwrap(),
            Vec::<PathBuf>::new(),
            "a session directory with no subagents/ has none"
        );

        let subagents = session_dir.join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let second = subagents.join("agent-b2222222222222222.jsonl");
        let first = subagents.join("agent-a1111111111111111.jsonl");
        for path in [&second, &first] {
            std::fs::write(path, "{\"type\":\"user\"}\n").unwrap();
        }
        std::fs::write(subagents.join("agent-a1111111111111111.meta.json"), "{}").unwrap();
        std::fs::write(subagents.join("notes.txt"), "kept").unwrap();

        assert_eq!(
            read_subagent_transcripts(&transcript).unwrap(),
            vec![first, second]
        );
    }

    /// `subagents/` is a file where the directory should be. The session
    /// still resolves by ID, without sub-agents, as it lists.
    #[test]
    fn a_session_whose_subagents_directory_cannot_be_read_still_resolves_by_id() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join(FIXTURE_PROJECT);
        let transcript = project.join(format!("{FIXTURE_SESSION}.jsonl"));
        write_transcript(&transcript);
        let session_dir = project.join(FIXTURE_SESSION);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join(SUBAGENTS_DIR), "not a directory").unwrap();
        assert!(read_subagent_transcripts(&transcript).is_err());

        let resolved = resolved_session_under(&SessionRoot::new(root.path()), FIXTURE_SESSION)
            .unwrap()
            .expect("the transcript is on disk");

        assert_eq!(resolved.root, SessionRoot::new(root.path()));
        assert_eq!(resolved.stub.locator, transcript);
        assert!(resolved.stub.subagents.is_empty());
    }

    const FIXTURE_PROJECT: &str = "-tmp-claude-subagent-fixture";
    const FIXTURE_SESSION: &str = "7b2f3c1e-4a5d-4e6f-8a9b-0c1d2e3f4a5b";
    const FIXTURE_SUBAGENTS: [&str; 3] = [
        "agent-a1111111111111111.jsonl",
        "agent-b2222222222222222.jsonl",
        "agent-c3333333333333333.jsonl",
    ];

    fn fixture_project() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/claude")
            .join(FIXTURE_PROJECT)
    }

    /// A projects root holding a copy of the fixture project, so a test can
    /// add to or break it.
    fn fixture_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        copy_dir_recursive(&fixture_project(), &root.path().join(FIXTURE_PROJECT)).unwrap();
        root
    }

    /// Claude's storage with its root pinned to `root`.
    fn storage_under(root: &Path) -> RootedStorage<ClaudeStorage> {
        RootedStorage {
            inner: ClaudeStorage,
            root: SessionRoot::new(root),
        }
    }

    /// Every session under `root`, through the shared load loop with its
    /// cache under `cache_base`.
    fn load_under(root: &Path, cache_base: &Path) -> Vec<Conversation> {
        let storage = storage_under(root);
        let cache = SessionCacheStore::under(cache_base, storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap()
    }

    /// The fixture project's one session, through a cache under `cache_base`.
    fn load_the_one_session(root: &Path, cache_base: &Path) -> Conversation {
        let mut conversations = load_under(root, cache_base);
        assert_eq!(conversations.len(), 1, "the project holds one session");
        conversations.remove(0)
    }

    fn parsed_alone(transcript: &Path) -> Conversation {
        CLAUDE_TRANSCRIPT
            .parse_conversation(transcript, None, None)
            .unwrap()
            .expect("the transcript holds a conversation")
    }

    fn write_subagent_transcript(path: &Path, text: &str) {
        let user = json!({
            "type": "user", "isSidechain": true, "agentId": "d4444444444444444",
            "timestamp": "2026-07-26T06:30:00.000Z",
            "message": {"role": "user", "content": "one more question"}
        });
        let assistant = json!({
            "type": "assistant", "isSidechain": true, "agentId": "d4444444444444444",
            "timestamp": "2026-07-26T06:30:05.000Z",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
        });
        std::fs::write(path, format!("{user}\n{assistant}\n")).unwrap();
    }

    /// The row is the session plus its sub-agent transcripts, the nested one
    /// (`spawnDepth: 2`) included: their turns counted, their tokens summed,
    /// and their text searchable by the agent CLI but not by the list. A
    /// second load restores the same row from the cache.
    #[test]
    fn a_claude_session_lists_once_with_its_sub_agents_merged_in() {
        let root = fixture_root();
        let cache = tempfile::tempdir().unwrap();
        let session = load_the_one_session(root.path(), cache.path());

        let project = root.path().join(FIXTURE_PROJECT);
        let subagents_dir = project.join(FIXTURE_SESSION).join("subagents");
        let subagents = FIXTURE_SUBAGENTS.map(|name| subagents_dir.join(name));
        assert_eq!(session.subagents, subagents);

        let alone = parsed_alone(&project.join(format!("{FIXTURE_SESSION}.jsonl")));
        let threads = subagents.iter().map(|path| parsed_alone(path));
        let (thread_messages, thread_tokens) =
            threads.fold((0, 0), |(messages, tokens), thread| {
                assert!(thread.message_count > 0);
                (
                    messages + thread.message_count,
                    tokens + thread.total_tokens,
                )
            });
        assert_eq!(session.message_count, alone.message_count + thread_messages);
        assert_eq!(session.total_tokens, alone.total_tokens + thread_tokens);
        for sentinel in [
            "EXPLORE_SUBAGENT_SENTINEL",
            "GENERAL_SUBAGENT_SENTINEL",
            "NESTED_SUBAGENT_SENTINEL",
        ] {
            assert!(session.agent_search_text.contains(sentinel), "{sentinel}");
            assert!(!session.full_text.contains(sentinel), "{sentinel}");
        }
        assert!(session.full_text.contains("PARENT_ANSWER_SENTINEL"));

        // The second load restores the row from the cache. The hit is proved
        // by rewriting a sub-agent transcript under its original size and
        // mtime: a re-parse would show the rewritten text.
        let nested = subagents_dir.join(FIXTURE_SUBAGENTS[2]);
        let modified = std::fs::metadata(&nested).unwrap().modified().unwrap();
        let rewritten = std::fs::read_to_string(&nested)
            .unwrap()
            .replace("NESTED_SUBAGENT_SENTINEL", "NESTED_SUBAGENT_REWRITE_");
        std::fs::write(&nested, rewritten).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&nested)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let restored = load_the_one_session(root.path(), cache.path());
        assert!(
            restored
                .agent_search_text
                .contains("NESTED_SUBAGENT_SENTINEL"),
            "the second load is a cache hit"
        );
        assert_eq!(restored.subagents, session.subagents);
        assert_eq!(restored.message_count, session.message_count);
        assert_eq!(restored.total_tokens, session.total_tokens);
        assert_eq!(restored.agent_search_text, session.agent_search_text);
        assert_eq!(restored.semantic_route_text, session.semantic_route_text);
    }

    /// Semantic and hybrid `agent search` route to a session by its
    /// `semantic_route_text`, so a phrase only a sub-agent transcript holds
    /// has to reach it.
    #[test]
    fn a_claude_sub_agents_text_reaches_the_sessions_semantic_routing_text() {
        let root = fixture_root();
        let cache = tempfile::tempdir().unwrap();
        let session = load_the_one_session(root.path(), cache.path());

        assert!(!session.full_text.contains("NESTED_SUBAGENT_SENTINEL"));
        assert!(
            session
                .semantic_route_text
                .contains("NESTED_SUBAGENT_SENTINEL"),
            "{}",
            session.semantic_route_text
        );
    }

    /// Most session directories hold `tool-results/` alone.
    #[test]
    fn a_session_directory_holding_tool_results_alone_changes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join(FIXTURE_PROJECT);
        std::fs::create_dir_all(&project).unwrap();
        let transcript = project.join(format!("{FIXTURE_SESSION}.jsonl"));
        std::fs::copy(
            fixture_project().join(format!("{FIXTURE_SESSION}.jsonl")),
            &transcript,
        )
        .unwrap();
        let tool_results = project.join(FIXTURE_SESSION).join("tool-results");
        std::fs::create_dir_all(&tool_results).unwrap();
        std::fs::write(
            tool_results.join("toolu_01FIXTUREAAAAAAAAAAAAAAA.txt"),
            "output",
        )
        .unwrap();

        let cache = tempfile::tempdir().unwrap();
        let session = load_the_one_session(root.path(), cache.path());

        let alone = parsed_alone(&transcript);
        assert!(session.subagents.is_empty());
        assert_eq!(session.message_count, alone.message_count);
        assert_eq!(session.total_tokens, alone.total_tokens);
        assert_eq!(session.agent_search_text, alone.agent_search_text);
    }

    /// The entry's stamp spans the sub-agent transcripts, so one written
    /// after the session's own last write is a miss, not a stale hit.
    #[test]
    fn a_sub_agent_written_after_the_sessions_last_write_invalidates_the_cache_entry() {
        let root = fixture_root();
        let cache = tempfile::tempdir().unwrap();
        let before = load_the_one_session(root.path(), cache.path());

        let late = root
            .path()
            .join(FIXTURE_PROJECT)
            .join(FIXTURE_SESSION)
            .join("subagents")
            .join("agent-d4444444444444444.jsonl");
        write_subagent_transcript(&late, "LATE_SUBAGENT_SENTINEL: written after the session");
        let after = load_the_one_session(root.path(), cache.path());

        assert_eq!(before.subagents.len(), 3);
        assert_eq!(after.subagents.len(), 4);
        assert!(after.agent_search_text.contains("LATE_SUBAGENT_SENTINEL"));
        assert_eq!(
            after.message_count,
            before.message_count + parsed_alone(&late).message_count
        );
    }

    /// A directory where a transcript should be cannot be read. The session
    /// still lists, without it; the row names it, as discovery found it.
    #[test]
    fn an_unreadable_sub_agent_transcript_is_left_out_of_its_session() {
        let intact = fixture_root();
        let broken = fixture_root();
        std::fs::create_dir(
            broken
                .path()
                .join(FIXTURE_PROJECT)
                .join(FIXTURE_SESSION)
                .join("subagents")
                .join("agent-d4444444444444444.jsonl"),
        )
        .unwrap();

        let intact_cache = tempfile::tempdir().unwrap();
        let broken_cache = tempfile::tempdir().unwrap();
        let expected = load_the_one_session(intact.path(), intact_cache.path());
        let session = load_the_one_session(broken.path(), broken_cache.path());

        assert_eq!(session.subagents.len(), 4);
        assert_eq!(session.message_count, expected.message_count);
        assert_eq!(session.total_tokens, expected.total_tokens);
        assert_eq!(session.agent_search_text, expected.agent_search_text);
    }

    /// `subagents/` is a file where the directory should be. The session
    /// is still deleted, its directory with it, and the delete reports no
    /// sub-agent sessions, as the list showed none.
    #[test]
    fn deleting_a_session_whose_subagents_directory_cannot_be_read_still_deletes_it() {
        let root = fixture_root();
        let project = root.path().join(FIXTURE_PROJECT);
        let transcript = project.join(format!("{FIXTURE_SESSION}.jsonl"));
        let session_dir = project.join(FIXTURE_SESSION);
        std::fs::remove_dir_all(session_dir.join("subagents")).unwrap();
        std::fs::write(session_dir.join("subagents"), "not a directory").unwrap();

        let deleted = delete_session_under(root.path(), &transcript).unwrap();

        assert_eq!(deleted, Deleted::just_the_session());
        assert!(!transcript.exists());
        assert!(!session_dir.exists());
    }

    /// A fork copied the session into a second project, its sub-agent
    /// transcripts with it.
    #[test]
    fn deleting_a_copied_session_deletes_every_copy_and_counts_its_sub_agents_once() {
        let root = fixture_root();
        let original = root.path().join(FIXTURE_PROJECT);
        let fork = root.path().join("-tmp-fork");
        copy_dir_recursive(&original, &fork).unwrap();

        let deleted = delete_session_under(
            root.path(),
            &original.join(format!("{FIXTURE_SESSION}.jsonl")),
        )
        .unwrap();

        assert_eq!(
            deleted,
            Deleted {
                stored_copies: 2,
                subagent_sessions: FIXTURE_SUBAGENTS.len(),
            }
        );
        for project in [&original, &fork] {
            assert!(!project.join(format!("{FIXTURE_SESSION}.jsonl")).exists());
            assert!(!project.join(FIXTURE_SESSION).exists());
        }
    }

    /// Another agent's transcript named with the session's id, in another
    /// project folder, is not a copy of the session.
    #[test]
    fn deleting_a_session_leaves_another_agents_file_named_with_its_id() {
        let root = fixture_root();
        let transcript = root
            .path()
            .join(FIXTURE_PROJECT)
            .join(format!("{FIXTURE_SESSION}.jsonl"));
        let foreign = root
            .path()
            .join("-tmp-other")
            .join(format!("{FIXTURE_SESSION}.jsonl"));
        std::fs::create_dir_all(foreign.parent().unwrap()).unwrap();
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl"),
            &foreign,
        )
        .unwrap();

        let deleted = delete_session_under(root.path(), &transcript).unwrap();

        assert_eq!(deleted.stored_copies, 1);
        assert!(!transcript.exists());
        assert!(foreign.exists());
    }

    /// A transcript written as `user`, in the project folder `project`.
    fn write_session_in(root: &Path, project: &str, session: &str, user: Value) -> PathBuf {
        let transcript = root.join(project).join(format!("{session}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let assistant = json!({
            "type": "assistant", "timestamp": "2026-07-26T06:30:05.000Z",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "an answer"}]}
        });
        std::fs::write(&transcript, format!("{user}\n{assistant}\n")).unwrap();
        transcript
    }

    /// A fork copies a session into the project it runs in, so one id can
    /// name a transcript in two project folders: each lists, under the key
    /// of its own path. A session recorded without a cwd is filed under the
    /// path its project folder's name decodes to, from the cache as from
    /// the transcript. A sub-agent transcript in the flat layout beside the
    /// sessions lists as nothing.
    #[test]
    fn discovery_lists_copies_apart_and_files_a_session_without_a_cwd_by_its_folder() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let copied = "3a000000-0000-4000-8000-000000000001";
        let without_cwd = "4b000000-0000-4000-8000-000000000002";
        let user_in = |cwd: Option<&str>| {
            let mut user = json!({
                "type": "user", "timestamp": "2026-07-26T06:30:00.000Z",
                "message": {"role": "user", "content": "a question"}
            });
            if let Some(cwd) = cwd {
                user["cwd"] = json!(cwd);
            }
            user
        };
        let original = write_session_in(
            root.path(),
            "-tmp-original",
            copied,
            user_in(Some("/tmp/original")),
        );
        let copy = write_session_in(
            root.path(),
            "-tmp-fork",
            copied,
            user_in(Some("/tmp/original")),
        );
        write_session_in(root.path(), "-tmp-no-cwd", without_cwd, user_in(None));
        write_session_in(
            root.path(),
            "-tmp-no-cwd",
            "agent-a1111111111111111",
            user_in(None),
        );

        let discovered = ClaudeStorage
            .discover(&SessionRoot::new(root.path()))
            .unwrap();

        let mut keys: Vec<&str> = discovered
            .stubs
            .iter()
            .map(|stub| stub.cache_key.as_str())
            .collect();
        keys.sort_unstable();
        let key = |project: &str, session: &str| {
            Path::new(project)
                .join(format!("{session}.jsonl"))
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(
            keys,
            [
                key("-tmp-fork", copied),
                key("-tmp-no-cwd", without_cwd),
                key("-tmp-original", copied),
            ]
        );
        assert_eq!(discovered.skipped, 1, "the flat sub-agent transcript");

        for load in ["cold", "warm"] {
            let conversations = load_under(root.path(), cache.path());
            let mut paths: Vec<&Path> = conversations
                .iter()
                .map(|conversation| conversation.path.as_path())
                .collect();
            paths.sort_unstable();
            let mut expected = vec![copy.as_path(), original.as_path()];
            expected.sort_unstable();
            assert_eq!(
                paths
                    .iter()
                    .filter(|path| path.file_stem().unwrap() == copied)
                    .copied()
                    .collect::<Vec<_>>(),
                expected,
                "{load}"
            );
            let filed_by_folder = conversations
                .iter()
                .find(|conversation| conversation.session_id == without_cwd)
                .unwrap();
            assert_eq!(filed_by_folder.cwd, None, "{load}");
            assert_eq!(
                filed_by_folder.project_path,
                Some(decode_project_dir_name_to_path("-tmp-no-cwd")),
                "{load}"
            );
        }
    }

    /// A file where a project folder should be cannot be listed, as a folder
    /// mid-delete cannot on Windows.
    #[test]
    fn a_project_folder_that_cannot_be_listed_leaves_the_others_listed() {
        let root = fixture_root();
        let unreadable = root.path().join("-tmp-unreadable");
        std::fs::write(&unreadable, "not a directory").unwrap();

        let discovered = discover_projects(
            &SessionRoot::new(root.path()),
            vec![unreadable.clone(), root.path().join(FIXTURE_PROJECT)],
        );

        assert_eq!(discovered.stubs.len(), 1);
        assert_eq!(
            discovered
                .unreadable_directories
                .iter()
                .map(|directory| &directory.path)
                .collect::<Vec<_>>(),
            [&unreadable]
        );
    }

    /// A Claude session's `ch_` reference and project id are digests of its
    /// project folder's name, whatever directory it recorded; the pinned
    /// values match earlier releases.
    #[test]
    fn a_claude_sessions_reference_is_filed_under_its_project_folder() {
        let mut conversation = crate::search::test_fixtures::one_message_conversation(
            "hello",
            chrono::Local::now(),
            None,
            None,
            None,
        );
        conversation.source = Source::Claude;
        conversation.path =
            PathBuf::from("/projects/-tmp-project/12345678-1234-4234-9234-123456789abc.jsonl");
        conversation.session_id = "12345678-1234-4234-9234-123456789abc".to_owned();
        conversation.project_path = Some(PathBuf::from("/somewhere/else"));

        let key = AgentConversationKey::from_conversation(&conversation).unwrap();

        assert_eq!(key.project_dir_name, "-tmp-project");
        assert_eq!(key.conversation_ref().canonical(), "ch_2eb29a5ff6fe");
        assert_eq!(
            key.conversation_ref().uuid(),
            "12345678-1234-4234-9234-123456789abc"
        );
        assert_eq!(key.project_id(), "pr_43f686a8bc2ab51b");
    }

    /// A per-project cache file of releases up to v0.3.1: its magic, a
    /// schema version, then entries.
    fn write_project_cache_file(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            [&PROJECT_CACHE_MAGIC[..], &16u32.to_le_bytes()].concat(),
        )
        .unwrap();
    }

    /// Releases up to v0.3.1 cached Claude per project directory, under a
    /// directory named for the `CLAUDE_CONFIG_DIR` when one was set. The
    /// shard cache beside it stays.
    #[test]
    fn the_per_project_cache_of_earlier_releases_is_removed() {
        let base = tempfile::tempdir().unwrap();
        let shards = base.path().join("claude").join("root-0000000000000000");
        std::fs::create_dir_all(&shards).unwrap();
        let unconfigured = base.path().join("projects");
        write_project_cache_file(&unconfigured.join("-tmp-project.bin"));

        remove_project_cache(base.path(), None);

        assert!(!unconfigured.exists());
        assert!(shards.exists());

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&"/elsewhere/.claude".to_owned(), &mut hasher);
        let configured = base.path().join(format!(
            "config-{:016x}",
            std::hash::Hasher::finish(&hasher)
        ));
        write_project_cache_file(&configured.join("projects").join("-tmp-project.bin"));

        remove_project_cache(base.path(), Some("/elsewhere/.claude"));

        assert!(!configured.exists());
        assert!(shards.exists());
    }

    /// `REARVIEW_CACHE_DIR` can name a directory whose `projects/` is the
    /// user's own.
    #[test]
    fn removing_the_per_project_cache_keeps_every_other_file_in_projects() {
        let base = tempfile::tempdir().unwrap();
        let projects = base.path().join("projects");
        let cache_file = projects.join("-tmp-project.bin");
        write_project_cache_file(&cache_file);
        let notes = projects.join("notes.txt");
        std::fs::write(&notes, "kept").unwrap();
        let other_bin = projects.join("model.bin");
        std::fs::write(&other_bin, "not a cache file").unwrap();
        let nested = projects.join("app").join("-tmp-project.bin");
        write_project_cache_file(&nested);
        let interrupted_write = projects.join(".tmpAbC123");
        write_project_cache_file(&interrupted_write);
        let other_temp = projects.join(".tmpNotes");
        std::fs::write(&other_temp, "not a cache file").unwrap();

        remove_project_cache(base.path(), None);

        assert!(!cache_file.exists());
        assert!(!interrupted_write.exists());
        assert!(other_temp.exists());
        assert!(notes.exists());
        assert!(other_bin.exists());
        assert!(nested.exists(), "the removal does not descend");
    }

    fn transcript_in_project_of(directory: &Path) -> PathBuf {
        projects_dir_of(directory)
            .unwrap()
            .join("12345678-1234-4234-9234-123456789abc.jsonl")
    }

    #[test]
    fn resume_action_uses_cwd_when_it_maps_to_selected_project_dir() {
        let cwd = tempfile::tempdir().unwrap();
        let stale_project = tempfile::tempdir().unwrap();

        let action = resolve_resume_action(
            &transcript_in_project_of(cwd.path()),
            Some(stale_project.path()),
            cwd.path(),
            false,
        )
        .unwrap();

        assert_eq!(
            action,
            ResumeAction::Run {
                current_dir: cwd.path().to_path_buf()
            }
        );
    }

    #[test]
    fn resume_action_uses_project_path_when_it_maps_to_selected_project_dir() {
        let cwd = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();

        let action = resolve_resume_action(
            &transcript_in_project_of(project.path()),
            Some(project.path()),
            cwd.path(),
            false,
        )
        .unwrap();

        assert_eq!(
            action,
            ResumeAction::Run {
                current_dir: project.path().to_path_buf()
            }
        );
    }

    #[test]
    fn resume_action_copies_selected_transcript_when_project_path_maps_elsewhere() {
        let cwd = tempfile::tempdir().unwrap();
        let selected_project = tempfile::tempdir().unwrap();
        let stale_project = tempfile::tempdir().unwrap();

        let action = resolve_resume_action(
            &transcript_in_project_of(selected_project.path()),
            Some(stale_project.path()),
            cwd.path(),
            false,
        )
        .unwrap();

        assert_eq!(
            action,
            ResumeAction::CopyToCurrent {
                cwd_projects_dir: projects_dir_of(cwd.path()).unwrap()
            }
        );
    }
}
