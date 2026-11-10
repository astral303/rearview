//! Grok Build sessions: one directory per session under
//! `~/.grok/sessions/<encoded-cwd>/<session-id>/`, holding `summary.json` and
//! the `updates.jsonl` transcript.
//!
//! A sub-agent is a full session and does not name its parent. Discovery
//! reads the parent's `subagents/<id>/meta.json` to attach each sub-agent to
//! its parent.

use super::walk::{self, DirectoryLister, SessionFiles, SystemLister};
use super::{
    Capability, Deleted, DiscoveredSessions, Fingerprint, RefNamespaces, ResolvedSession,
    RootEnvironment, SessionCache, SessionDeleter, SessionForker, SessionIdResolver, SessionLaunch,
    SessionProvider, SessionRenamer, SessionResumer, SessionRoot, SessionStorage, SessionStub,
    SessionTitle, SessionsDirLookup, SourceLabels, SubagentFinder, resolved_session_paths,
    write_atomically,
};
use crate::cli::DebugLevel;
use crate::clock::Clock;
use crate::error::{AppError, Result};
use crate::history::format::grok::{self, SUMMARY_FILE, UPDATES_FILE};
use crate::history::format::{self, SessionFormat};
use crate::history::{Conversation, Source, parser};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Moves Grok's whole home, `~/.grok` by default.
const HOME_VARIABLE: &str = "GROK_HOME";

/// Grok's search index over sessions, beside the cwd directories.
const SEARCH_INDEX_FILE: &str = "session_search.sqlite";

/// The title-refresh watermark Grok reads before refreshing an auto title.
const TITLE_REFRESH_WATERMARK_FILE: &str = "title_refresh_idx";

/// The value marking every title-refresh checkpoint consumed. Writing it
/// freezes Grok's auto title so it cannot overwrite a rename. Grok's own
/// rename writes `TITLE_REFRESH_TURNS.len()` (`xai-grok-shell`,
/// `session_summary.rs`), `[3, 6]` as of 1.0.45, and clamps a larger value
/// to the same meaning.
const TITLE_REFRESH_CHECKPOINTS: usize = 2;

/// The longest a delete waits for Grok to release its search index; past
/// that, the session's row stays in the index and the delete still succeeds.
const SEARCH_INDEX_BUSY_TIMEOUT: Duration = Duration::from_millis(250);

pub struct GrokProvider;

impl SessionProvider for GrokProvider {
    fn source(&self) -> Source {
        Source::Grok
    }

    fn labels(&self) -> SourceLabels {
        SourceLabels {
            name: "grok",
            list: "GROK",
            display: "Grok",
        }
    }

    fn ref_namespaces(&self) -> RefNamespaces {
        RefNamespaces {
            conversation: "agent-grok-v1",
            project: "agent-grok-project-v1",
        }
    }

    fn storage(&self) -> &dyn SessionStorage {
        &GrokStorage
    }

    fn storage_env_vars(&self) -> &'static [&'static str] {
        &[HOME_VARIABLE]
    }

    fn format(&self) -> &dyn SessionFormat {
        &grok::GROK_UPDATES
    }

    fn resume(&self) -> Capability<&dyn SessionResumer> {
        Capability::Capable(&GrokLauncher)
    }

    fn fork(&self) -> Capability<&dyn SessionForker> {
        Capability::Capable(&GrokLauncher)
    }

    fn rename(&self) -> Capability<&dyn SessionRenamer> {
        Capability::Capable(self)
    }

    fn delete(&self) -> Capability<&dyn SessionDeleter> {
        Capability::Capable(self)
    }

    fn subagents(&self) -> Capability<&dyn SubagentFinder> {
        Capability::Capable(self)
    }

    fn subagent_by_id(&self) -> Capability {
        Capability::Capable(())
    }

    fn session_id_lookup(&self) -> Capability<&dyn SessionIdResolver> {
        Capability::Capable(self)
    }

    fn find_sessions_by_id(&self, session_id: &str) -> Result<Vec<PathBuf>> {
        resolved_session_paths(self, session_id)
    }

    fn workspace_sessions_dir(&self) -> Capability<SessionsDirLookup> {
        Capability::Capable(sessions_dir_of)
    }

    fn current_session_variable(&self) -> Capability<&'static str> {
        Capability::NotCapable(None)
    }
}

impl SessionRenamer for GrokProvider {
    /// Set the manual title in `summary.json` and freeze Grok's auto title.
    fn rename_session(&self, path: &Path, title: &str, _renamed_at: DateTime<Utc>) -> Result<()> {
        format::require_owned_transcript(Source::Grok, path)?;
        let session_dir = session_dir_of(path)?;
        rewrite_summary_title(&session_dir.join(SUMMARY_FILE), title)?;
        write_atomically(
            &session_dir.join(TITLE_REFRESH_WATERMARK_FILE),
            TITLE_REFRESH_CHECKPOINTS.to_string().as_bytes(),
        )
    }
}

impl SessionDeleter for GrokProvider {
    /// Remove the session's sub-agent sessions with it, then their rows in
    /// Grok's search index. A sub-agent left behind lists as a session of its
    /// own, because the parent's `subagents/` is the only record that it is a
    /// thread. `grok sessions delete` removes only the session.
    fn delete_session(&self, path: &Path) -> Result<Deleted> {
        format::require_owned_transcript(Source::Grok, path)?;
        let session_dir = session_dir_of(path)?;
        let sessions_root = sessions_root_of(&session_dir);
        let subagent_dirs = match sessions_root {
            Some(root) => {
                let index = SessionDirectories::probing(&SystemLister, root);
                subagent_session_dirs(&SystemLister, &session_dir, &|id| index.locate(id))
            }
            None => Vec::new(),
        };
        for directory in removal_order(&session_dir, &subagent_dirs) {
            std::fs::remove_dir_all(directory)?;
        }
        if let Some(root) = sessions_root {
            let removed: Vec<String> = std::iter::once(&session_dir)
                .chain(&subagent_dirs)
                .filter_map(|directory| directory_name(directory))
                .collect();
            evict_from_search_index(&root.join(SEARCH_INDEX_FILE), &removed);
        }
        Ok(Deleted {
            subagent_sessions: subagent_dirs.len(),
            ..Deleted::just_the_session()
        })
    }
}

impl SubagentFinder for GrokProvider {
    /// The transcripts of the sub-agent sessions the locator's session
    /// directory names, found in the cwd directories beside it. A locator
    /// outside a Grok session directory has none.
    fn subagent_transcripts(&self, locator: &Path) -> Result<Vec<PathBuf>> {
        let Ok(session_dir) = session_dir_of(locator) else {
            return Ok(Vec::new());
        };
        let Some(sessions_root) = sessions_root_of(&session_dir) else {
            return Ok(Vec::new());
        };
        let index = SessionDirectories::probing(&SystemLister, sessions_root);
        let locate = |id: &str| index.locate(id);
        Ok(subagent_transcripts_of(
            &SystemLister,
            &session_dir,
            &locate,
        ))
    }
}

impl SessionIdResolver for GrokProvider {
    fn is_session_id_shape(&self, query: &str) -> bool {
        crate::search::is_uuid(query)
    }

    /// The session directory the id names, in whichever cwd directory holds
    /// it, with its sub-agents found the same way. A sub-agent's id resolves
    /// to its own session.
    fn resolve_session_id(&self, session_id: &str) -> Result<Option<ResolvedSession>> {
        if !self.is_session_id_shape(session_id) {
            return Ok(None);
        }
        for root in GrokStorage.roots()? {
            if let Some(stub) = session_stub_of(&SystemLister, &root, session_id) {
                return Ok(Some(ResolvedSession { root, stub }));
            }
        }
        Ok(None)
    }
}

/// The stub of `session_id` under `root`, probing the cwd directories for it
/// and for each sub-agent it names, listed through `lister`.
fn session_stub_of(
    lister: &dyn DirectoryLister,
    root: &SessionRoot,
    session_id: &str,
) -> Option<SessionStub> {
    let index = SessionDirectories::probing(lister, &root.path);
    let directory = index.locate(&crate::search::session_id_for_lookup(session_id))?;
    session_stub(lister, root, &directory, &|id| index.locate(id))
}

/// The session directory a locator sits in, when it is a Grok session
/// directory: named by a session id.
fn session_dir_of(path: &Path) -> Result<PathBuf> {
    path.parent()
        .filter(|directory| {
            directory_name(directory).is_some_and(|name| crate::search::is_uuid(&name))
        })
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            AppError::ConfigError(format!(
                "{} is not inside a Grok session directory",
                path.display()
            ))
        })
}

/// The sessions root two levels above a session directory, past its cwd
/// directory.
fn sessions_root_of(session_dir: &Path) -> Option<&Path> {
    session_dir.parent().and_then(Path::parent)
}

fn directory_name(directory: &Path) -> Option<String> {
    directory
        .file_name()
        .and_then(OsStr::to_str)
        .map(str::to_owned)
}

/// The directories a delete removes, in order: each nested sub-agent before
/// the sub-agent naming it, and the session last. A removal that fails
/// partway leaves the remaining sub-agents attached to the directory still
/// naming them. `subagent_dirs` lists each parent before its nested ones.
fn removal_order<'a>(session_dir: &'a Path, subagent_dirs: &'a [PathBuf]) -> Vec<&'a Path> {
    subagent_dirs
        .iter()
        .rev()
        .map(PathBuf::as_path)
        .chain(std::iter::once(session_dir))
        .collect()
}

/// Set the title in `summary.json`, keeping every field rearview does not
/// read.
fn rewrite_summary_title(summary_path: &Path, title: &str) -> Result<()> {
    let contents = std::fs::read_to_string(summary_path).map_err(|error| {
        AppError::Io(std::io::Error::new(
            error.kind(),
            format!("{}: {error}", summary_path.display()),
        ))
    })?;
    let mut value: Value = serde_json::from_str(&contents)?;
    let object = value.as_object_mut().ok_or_else(|| {
        AppError::ConfigError(format!(
            "{} does not hold a JSON object",
            summary_path.display()
        ))
    })?;
    let cleaned = title.replace(['\r', '\n'], " ").trim().to_owned();
    object.insert("generated_title".to_owned(), json!(cleaned));
    object.insert("title_is_manual".to_owned(), json!(true));
    write_atomically(summary_path, serde_json::to_string(&value)?.as_bytes())
}

/// Drop each removed session's row from Grok's search index; its FTS
/// triggers update the index. Best-effort: a missing, unreadable or locked
/// index is nothing to prune and must not fail the delete.
fn evict_from_search_index(index: &Path, session_ids: &[String]) {
    if !index.is_file() {
        return;
    }
    let Ok(connection) =
        rusqlite::Connection::open_with_flags(index, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
    else {
        return;
    };
    if connection.busy_timeout(SEARCH_INDEX_BUSY_TIMEOUT).is_err() {
        return;
    }
    for session_id in session_ids {
        let _ = connection.execute(
            "DELETE FROM session_docs WHERE session_id = ?1",
            [session_id],
        );
    }
}

/// The session directories under a sessions root, by session id: the
/// directories two levels down that hold a `summary.json`.
struct SessionDirectories<'a> {
    /// Lists every directory a walk or a probe reads.
    lister: &'a dyn DirectoryLister,
    sessions_root: PathBuf,
    /// Every session directory, sorted, when the whole tree was walked;
    /// `None` for a lookup that probes the cwd directories instead.
    walked: Option<Vec<PathBuf>>,
    /// The first directory holding each id, for a walked tree.
    by_id: HashMap<String, PathBuf>,
}

impl<'a> SessionDirectories<'a> {
    /// Every session directory under `sessions_root`. A missing root returns
    /// none rather than an error: the user has not installed Grok.
    fn walk(lister: &'a dyn DirectoryLister, sessions_root: &Path) -> Result<Self> {
        let mut directories = Vec::new();
        if sessions_root.exists() {
            for cwd_dir in walk::subdirectories(lister, sessions_root)? {
                directories.extend(
                    walk::subdirectories(lister, &cwd_dir)?
                        .into_iter()
                        .filter(|directory| directory.join(SUMMARY_FILE).is_file()),
                );
            }
        }
        directories.sort();
        let mut by_id = HashMap::new();
        for directory in &directories {
            if let Some(name) = directory_name(directory) {
                by_id.entry(name).or_insert_with(|| directory.clone());
            }
        }
        Ok(Self {
            lister,
            sessions_root: sessions_root.to_path_buf(),
            walked: Some(directories),
            by_id,
        })
    }

    /// A lookup that probes `<sessions_root>/*/<id>/` for each id asked.
    fn probing(lister: &'a dyn DirectoryLister, sessions_root: &Path) -> Self {
        Self {
            lister,
            sessions_root: sessions_root.to_path_buf(),
            walked: None,
            by_id: HashMap::new(),
        }
    }

    /// The directory of session `id`. Grok writes ids in lowercase. When two
    /// cwd directories hold the id, return the first; Grok reports such an id
    /// as ambiguous.
    fn locate(&self, id: &str) -> Option<PathBuf> {
        if self.walked.is_some() {
            return self.by_id.get(id).cloned();
        }
        let mut cwd_dirs = walk::subdirectories(self.lister, &self.sessions_root).ok()?;
        cwd_dirs.sort();
        cwd_dirs
            .into_iter()
            .map(|cwd_dir| cwd_dir.join(id))
            .find(|directory| directory.join(SUMMARY_FILE).is_file())
    }
}

/// The `child_session_id` of each `subagents/*/meta.json` in `session_dir`,
/// listed through `lister`, in directory order. A `meta.json` that cannot be
/// read is skipped.
fn child_session_ids(lister: &dyn DirectoryLister, session_dir: &Path) -> Vec<String> {
    let Ok(mut entries) = walk::subdirectories(lister, &session_dir.join("subagents")) else {
        return Vec::new();
    };
    entries.sort();
    entries
        .into_iter()
        .filter_map(|entry| {
            let meta = std::fs::read_to_string(entry.join("meta.json")).ok()?;
            let meta = serde_json::from_str::<Value>(&meta).ok()?;
            meta.get("child_session_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        })
        .collect()
}

/// Every sub-agent session directory `session_dir` names, nested ones
/// flattened after their parent, each once. A missing sub-agent directory is
/// skipped.
fn subagent_session_dirs(
    lister: &dyn DirectoryLister,
    session_dir: &Path,
    locate: &dyn Fn(&str) -> Option<PathBuf>,
) -> Vec<PathBuf> {
    fn collect(
        lister: &dyn DirectoryLister,
        session_dir: &Path,
        locate: &dyn Fn(&str) -> Option<PathBuf>,
        seen: &mut HashSet<String>,
        found: &mut Vec<PathBuf>,
    ) {
        for child in child_session_ids(lister, session_dir) {
            if !seen.insert(child.clone()) {
                continue;
            }
            let Some(child_dir) = locate(&child) else {
                continue;
            };
            found.push(child_dir.clone());
            collect(lister, &child_dir, locate, seen, found);
        }
    }
    let mut seen = directory_name(session_dir).into_iter().collect();
    let mut found = Vec::new();
    collect(lister, session_dir, locate, &mut seen, &mut found);
    found
}

/// The transcript of every sub-agent session `session_dir` names, nested ones
/// flattened after their parent.
fn subagent_transcripts_of(
    lister: &dyn DirectoryLister,
    session_dir: &Path,
    locate: &dyn Fn(&str) -> Option<PathBuf>,
) -> Vec<PathBuf> {
    subagent_session_dirs(lister, session_dir, locate)
        .into_iter()
        .map(|directory| directory.join(UPDATES_FILE))
        .collect()
}

/// The stub of the session in `session_dir`: its transcript, with every
/// sub-agent transcript it names. A session closed before its first prompt
/// has no transcript; its summary supplies the fingerprint, and the session
/// reads as empty.
fn session_stub(
    lister: &dyn DirectoryLister,
    root: &SessionRoot,
    session_dir: &Path,
    locate: &dyn Fn(&str) -> Option<PathBuf>,
) -> Option<SessionStub> {
    let transcript = session_dir.join(UPDATES_FILE);
    let subagents = subagent_transcripts_of(lister, session_dir, locate);
    if let Some(stub) = walk::session_stubs(
        root,
        vec![SessionFiles {
            transcript: transcript.clone(),
            subagents,
        }],
    )
    .pop()
    {
        return Some(stub);
    }
    let summary = std::fs::metadata(session_dir.join(SUMMARY_FILE)).ok()?;
    Some(SessionStub {
        cache_key: transcript
            .strip_prefix(&root.path)
            .unwrap_or(&transcript)
            .to_string_lossy()
            .into_owned(),
        locator: transcript,
        subagents: Vec::new(),
        fingerprint: Fingerprint {
            size: summary.len(),
            modified: summary.modified().ok(),
        },
    })
}

struct GrokStorage;

impl SessionStorage for GrokStorage {
    fn source(&self) -> Source {
        Source::Grok
    }

    fn cache(&self) -> SessionCache {
        SessionCache {
            directory: "grok",
            magic: *b"GKHIST01",
            schema_version: 10,
        }
    }

    fn roots_in(&self, environment: &RootEnvironment) -> Result<Vec<SessionRoot>> {
        Ok(vec![sessions_root_in(environment)?])
    }

    /// Every session directory is a session, unless some session's
    /// `subagents/` names it: then its transcript is that session's
    /// sub-agent transcript.
    fn discover(
        &self,
        root: &SessionRoot,
        lister: &dyn DirectoryLister,
    ) -> Result<DiscoveredSessions> {
        let directories = SessionDirectories::walk(lister, &root.path)?;
        let walked = directories.walked.as_deref().unwrap_or_default();
        let children: HashSet<String> = walked
            .iter()
            .flat_map(|directory| child_session_ids(lister, directory))
            .collect();
        let stubs = walked
            .iter()
            .filter(|directory| {
                directory_name(directory).is_none_or(|name| !children.contains(&name))
            })
            .filter_map(|directory| {
                session_stub(lister, root, directory, &|id| directories.locate(id))
            })
            .collect();
        Ok(DiscoveredSessions::complete(stubs))
    }

    /// A sub-agent opened by its ID: discovery lists its transcript under the
    /// session that names it, not as a stub of its own.
    fn rediscover_unlisted_session(
        &self,
        root: &SessionRoot,
        locator: &Path,
        lister: &dyn DirectoryLister,
    ) -> Result<Option<SessionStub>> {
        let session_dir = session_dir_of(locator)?;
        Ok(directory_name(&session_dir)
            .and_then(|session_id| session_stub_of(lister, root, &session_id)))
    }

    fn parse_session(
        &self,
        stub: &SessionStub,
        _root: &SessionRoot,
        debug_level: Option<DebugLevel>,
        on_transcript_read: &(dyn Fn() + Sync),
        clock: &dyn Clock,
    ) -> Result<Option<Conversation>> {
        parser::process_session_file(
            stub,
            &grok::GROK_UPDATES,
            debug_level,
            on_transcript_read,
            clock,
        )
    }

    /// Read each session's title from its `summary.json`. A rename in Grok or
    /// in rearview changes that file, not the transcript the cache validates
    /// against.
    fn external_titles(&self, root: &SessionRoot) -> HashMap<String, SessionTitle> {
        let Ok(directories) = SessionDirectories::walk(&SystemLister, &root.path) else {
            return HashMap::new();
        };
        directories
            .walked
            .unwrap_or_default()
            .iter()
            .filter_map(|directory| {
                let summary = grok::session_summary(directory)?;
                let title = summary.title()?;
                Some((summary.id, title))
            })
            .collect()
    }
}

fn sessions_root_in(environment: &RootEnvironment) -> Result<SessionRoot> {
    Ok(sessions_root_from(
        environment.var(HOME_VARIABLE).as_deref(),
        environment.home().ok_or_else(AppError::home_not_found)?,
    ))
}

/// `GROK_HOME` moves the whole home when it is set and not empty, as it does
/// for Grok itself.
fn sessions_root_from(grok_home: Option<&str>, home: &Path) -> SessionRoot {
    let base = grok_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".grok"));
    SessionRoot::new(base.join("sessions")).in_agent_tree()
}

/// The cwd directory Grok stores the sessions recorded in `directory` in.
fn sessions_dir_of(environment: &RootEnvironment, directory: &Path) -> Result<PathBuf> {
    Ok(sessions_root_in(environment)?
        .path
        .join(cwd_directory_name(&directory.to_string_lossy())))
}

/// The longest cwd directory name Grok writes URL-encoded.
const MAX_ENCODED_CWD_BYTES: usize = 255;
/// The longest slug in a long cwd's directory name.
const MAX_SLUG_CHARS: usize = 40;
/// The count of hex digits of the cwd's BLAKE3 hash in a long cwd's
/// directory name.
const CWD_HASH_HEX_DIGITS: usize = 16;
/// The slug of a long cwd when its last component yields none.
const FALLBACK_SLUG: &str = "workspace";

/// Grok's name for the directory holding the sessions recorded in `cwd`:
/// `cwd` URL-encoded, or for a long one, a slug of its last component and
/// a hash (`encode_cwd_dirname` in Grok's `xai-grok-config`).
fn cwd_directory_name(cwd: &str) -> String {
    let encoded = url_encoded(cwd);
    if encoded.len() <= MAX_ENCODED_CWD_BYTES {
        return encoded;
    }
    let slug = Path::new(cwd)
        .file_name()
        .and_then(OsStr::to_str)
        .map(slug_of)
        .filter(|slug| !slug.is_empty())
        .unwrap_or_else(|| FALLBACK_SLUG.to_owned());
    let hash = blake3::hash(cwd.as_bytes()).to_hex();
    format!("{slug}-{}", &hash[..CWD_HASH_HEX_DIGITS])
}

/// Every byte but an ASCII letter, digit, `-`, `.`, `_` or `~` as `%XX`, in
/// uppercase hex.
fn url_encoded(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// `name` lowercased, each run of characters other than an ASCII letter or
/// digit as one `-`, without a leading or trailing `-`, then truncated.
fn slug_of(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    for character in name.to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-')
        .chars()
        .take(MAX_SLUG_CHARS)
        .collect()
}

/// Grok resumes and forks by session id, the session directory's name.
struct GrokLauncher;

impl SessionResumer for GrokLauncher {
    fn resume_command(&self, launch: &SessionLaunch) -> Result<std::process::Command> {
        let mut command = std::process::Command::new("grok");
        command.arg("--resume").arg(session_id_of(launch.path)?);
        if let Some(project_path) = launch.project_path {
            command.current_dir(project_path);
        }
        Ok(command)
    }
}

impl SessionForker for GrokLauncher {
    /// A fork runs where the user is now: the branch belongs to the current
    /// project.
    fn fork_command(&self, launch: &SessionLaunch) -> Result<std::process::Command> {
        let mut command = std::process::Command::new("grok");
        command
            .arg("--resume")
            .arg(session_id_of(launch.path)?)
            .arg("--fork-session");
        Ok(command)
    }
}

fn session_id_of(path: &Path) -> Result<String> {
    directory_name(&session_dir_of(path)?).ok_or_else(|| {
        AppError::ConfigError(format!(
            "{} is not inside a Grok session directory",
            path.display()
        ))
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::history::format::grok::fixture::{
        agent_chunk, call, reply, summary, turn_completed, user,
        write_session as write_grok_session, write_subagent_meta,
    };
    use crate::history::provider::RootOrigin;
    use crate::history::provider::contract_tests::{
        AgentSession, ExpectedCommand, FixtureIds, IdCase, Marker, ProviderFixture, copy_tree,
        load_under,
    };
    use std::ffi::{OsStr as StdOsStr, OsString};

    const SESSION: &str = "019a0000-0000-7000-8000-000000000001";
    const OTHER_SESSION: &str = "019a0000-0000-7000-8000-000000000002";
    const CHILD: &str = "019a0000-0000-7000-8000-0000000000c1";
    const NESTED: &str = "019a0000-0000-7000-8000-0000000000c2";
    /// The working directory the fixture's sessions are recorded in.
    const PROJECT: &str = "/tmp/grok-project";
    /// The cwd directory Grok names for [`PROJECT`].
    const PROJECT_DIR: &str = "%2Ftmp%2Fgrok-project";

    fn sessions_under(home: &Path) -> PathBuf {
        home.join("sessions")
    }

    fn session_dir_under(home: &Path, id: &str) -> PathBuf {
        sessions_under(home).join(PROJECT_DIR).join(id)
    }

    /// A listable session with a reply, returning its locator.
    fn write_session(home: &Path, id: &str) -> PathBuf {
        write_grok_session(
            &sessions_under(home),
            PROJECT_DIR,
            id,
            &summary(id, PROJECT, json!({"generated_title": "grok title"})),
            &[
                user(1_000, "grok question", 0),
                reply(2_000, "grok answer", "p1"),
            ],
        )
    }

    /// A sub-agent session `child` of `parent`, in `cwd_dir`, named by the
    /// parent's `subagents/`; returns its locator.
    fn write_subagent(home: &Path, parent: &str, child: &str, cwd_dir: &str) -> PathBuf {
        let locator = write_grok_session(
            &sessions_under(home),
            cwd_dir,
            child,
            &summary(
                child,
                "/tmp/grok-project",
                json!({"session_kind": "subagent", "agent_name": "explore"}),
            ),
            &[
                user(1_020, "the spawn prompt", 0),
                reply(1_050, "grok child answer searchable", "c"),
                turn_completed(1_060, "grok-4.6", 40, 8),
            ],
        );
        let parent_dir = GrokFixtureIndex::locate(home, parent);
        write_subagent_meta(&parent_dir, child, "explore");
        locator
    }

    /// The directory of `id` wherever a fixture wrote it.
    struct GrokFixtureIndex;

    impl GrokFixtureIndex {
        fn locate(home: &Path, id: &str) -> PathBuf {
            SessionDirectories::probing(&SystemLister, &sessions_under(home))
                .locate(id)
                .unwrap()
        }
    }

    /// Grok's sessions for the provider contracts in `contract_tests`.
    pub(crate) struct GrokFixture;

    impl ProviderFixture for GrokFixture {
        fn provider(&self) -> &'static dyn SessionProvider {
            Source::Grok.provider()
        }

        fn ids(&self) -> FixtureIds {
            FixtureIds {
                session: SESSION,
                other: OTHER_SESSION,
                unknown: "019a0000-0000-7000-8000-0000000000ff",
                session_in_other_case: "019A0000-0000-7000-8000-000000000001",
            }
        }

        fn id_case(&self) -> IdCase {
            IdCase::Insensitive
        }

        fn root_under(&self, home: &Path) -> SessionRoot {
            SessionRoot::new(sessions_under(home)).in_agent_tree()
        }

        fn write_session(&self, home: &Path, session: &str) -> PathBuf {
            write_session(home, session)
        }

        fn session_working_dir(&self) -> PathBuf {
            PathBuf::from(PROJECT)
        }

        fn root_variables(&self, home: &Path) -> Vec<(&'static str, OsString)> {
            vec![(HOME_VARIABLE, home.into())]
        }

        /// Built for the test, because Grok keeps the session and each
        /// sub-agent in directories of their own.
        fn every_record_session(&self) -> AgentSession {
            let storage = tempfile::tempdir().unwrap();
            let sessions = sessions_under(storage.path());
            let mut records = vec![
                user(1_000, "active grok question", 0),
                agent_chunk(
                    1_100,
                    "agent_thought_chunk",
                    "GROK_THINKING_VISIBLE",
                    "p1",
                    1_100,
                ),
                reply(1_200, "grok answer searchable", "p1"),
            ];
            records.extend(call(
                1_300,
                "read-1",
                "read_file",
                "read",
                json!({"target_file": "/tmp/grok-project/GROK_TOOL_INPUT_VISIBLE.rs"}),
                "completed",
                json!({"type": "ReadFile", "FileContent": {"content": "grok tool output searchable"}}),
            ));
            records.extend(call(
                1_400,
                "list-1",
                "list_dir",
                "list",
                json!({"target_directory": "/tmp/grok-project/GROK_LIST_DIR_SENTINEL"}),
                "completed",
                json!({"type": "ListDir", "Content": {"content": "a.rs\nb.rs"}}),
            ));
            records.extend(call(
                1_500,
                "shell-1",
                "run_terminal_command",
                "execute",
                json!({"command": "cat notes.txt"}),
                "completed",
                json!({"type": "Bash", "output_for_prompt": "", "exit_code": 0}),
            ));
            records.push(reply(
                1_600,
                "NUMBERED_REPLY_SENTINEL: the command printed nothing.",
                "p1",
            ));
            records.extend(call(
                1_700,
                "spawn-1",
                "spawn_subagent",
                "task",
                json!({
                    "description": "List the release steps",
                    "prompt": "LAUNCH_PROMPT_SENTINEL: list the release steps.",
                    "subagent_type": "explore",
                }),
                "completed",
                json!({"type": "Text", "text": "Subagent started in background."}),
            ));
            let session = write_grok_session(
                &sessions,
                PROJECT_DIR,
                SESSION,
                &summary(SESSION, "/tmp/grok-project", json!({})),
                &records,
            );
            let child = write_subagent(storage.path(), SESSION, CHILD, PROJECT_DIR);
            const CALLS: &str = "Listed 1 directory, read 1 file, ran 1 shell command";
            AgentSession::new(
                Source::Grok,
                session,
                vec![child],
                vec![
                    Marker::searched("user text", "active grok question"),
                    Marker::searched("assistant text", "grok answer searchable"),
                    Marker::shown("thinking", "GROK_THINKING_VISIBLE"),
                    Marker::agent_searched("tool call", "GROK_TOOL_INPUT_VISIBLE")
                        .in_summary_row(CALLS),
                    Marker::searched("tool result", "grok tool output searchable")
                        .in_summary_row(CALLS),
                    Marker::agent_searched("sub-agent message", "grok child answer searchable"),
                    Marker::agent_searched("directory listing", "GROK_LIST_DIR_SENTINEL")
                        .in_summary_row(CALLS),
                    Marker::agent_searched("launch prompt", "LAUNCH_PROMPT_SENTINEL")
                        .in_summary_row("Started 1 agent"),
                ],
            )
            .with_reply_after_records_without_text("NUMBERED_REPLY_SENTINEL", 12)
            // The sub-agent's turn: 40 input and 8 output.
            .with_subagent_tokens(48)
            .kept_in(storage)
        }

        /// The fixture is built in a sessions directory of its own: the
        /// session's and the sub-agent's directories, and the meta naming
        /// the sub-agent.
        fn write_every_record_session(&self, home: &Path, session: &AgentSession) {
            let built_in = session
                .row
                .path
                .ancestors()
                .find(|directory| directory.ends_with("sessions"))
                .unwrap();
            copy_tree(built_in, &sessions_under(home));
        }

        fn write_subagent(&self, home: &Path, session: &str) -> PathBuf {
            write_subagent(home, session, CHILD, PROJECT_DIR)
        }

        /// A nested sub-agent writes its own `subagents/` under its own
        /// session directory.
        fn write_nested_subagent(&self, home: &Path, _session: &str) -> PathBuf {
            write_subagent(home, CHILD, NESTED, PROJECT_DIR)
        }

        /// A sub-agent is a session of its own, named by its id.
        fn subagent_id(&self, _session: &str) -> String {
            CHILD.to_owned()
        }

        fn resolve_under(&self, home: &Path, id: &str) -> Result<Option<ResolvedSession>> {
            if !GrokProvider.is_session_id_shape(id) {
                return Ok(None);
            }
            let root = self.root_under(home);
            Ok(
                session_stub_of(&SystemLister, &root, id)
                    .map(|stub| ResolvedSession { root, stub }),
            )
        }

        fn expected_fork(&self, _locator: &Path) -> ExpectedCommand {
            ExpectedCommand {
                program: "grok",
                arguments: ["--resume", SESSION, "--fork-session"]
                    .map(OsString::from)
                    .into(),
                current_dir: None,
            }
        }

        fn roots_from(&self, override_dir: Option<&str>, home: &Path) -> Vec<PathBuf> {
            vec![sessions_root_from(override_dir, home).path]
        }

        fn foreign_transcript(&self, directory: &Path) -> PathBuf {
            let path = directory.join(UPDATES_FILE);
            std::fs::copy(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/pi/v3-branched.jsonl"),
                &path,
            )
            .unwrap();
            path
        }
    }

    fn discovered(home: &Path) -> Vec<SessionStub> {
        GrokStorage
            .discover(&SessionRoot::new(sessions_under(home)), &SystemLister)
            .unwrap()
            .stubs
    }

    #[test]
    fn sessions_root_is_under_grok_home_or_its_override() {
        let home = Path::new("/home/user");
        let default = sessions_root_from(None, home);
        assert_eq!(default.path, home.join(".grok/sessions"));
        assert_eq!(default.origin(), RootOrigin::AgentTree);
        assert_eq!(
            sessions_root_from(Some("/opt/grok"), home).path,
            Path::new("/opt/grok").join("sessions")
        );
    }

    /// The first two names are literals in Grok's own tests; the rest are
    /// encoded by hand.
    #[test]
    fn cwd_directory_is_url_encoded_cwd() {
        for (cwd, name) in [
            ("/Users/alice/code/xai", "%2FUsers%2Falice%2Fcode%2Fxai"),
            ("/tmp/.tmprace/repo", "%2Ftmp%2F.tmprace%2Frepo"),
            (
                r"C:\Users\micro\my project~",
                "C%3A%5CUsers%5Cmicro%5Cmy%20project~",
            ),
            ("/srv/项目", "%2Fsrv%2F%E9%A1%B9%E7%9B%AE"),
        ] {
            assert_eq!(cwd_directory_name(cwd), name, "{cwd}");
        }
    }

    /// A cwd longer than 255 bytes once encoded is named by a slug of its
    /// last component and the start of its BLAKE3 hash. The hashes are the
    /// `blake3` crate's, which Grok uses.
    #[test]
    fn long_cwd_directory_is_slug_and_hash() {
        let parent = format!("/srv/{}", "a".repeat(250));
        assert_eq!(
            cwd_directory_name(&format!("{parent}/My Project!")),
            "my-project-e9c382357033127c"
        );
        assert_eq!(
            cwd_directory_name(&format!("{parent}/项目")),
            "workspace-b03cc38a61fc298a",
            "a last component without an ASCII letter or digit gets the `workspace` slug"
        );
    }

    /// Short working directories are URL-encoded; long ones are a slug, a
    /// hash and a `.cwd` file. Discovery reads neither name.
    #[test]
    fn discovery_walks_both_cwd_directory_encodings() {
        let home = tempfile::tempdir().unwrap();
        let encoded = write_session(home.path(), SESSION);
        let hashed_dir = "very-long-project-path-0123456789abcdef";
        let hashed = write_grok_session(
            &sessions_under(home.path()),
            hashed_dir,
            OTHER_SESSION,
            &summary(OTHER_SESSION, "/tmp/a/very/long/path", json!({})),
            &[user(1, "hello", 0), reply(2, "hi", "p")],
        );
        std::fs::write(
            sessions_under(home.path()).join(hashed_dir).join(".cwd"),
            "/tmp/a/very/long/path",
        )
        .unwrap();
        std::fs::write(
            sessions_under(home.path()).join(SEARCH_INDEX_FILE),
            "not a session",
        )
        .unwrap();

        let mut locators: Vec<PathBuf> = discovered(home.path())
            .into_iter()
            .map(|stub| stub.locator)
            .collect();
        locators.sort();
        let mut expected = vec![encoded, hashed];
        expected.sort();
        assert_eq!(locators, expected);
    }

    #[test]
    fn session_with_no_transcript_is_discovered_and_isnt_listed() {
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let closed_before_first_prompt = write_grok_session(
            &sessions_under(home.path()),
            PROJECT_DIR,
            SESSION,
            &summary(SESSION, "/tmp/grok-project", json!({})),
            &[],
        );

        let stubs = discovered(home.path());
        assert_eq!(stubs.len(), 1);
        assert_eq!(stubs[0].locator, closed_before_first_prompt);
        assert!(
            stubs[0].fingerprint.modified.is_some(),
            "the summary fingerprints it"
        );
        assert!(load_under(&GrokFixture, home.path(), cache.path()).is_empty());
    }

    #[test]
    fn fork_lists_as_session_though_its_summary_names_its_origin() {
        let home = tempfile::tempdir().unwrap();
        write_session(home.path(), SESSION);
        let fork = write_grok_session(
            &sessions_under(home.path()),
            PROJECT_DIR,
            OTHER_SESSION,
            &summary(
                OTHER_SESSION,
                "/tmp/grok-project",
                json!({"session_kind": "fork", "parent_session_id": SESSION}),
            ),
            &[user(1, "forked", 0), reply(2, "answer", "p")],
        );

        let stubs = discovered(home.path());
        assert_eq!(stubs.len(), 2);
        let fork_stub = stubs.iter().find(|stub| stub.locator == fork).unwrap();
        assert!(fork_stub.subagents.is_empty());
    }

    #[test]
    fn sub_agent_in_another_cwd_directory_attaches_to_its_session() {
        let home = tempfile::tempdir().unwrap();
        let session = write_session(home.path(), SESSION);
        let same = write_subagent(home.path(), SESSION, CHILD, PROJECT_DIR);
        let elsewhere = write_subagent(home.path(), SESSION, NESTED, "%2Ftmp%2Fworktree");

        let stubs = discovered(home.path());
        assert_eq!(stubs.len(), 1);
        assert_eq!(stubs[0].locator, session);
        let mut subagents = stubs[0].subagents.clone();
        subagents.sort();
        let mut expected = vec![same, elsewhere];
        expected.sort();
        assert_eq!(subagents, expected);
    }

    #[test]
    fn sub_agent_named_but_gone_leaves_its_session_listed() {
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_session(home.path(), SESSION);
        write_subagent_meta(&session_dir_under(home.path(), SESSION), CHILD, "explore");

        let stubs = discovered(home.path());
        assert_eq!(stubs.len(), 1);
        assert!(stubs[0].subagents.is_empty());
        assert_eq!(load_under(&GrokFixture, home.path(), cache.path()).len(), 1);
    }

    /// The spawn prompt appears in the parent's call input, the parent's
    /// `meta.json` and the sub-agent's first message; only the sub-agent's
    /// first message is read as text.
    #[test]
    fn spawn_prompt_reaches_sessions_text_once() {
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_session(home.path(), SESSION);
        write_subagent(home.path(), SESSION, CHILD, PROJECT_DIR);

        let listed = load_under(&GrokFixture, home.path(), cache.path());
        assert_eq!(listed.len(), 1);
        let text = format!("{} {}", listed[0].full_text, listed[0].agent_search_text);
        assert_eq!(text.matches("the spawn prompt").count(), 1, "{text}");
        assert!(
            listed[0]
                .agent_search_text
                .contains("grok child answer searchable")
        );
        assert!(!listed[0].full_text.contains("grok child answer searchable"));
    }

    #[test]
    fn spliced_sub_agent_thread_is_labelled_with_its_type() {
        let home = tempfile::tempdir().unwrap();
        let session = write_session(home.path(), SESSION);
        let child = write_subagent(home.path(), SESSION, CHILD, PROJECT_DIR);

        let entries = grok::GROK_UPDATES
            .session_entries(&session, &[child])
            .unwrap()
            .unwrap()
            .entries;
        let labels: Vec<String> = entries
            .iter()
            .filter_map(|(_, entry)| match entry {
                crate::log_entry::LogEntry::Progress { data, .. } => data
                    .get("agentId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                _ => None,
            })
            .collect();
        assert!(!labels.is_empty());
        assert!(labels.iter().all(|label| label == "explore"), "{labels:?}");
    }

    #[test]
    fn resume_uses_session_id() {
        let home = tempfile::tempdir().unwrap();
        let locator = write_session(home.path(), SESSION);
        let project = PathBuf::from("/tmp/grok-project");
        let launch = SessionLaunch {
            path: &locator,
            project_path: Some(&project),
            configured_args: &["--claude-only".to_owned()],
        };
        let Capability::Capable(resumer) = Source::Grok.provider().resume() else {
            panic!("Grok resumes");
        };

        let resume = resumer.resume_command(&launch).unwrap();
        assert_eq!(resume.get_program(), StdOsStr::new("grok"));
        assert_eq!(
            resume.get_args().collect::<Vec<_>>(),
            vec![StdOsStr::new("--resume"), StdOsStr::new(SESSION)]
        );
        assert_eq!(resume.get_current_dir(), Some(project.as_path()));
    }

    #[test]
    fn rename_rewrites_summary_freezes_auto_title_and_keeps_unknown_fields() {
        let home = tempfile::tempdir().unwrap();
        let locator = write_session(home.path(), SESSION);

        GrokProvider
            .rename_session(
                &locator,
                "renamed\nGrok",
                crate::clock::TestClock::start_wall(),
            )
            .unwrap();

        let session_dir = session_dir_under(home.path(), SESSION);
        let recorded: Value =
            serde_json::from_str(&std::fs::read_to_string(session_dir.join(SUMMARY_FILE)).unwrap())
                .unwrap();
        assert_eq!(recorded["generated_title"], "renamed Grok");
        assert_eq!(recorded["title_is_manual"], true);
        assert_eq!(
            recorded["unknown_field"]["kept"], true,
            "fields rearview does not read survive a rename"
        );
        assert_eq!(
            std::fs::read_to_string(session_dir.join(TITLE_REFRESH_WATERMARK_FILE)).unwrap(),
            "2"
        );
    }

    /// Grok's search index, with a row per session.
    fn write_search_index(home: &Path, session_ids: &[&str]) -> PathBuf {
        let index = sessions_under(home).join(SEARCH_INDEX_FILE);
        std::fs::create_dir_all(index.parent().unwrap()).unwrap();
        let connection = rusqlite::Connection::open(&index).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL, \
                 updated_at INTEGER NOT NULL, title TEXT NOT NULL, content TEXT NOT NULL, \
                 content_hash TEXT NOT NULL);",
            )
            .unwrap();
        for id in session_ids {
            connection
                .execute(
                    "INSERT INTO session_docs VALUES (?1, '/tmp', 0, 't', 'c', 'h')",
                    [id],
                )
                .unwrap();
        }
        index
    }

    fn indexed_ids(index: &Path) -> Vec<String> {
        let connection = rusqlite::Connection::open(index).unwrap();
        let mut statement = connection
            .prepare("SELECT session_id FROM session_docs ORDER BY session_id")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    #[test]
    fn delete_evicts_removed_sessions_from_grok_search_index() {
        let home = tempfile::tempdir().unwrap();
        let doomed = write_session(home.path(), SESSION);
        write_subagent(home.path(), SESSION, CHILD, PROJECT_DIR);
        write_session(home.path(), OTHER_SESSION);
        let index = write_search_index(home.path(), &[SESSION, CHILD, OTHER_SESSION]);

        GrokProvider.delete_session(&doomed).unwrap();

        assert_eq!(indexed_ids(&index), vec![OTHER_SESSION.to_owned()]);
    }

    #[test]
    fn delete_removes_nested_sub_agents_first_and_session_last() {
        let session = Path::new("/sessions/cwd/session");
        let child = PathBuf::from("/sessions/cwd/child");
        let nested = PathBuf::from("/sessions/cwd/nested");
        let second = PathBuf::from("/sessions/cwd/second");
        let subagent_dirs = [child.clone(), nested.clone(), second.clone()];

        assert_eq!(
            removal_order(session, &subagent_dirs),
            vec![second.as_path(), nested.as_path(), child.as_path(), session]
        );
    }

    #[test]
    fn delete_succeeds_while_search_index_is_locked() {
        let home = tempfile::tempdir().unwrap();
        let doomed = write_session(home.path(), SESSION);
        let index = write_search_index(home.path(), &[SESSION]);
        let holder = rusqlite::Connection::open(&index).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE;").unwrap();

        GrokProvider.delete_session(&doomed).unwrap();

        assert!(!session_dir_under(home.path(), SESSION).exists());
        holder.execute_batch("ROLLBACK;").unwrap();
        assert_eq!(indexed_ids(&index), vec![SESSION.to_owned()]);
    }

    #[test]
    fn id_that_is_not_uuid_doesnt_resolve() {
        assert_eq!(GrokProvider.resolve_session_id("../escape").unwrap(), None);
        assert_eq!(
            GrokProvider.resolve_session_id("updates.jsonl").unwrap(),
            None
        );
    }
}
