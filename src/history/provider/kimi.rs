//! Kimi Code sessions: one directory per session under
//! `~/.kimi-code/sessions/<workspace>/`, holding `state.json` and a
//! `wire.jsonl` per agent.

use super::walk::SessionFiles;
use super::{
    Capability, Deleted, DenialDetail, DiscoveredSessions, RefNamespaces, ResolvedSession,
    RootEnvironment, SessionCache, SessionDeleter, SessionForker, SessionIdResolver, SessionLaunch,
    SessionProvider, SessionRenamer, SessionResumer, SessionRoot, SessionStorage, SessionStub,
    SessionTitle, SessionsDirLookup, SourceLabels, SubagentFinder, resolved_session_paths,
    retain_index_records, walk, write_atomically,
};
use crate::cli::DebugLevel;
use crate::error::{AppError, Result};
use crate::history::format::{self, SessionFormat, kimi};
use crate::history::{Conversation, Source, parser};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Kimi names each session directory `session_<uuid>`.
const SESSION_DIRECTORY_PREFIX: &str = "session_";

pub struct KimiProvider;

/// Moves Kimi's whole home, in place of both default homes.
const HOME_VARIABLE: &str = "KIMI_CODE_HOME";

impl SessionProvider for KimiProvider {
    fn source(&self) -> Source {
        Source::Kimi
    }

    fn labels(&self) -> SourceLabels {
        SourceLabels {
            name: "kimi",
            list: "KIMI",
            display: "Kimi",
        }
    }

    fn ref_namespaces(&self) -> RefNamespaces {
        RefNamespaces {
            conversation: "agent-kimi-v1",
            project: "agent-kimi-project-v1",
        }
    }

    fn storage(&self) -> &dyn SessionStorage {
        &KimiStorage
    }

    fn storage_env_vars(&self) -> &'static [&'static str] {
        &[HOME_VARIABLE]
    }

    fn format(&self) -> &dyn SessionFormat {
        &kimi::KIMI_WIRE
    }

    fn resume(&self) -> Capability<&dyn SessionResumer> {
        Capability::Capable(&KimiLauncher)
    }

    /// Kimi's CLI cannot start a forked session: forking exists only as the
    /// `/fork` command inside a running session.
    fn fork(&self) -> Capability<&dyn SessionForker> {
        Capability::NotCapable(Some(DenialDetail::ResumeThenForkInside))
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
        Capability::NotCapable(None)
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

impl SessionRenamer for KimiProvider {
    /// A Kimi title lives in `state.json` beside the wires, so a rename
    /// rewrites that file in place and leaves the transcript untouched.
    fn rename_session(&self, path: &Path, title: &str) -> Result<()> {
        format::require_owned_transcript(Source::Kimi, path)?;
        let session_dir = owned_session_dir(path).ok_or_else(|| {
            AppError::ConfigError(format!(
                "{} is not inside a Kimi session directory",
                path.display()
            ))
        })?;
        rewrite_state_title(&session_dir.join("state.json"), title)
    }
}

impl SessionDeleter for KimiProvider {
    /// Delete removes the session's directory, with its state, wires and
    /// diagnostic logs, then its index record. In a directory without a main
    /// wire, delete removes only the chosen wire while another wire remains,
    /// because discovery lists each of those wires as a session.
    fn delete_session(&self, path: &Path) -> Result<Deleted> {
        format::require_owned_transcript(Source::Kimi, path)?;
        let Some(session_dir) = owned_session_dir(path) else {
            // A stray wire outside a session directory: whatever holds the
            // file is not Kimi's to remove.
            std::fs::remove_file(path)?;
            return Ok(Deleted::just_the_session());
        };
        let listing = session_directory(&session_dir);
        if let Ok(SessionDirectory::NoMainWire(wires)) = &listing
            && wires.iter().any(|wire| wire.as_path() != path)
        {
            remove_wire(&session_dir, path)?;
            return Ok(Deleted::just_the_session());
        }
        let session_id = session_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Count before removing: removing the directory deletes the
        // sub-agent wires. An unreadable directory is deleted with no
        // sub-agent count.
        let subagent_sessions = match listing {
            Ok(SessionDirectory::Session(files)) => files.subagents.len(),
            Ok(SessionDirectory::NoMainWire(_)) | Err(_) => 0,
        };
        std::fs::remove_dir_all(&session_dir)?;
        prune_index_records(&session_dir, &session_id)?;
        Ok(Deleted {
            subagent_sessions,
            ..Deleted::just_the_session()
        })
    }
}

impl SubagentFinder for KimiProvider {
    /// Every other agent's wire in the session directory of a main wire. A
    /// sub-agent's wire, and a wire in a directory without a main wire, have
    /// none.
    fn subagent_transcripts(&self, locator: &Path) -> Result<Vec<PathBuf>> {
        let session_dir = kimi::wire_location(locator).session_dir;
        Ok(match session_directory(&session_dir)? {
            SessionDirectory::Session(files) if format::same_file(&files.transcript, locator) => {
                files.subagents
            }
            SessionDirectory::Session(_) | SessionDirectory::NoMainWire(_) => Vec::new(),
        })
    }
}

impl SessionIdResolver for KimiProvider {
    fn is_session_id_shape(&self, query: &str) -> bool {
        is_session_directory_name(query)
    }

    /// A Kimi session id names the directory holding its wires. A sub-agent
    /// thread is named `<session>#<agent>` and is read through its parent, so
    /// it resolves to no wire of its own.
    fn resolve_session_id(&self, session_id: &str) -> Result<Option<ResolvedSession>> {
        if !self.is_session_id_shape(session_id) {
            return Ok(None);
        }
        for root in KimiStorage.roots()? {
            if let Some(stub) = session_stub_of(&root, session_id)? {
                return Ok(Some(ResolvedSession { root, stub }));
            }
        }
        Ok(None)
    }
}

/// The stub of `session_id`, in whichever workspace under `root` holds that
/// session: its main wire with the sub-agent wires beside it. A session
/// directory with no main wire is not the session the id names.
fn session_stub_of(root: &SessionRoot, session_id: &str) -> Result<Option<SessionStub>> {
    if !root.path.is_dir() {
        return Ok(None);
    }
    let directory_name = session_directory_name_for_lookup(session_id);
    for workspace in walk::subdirectories(&root.path)? {
        let session_dir = workspace.join(&*directory_name);
        if !session_dir.is_dir() {
            continue;
        }
        if let SessionDirectory::Session(files) = session_directory(&session_dir)? {
            return Ok(walk::session_stubs(root, vec![files]).into_iter().next());
        }
    }
    Ok(None)
}

/// The directory to probe for `session_id`. Kimi names it `session_<uuid>`
/// with the UUID in lowercase, so that part is lowercased; the prefix and a
/// sub-agent's `#<agent>` suffix stay as typed.
fn session_directory_name_for_lookup(session_id: &str) -> Cow<'_, str> {
    let Some(after_prefix) = session_id.strip_prefix(SESSION_DIRECTORY_PREFIX) else {
        return Cow::Borrowed(session_id);
    };
    let (uuid, suffix) = after_prefix
        .split_once('#')
        .map_or((after_prefix, None), |(uuid, agent)| (uuid, Some(agent)));
    if !crate::search::is_uuid(uuid) {
        return Cow::Borrowed(session_id);
    }
    let mut name = format!(
        "{SESSION_DIRECTORY_PREFIX}{}",
        crate::search::session_id_for_lookup(uuid)
    );
    if let Some(agent) = suffix {
        name.push('#');
        name.push_str(agent);
    }
    Cow::Owned(name)
}

/// A name Kimi could have given a session directory: its own prefix, and one
/// plain component, so a joined id cannot reach out of the workspace.
fn is_session_directory_name(session_id: &str) -> bool {
    session_id.starts_with(SESSION_DIRECTORY_PREFIX)
        && Path::new(session_id).components().count() == 1
        && Path::new(session_id)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

/// The session directory `path`'s wire belongs to — only when the directory is
/// unmistakably a Kimi session: named `session_…` and carrying a `state.json`.
/// Anything else is a stray copy, so a delete aimed at it cannot take a
/// directory Kimi does not own.
fn owned_session_dir(path: &Path) -> Option<PathBuf> {
    let location = kimi::wire_location(path);
    let name = location.session_dir.file_name()?.to_str()?;
    if !name.starts_with(SESSION_DIRECTORY_PREFIX)
        || !location.session_dir.join("state.json").is_file()
    {
        return None;
    }
    Some(location.session_dir)
}

/// Remove one wire of `session_dir`, and its agent directory under `agents/`
/// when that directory is left empty.
fn remove_wire(session_dir: &Path, wire: &Path) -> Result<()> {
    std::fs::remove_file(wire)?;
    let Some(agent_dir) = wire.parent().filter(|parent| *parent != session_dir) else {
        return Ok(());
    };
    match std::fs::remove_dir(agent_dir) {
        Err(error) if error.kind() != std::io::ErrorKind::DirectoryNotEmpty => Err(error.into()),
        _ => Ok(()),
    }
}

/// Set the session's title in `state.json`, preserving every field this
/// browser does not understand. `isCustomTitle` is the legacy spelling of
/// "the user chose this name", `titleKind` the current one; both are written
/// so either Kimi version reads the rename.
fn rewrite_state_title(state_path: &Path, title: &str) -> Result<()> {
    let contents = std::fs::read_to_string(state_path).map_err(|error| {
        AppError::Io(std::io::Error::new(
            error.kind(),
            format!("{}: {error}", state_path.display()),
        ))
    })?;
    let mut value: Value = serde_json::from_str(&contents)?;
    let object = value.as_object_mut().ok_or_else(|| {
        AppError::ConfigError(format!(
            "{} does not hold a JSON object",
            state_path.display()
        ))
    })?;
    let cleaned = title.replace(['\r', '\n'], " ").trim().to_owned();
    object.insert("title".to_owned(), json!(cleaned));
    object.insert("isCustomTitle".to_owned(), json!(true));
    object.insert("titleKind".to_owned(), json!("custom"));
    write_atomically(state_path, serde_json::to_string(&value)?.as_bytes())
}

/// Drop the session's record from `session_index.jsonl`, atomically. A stale
/// record would offer Kimi's own session picker a session that no longer
/// exists.
fn prune_index_records(session_dir: &Path, session_id: &str) -> Result<()> {
    let Some(index) = session_index_path(session_dir) else {
        return Ok(());
    };
    retain_index_records(&index, |record| {
        record.get("sessionId").and_then(Value::as_str) != Some(session_id)
    })
}

/// `session_index.jsonl` sits beside the `sessions` tree in the Kimi home.
fn session_index_path(session_dir: &Path) -> Option<PathBuf> {
    let sessions = session_dir.parent()?.parent()?;
    if sessions.file_name() != Some(OsStr::new("sessions")) {
        return None;
    }
    Some(sessions.parent()?.join("session_index.jsonl"))
}

struct KimiStorage;

impl SessionStorage for KimiStorage {
    fn source(&self) -> Source {
        Source::Kimi
    }

    fn cache(&self) -> SessionCache {
        SessionCache {
            directory: "kimi",
            magic: *b"KIHIST01",
            schema_version: 15,
        }
    }

    fn roots_in(&self, environment: &RootEnvironment) -> Result<Vec<SessionRoot>> {
        Ok(session_roots_from(
            environment.var(HOME_VARIABLE).as_deref(),
            environment.home().ok_or_else(AppError::home_not_found)?,
        ))
    }

    /// Wire logs are the transcripts; whatever else ends up in a session
    /// directory — state, exports — is not a session of its own. A session
    /// directory is one session: its main wire, with every other agent's wire
    /// as a sub-agent transcript. A directory with no main wire lists each
    /// wire it holds as a session, so none of them is unreachable.
    fn discover(&self, root: &SessionRoot) -> Result<DiscoveredSessions> {
        let mut sessions = Vec::new();
        for session_dir in session_directories(&root.path)? {
            match session_directory(&session_dir)? {
                SessionDirectory::Session(files) => sessions.push(files),
                SessionDirectory::NoMainWire(wires) => {
                    sessions.extend(wires.into_iter().map(|transcript| SessionFiles {
                        transcript,
                        subagents: Vec::new(),
                    }))
                }
            }
        }
        Ok(DiscoveredSessions::complete(walk::session_stubs(
            root, sessions,
        )))
    }

    fn parse_session(
        &self,
        stub: &SessionStub,
        _root: &SessionRoot,
        debug_level: Option<DebugLevel>,
        on_transcript_read: &(dyn Fn() + Sync),
    ) -> Result<Option<Conversation>> {
        parser::process_session_file(stub, &kimi::KIMI_WIRE, debug_level, on_transcript_read)
    }

    /// Titles live in each session's `state.json`, so a rename — Kimi's or
    /// this browser's — never touches the wire the cache validates against.
    fn external_titles(&self, root: &SessionRoot) -> HashMap<String, SessionTitle> {
        let mut titles = HashMap::new();
        let Ok(workspaces) = std::fs::read_dir(&root.path) else {
            return titles;
        };
        for workspace in workspaces.flatten() {
            let Ok(sessions) = std::fs::read_dir(workspace.path()) else {
                continue;
            };
            for session in sessions.flatten() {
                let state = kimi::session_state(&session.path());
                let Some(title) = state.title else {
                    continue;
                };
                let title = if state.title_is_custom {
                    SessionTitle::Custom(title)
                } else {
                    SessionTitle::Generated(title)
                };
                titles.insert(session.file_name().to_string_lossy().into_owned(), title);
            }
        }
        titles
    }
}

/// `KIMI_CODE_HOME` moves the whole home, as it does for Kimi itself; without
/// it both the current home and the legacy `~/.kimi` are searched.
fn session_roots_from(kimi_code_home: Option<&str>, home: &Path) -> Vec<SessionRoot> {
    let mut bases = vec![current_home_from(kimi_code_home, home)];
    if kimi_code_home.is_none_or(str::is_empty) {
        bases.push(home.join(".kimi"));
    }
    bases
        .into_iter()
        .map(|base| SessionRoot::new(base.join(SESSIONS_DIRECTORY)).in_agent_tree())
        .collect()
}

/// The home Kimi Code writes sessions to: `KIMI_CODE_HOME`, else
/// `~/.kimi-code`.
fn current_home_from(kimi_code_home: Option<&str>, home: &Path) -> PathBuf {
    kimi_code_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".kimi-code"))
}

const SESSIONS_DIRECTORY: &str = "sessions";

/// Kimi's catalogue of workspaces, in its home: each workspace's id and root.
const WORKSPACE_CATALOGUE: &str = "workspaces.json";

/// The workspace directory Kimi Code stores the sessions recorded in
/// `directory` in, under the home `environment` names.
fn sessions_dir_of(environment: &RootEnvironment, directory: &Path) -> Result<PathBuf> {
    let kimi_home = current_home_from(
        environment.var(HOME_VARIABLE).as_deref(),
        environment.home().ok_or_else(AppError::home_not_found)?,
    );
    Ok(sessions_dir_in(&kimi_home, directory))
}

/// The workspace directory under Kimi's home `kimi_home` holding the
/// sessions recorded in `directory`. A workspace the catalogue holds for the
/// same directory keeps its id, as in Kimi's `WorkspaceService`; see
/// [`workspace_root_key`] for which spellings name the same directory.
fn sessions_dir_in(kimi_home: &Path, directory: &Path) -> PathBuf {
    let directory = directory.to_string_lossy();
    let computed = workspace_id_of(&directory);
    let id = catalogued_workspace_id(&kimi_home.join(WORKSPACE_CATALOGUE), &computed, &directory)
        .unwrap_or(computed);
    kimi_home.join(SESSIONS_DIRECTORY).join(id)
}

/// The id of the catalogued workspace for `directory`: `computed` when the
/// catalogue holds it, else the first workspace in file order whose root has
/// the same [`workspace_root_key`]. `None` without a readable catalogue or a
/// match.
fn catalogued_workspace_id(catalogue: &Path, computed: &str, directory: &str) -> Option<String> {
    let catalogue: WorkspaceCatalogue =
        serde_json::from_str(&std::fs::read_to_string(catalogue).ok()?).ok()?;
    let kept: Vec<(&String, &str)> = catalogue
        .workspaces
        .iter()
        .filter_map(|(id, entry)| Some((id, kept_workspace_root(entry)?)))
        .collect();
    if kept.iter().any(|(id, _)| *id == computed) {
        return Some(computed.to_owned());
    }
    let root_key = workspace_root_key(directory);
    kept.into_iter()
        .find(|(_, root)| workspace_root_key(root) == root_key)
        .map(|(id, _)| id.clone())
}

/// Kimi's `workspaces.json`, its workspaces as `(id, entry)` in file order.
#[derive(serde::Deserialize)]
struct WorkspaceCatalogue {
    #[serde(deserialize_with = "entries_in_file_order")]
    workspaces: Vec<(String, Value)>,
}

/// A JSON object's entries in the order the file lists them, as Kimi reads
/// them; a `serde_json` map would sort them by key.
fn entries_in_file_order<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<(String, Value)>, D::Error> {
    struct Entries;

    impl<'de> serde::de::Visitor<'de> for Entries {
        type Value = Vec<(String, serde_json::Value)>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a map of workspaces")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut entries = Vec::new();
            while let Some(entry) = map.next_entry()? {
                entries.push(entry);
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(Entries)
}

/// The root of a workspace entry Kimi keeps: one holding `root`, `name`,
/// `created_at` and `last_opened_at` as strings. Kimi drops any other entry
/// (`sanitizeEntry` in its `fileWorkspacePersistence.ts`).
fn kept_workspace_root(entry: &Value) -> Option<&str> {
    let is_string = |field: &str| entry.get(field).is_some_and(Value::is_string);
    if !(is_string("name") && is_string("created_at") && is_string("last_opened_at")) {
        return None;
    }
    entry.get("root")?.as_str()
}

/// The longest slug in a workspace id.
const MAX_WORKSPACE_SLUG_CHARS: usize = 40;
/// The count of hex digits of the directory's SHA-256 hash in a workspace
/// id.
const WORKSPACE_HASH_HEX_DIGITS: usize = 12;

/// Kimi's id for the workspace at `directory` (`encodeWorkDirKey` in Kimi's
/// `workdir-slug.ts`): `wd_`, a slug of its last component, `_`, and the
/// start of the SHA-256 hash of the directory with `/` separators and no
/// trailing one.
fn workspace_id_of(directory: &str) -> String {
    use sha2::{Digest, Sha256};

    let slashed = directory.replace('\\', "/");
    let without_trailing_slash = slashed.trim_end_matches('/');
    let last_component = without_trailing_slash
        .rsplit('/')
        .next()
        .unwrap_or(without_trailing_slash);
    let hash = format!("{:x}", Sha256::digest(without_trailing_slash.as_bytes()));
    format!(
        "wd_{}_{}",
        workspace_slug_of(last_component),
        &hash[..WORKSPACE_HASH_HEX_DIGITS]
    )
}

/// `name` lowercased, each run of characters other than an ASCII letter,
/// digit, `.`, `_` or `-` as one `-`, without a leading or trailing `-`, then
/// truncated; `workspace` when that leaves nothing, `.` or `..`.
fn workspace_slug_of(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut in_replaced_run = false;
    for character in name.to_lowercase().chars() {
        if character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '_' | '-')
        {
            slug.push(character);
            in_replaced_run = false;
        } else if !in_replaced_run {
            slug.push('-');
            in_replaced_run = true;
        }
    }
    let truncated: String = slug
        .trim_matches('-')
        .chars()
        .take(MAX_WORKSPACE_SLUG_CHARS)
        .collect();
    match truncated.trim_matches('-') {
        "" | "." | ".." => "workspace".to_owned(),
        slug => slug.to_owned(),
    }
}

/// The key Kimi compares workspace roots by (`workspaceRootKey`): `/`
/// separators, no trailing one, and lowercase for a drive-letter or UNC path.
fn workspace_root_key(root: &str) -> String {
    let slashed = root.replace('\\', "/");
    let bytes = slashed.as_bytes();
    let is_windows_shaped = slashed.starts_with("//")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'/');
    let without_trailing_slash = slashed.trim_end_matches('/');
    if is_windows_shaped {
        without_trailing_slash.to_lowercase()
    } else {
        without_trailing_slash.to_owned()
    }
}

/// Every session directory under `sessions`, sorted so successive runs
/// agree.
///
/// Sessions sit in a directory per session inside a directory per workspace.
/// Fixed depth rather than recursion, so a symlink cycle inside the tree
/// cannot make the walk unbounded. A missing root yields no directories: an
/// agent the user has not installed is an absence, not a failure.
fn session_directories(sessions: &Path) -> Result<Vec<PathBuf>> {
    if !sessions.exists() {
        return Ok(Vec::new());
    }
    let mut directories = Vec::new();
    for workspace in walk::subdirectories(sessions)? {
        directories.extend(walk::subdirectories(&workspace)?);
    }
    directories.sort();
    Ok(directories)
}

/// The wires one session directory holds, as discovery reads them.
enum SessionDirectory {
    /// The main agent's wire, with every other agent's wire as a sub-agent
    /// transcript, in agent order.
    Session(SessionFiles),
    /// Every wire of a directory with no main wire, in agent order.
    NoMainWire(Vec<PathBuf>),
}

fn session_directory(session_dir: &Path) -> Result<SessionDirectory> {
    let state = kimi::session_state(session_dir);
    let mut wires = session_wires(session_dir)?;
    let Some(main) = wires
        .iter()
        .position(|wire| state.agent_is_main(&kimi::wire_location(wire).agent_id))
    else {
        return Ok(SessionDirectory::NoMainWire(wires));
    };
    let transcript = wires.remove(main);
    Ok(SessionDirectory::Session(SessionFiles {
        transcript,
        subagents: wires,
    }))
}

/// Every wire one session directory holds, sorted: the main agent's, and one
/// per sub-agent. Wires sit directly in the session directory
/// (`wire.jsonl`, the legacy arrangement) or one level further down in a
/// directory per agent: `agents/<agent>/wire.jsonl`.
fn session_wires(session_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut wires = Vec::new();
    collect_wire(session_dir, &mut wires);
    let agents = session_dir.join("agents");
    if agents.is_dir() {
        for agent in walk::subdirectories(&agents)? {
            collect_wire(&agent, &mut wires);
        }
    }
    wires.sort();
    Ok(wires)
}

fn collect_wire(directory: &Path, wires: &mut Vec<PathBuf>) {
    let wire = directory.join("wire.jsonl");
    if wire.is_file() {
        wires.push(wire);
    }
}

/// Kimi resumes by session id — the session directory's name, `session_`
/// prefix included — run in the session's working directory.
struct KimiLauncher;

impl SessionResumer for KimiLauncher {
    fn resume_command(&self, launch: &SessionLaunch) -> Result<std::process::Command> {
        let mut command = std::process::Command::new("kimi");
        command.arg("--session").arg(session_id_of(launch.path)?);
        if let Some(project_path) = launch.project_path {
            command.current_dir(project_path);
        }
        Ok(command)
    }
}

fn session_id_of(path: &Path) -> Result<String> {
    kimi::wire_location(path)
        .session_dir
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| name.starts_with(SESSION_DIRECTORY_PREFIX))
        .map(str::to_owned)
        .ok_or_else(|| {
            AppError::ConfigError(format!(
                "{} is not inside a Kimi session directory",
                path.display()
            ))
        })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::history::provider::RootOrigin;
    use crate::history::provider::contract_tests::{
        AgentSession, FixtureIds, IdCase, Marker, ProviderFixture, fixture_with_lines, load_under,
    };
    use std::ffi::{OsStr as StdOsStr, OsString};

    const SESSION: &str = "session_0f000000-0000-4000-8000-000000000001";
    const OTHER_SESSION: &str = "session_1e000000-0000-4000-8000-000000000002";
    /// The agent id of the sub-agent the contract fixture writes.
    const SUBAGENT: &str = "agent-0";
    /// The agent id of the sub-agent [`SUBAGENT`] ran.
    const NESTED_SUBAGENT: &str = "agent-1";

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/kimi")
            .join(name)
    }

    /// A session directory under `home` with a state file and a main wire,
    /// returning the wire's path.
    fn write_session(home: &Path, session_id: &str, title: &str, is_custom: bool) -> PathBuf {
        let session_dir = session_dir_of(home, session_id);
        std::fs::create_dir_all(session_dir.join("agents/main")).unwrap();
        std::fs::write(
            session_dir.join("state.json"),
            format!(
                concat!(
                    "{{\"id\":\"{id}\",\"version\":2,\"cwd\":\"{cwd}\",",
                    "\"createdAt\":1786010400000,\"agents\":{{\"main\":{{\"type\":\"main\"}}}},",
                    "\"custom\":{{\"kept\":\"field\"}},\"title\":\"{title}\",",
                    "\"isCustomTitle\":{custom},\"lastPrompt\":\"active kimi question\"}}",
                ),
                id = session_id,
                cwd = PROJECT,
                title = title,
                custom = is_custom,
            ),
        )
        .unwrap();
        let wire = session_dir.join("agents/main/wire.jsonl");
        std::fs::copy(fixture("wire.jsonl"), &wire).unwrap();
        wire
    }

    /// The working directory the fixture's sessions are recorded in.
    const PROJECT: &str = "/tmp/kimi-project";
    /// The workspace id Kimi gives [`PROJECT`]: its last component, and the
    /// first 12 hex digits of its SHA-256 hash.
    const PROJECT_WORKSPACE: &str = "wd_kimi-project_26d1fd861767";

    /// The directory of the session `session_id` that `write_session` writes.
    fn session_dir_of(home: &Path, session_id: &str) -> PathBuf {
        home.join("sessions")
            .join(PROJECT_WORKSPACE)
            .join(session_id)
    }

    /// The main wire of the session `session_id` that `write_session` writes.
    fn main_wire_of(home: &Path, session_id: &str) -> PathBuf {
        session_dir_of(home, session_id).join("agents/main/wire.jsonl")
    }

    /// Kimi's sessions for the provider contracts in `contract_tests`.
    pub(crate) struct KimiFixture;

    impl ProviderFixture for KimiFixture {
        fn provider(&self) -> &'static dyn SessionProvider {
            Source::Kimi.provider()
        }

        fn ids(&self) -> FixtureIds {
            FixtureIds {
                session: SESSION,
                other: OTHER_SESSION,
                unknown: "session_2d000000-0000-4000-8000-000000000003",
                session_in_other_case: "session_0F000000-0000-4000-8000-000000000001",
            }
        }

        fn id_case(&self) -> IdCase {
            IdCase::Insensitive
        }

        fn root_under(&self, home: &Path) -> SessionRoot {
            SessionRoot::new(home.join("sessions")).in_agent_tree()
        }

        fn write_session(&self, home: &Path, session: &str) -> PathBuf {
            write_session(home, session, "kimi title", false)
        }

        fn session_working_dir(&self) -> PathBuf {
            PathBuf::from(PROJECT)
        }

        fn root_variables(&self, home: &Path) -> Vec<(&'static str, OsString)> {
            vec![(HOME_VARIABLE, home.into())]
        }

        fn every_record_session(&self) -> AgentSession {
            let appended = |role: &str, text: &str, origin: Value| {
                json!({
                    "type": "context.append_message",
                    "message": {"role": role, "content": [{"type": "text", "text": text}], "origin": origin},
                    "time": 1786010600000_i64,
                })
                .to_string()
            };
            let event = |event: Value| {
                json!({"type": "context.append_loop_event", "event": event, "time": 1786010601000_i64})
                    .to_string()
            };
            let task_notice = concat!(
                "<notification id=\"task:bash-9:completed\" category=\"task\" type=\"task.completed\" source_kind=\"background_task\" source_id=\"bash-9\">\n",
                "Title: Background process completed\n",
                "Severity: info\n",
                "TASK_NOTICE_SENTINEL: headless trace completed.\n",
                "</notification>",
            );
            let (wire, storage) = fixture_with_lines(
                &fixture("wire.jsonl"),
                &[
                    appended(
                        "user",
                        task_notice,
                        json!({"kind": "task", "taskId": "bash-9", "status": "completed"}),
                    ),
                    appended(
                        "user",
                        "<hook_result hook_event=\"UserPromptSubmit\">\nHOOK_RESULT_SENTINEL: prompt quality C.\n</hook_result>",
                        json!({"kind": "hook_result", "event": "UserPromptSubmit"}),
                    ),
                    appended(
                        "assistant",
                        "<hook_result hook_event=\"PreToolUse\">\nBLOCKED_HOOK_SENTINEL: rm is not allowed.\n</hook_result>",
                        json!({"kind": "hook_result", "event": "PreToolUse", "blocked": true}),
                    ),
                    appended(
                        "user",
                        "<cron-fire jobId=\"job-9\" cron=\"0 9 * * *\" recurring=\"true\" coalescedCount=\"1\" stale=\"false\">\n<prompt>\nSCHEDULED_SENTINEL: check the build.\n</prompt>\n</cron-fire>",
                        json!({"kind": "cron_job", "jobId": "job-9", "cron": "0 9 * * *", "recurring": true}),
                    ),
                    appended(
                        "user",
                        "<bash-input>\necho SHELL_COMMAND_SENTINEL\n</bash-input>",
                        json!({"kind": "shell_command", "phase": "input"}),
                    ),
                    appended(
                        "user",
                        "<bash-stdout>SHELL_OUTPUT_SENTINEL\n</bash-stdout>",
                        json!({"kind": "shell_command", "phase": "output"}),
                    ),
                    appended(
                        "user",
                        "<git-context>\nBranch: main\n</git-context>\n\nSUBAGENT_PROMPT_SENTINEL: research the part numbers.",
                        json!({"kind": "system_trigger", "name": "subagent"}),
                    ),
                    appended(
                        "user",
                        "GOAL_CONTINUATION_SENTINEL: continue working toward the goal.",
                        json!({"kind": "system_trigger", "name": "goal_continuation"}),
                    ),
                    appended(
                        "user",
                        "INJECTION_SENTINEL: plan mode is active.",
                        json!({"kind": "injection", "variant": "plan_mode"}),
                    ),
                    event(json!({
                        "type": "tool.call", "toolCallId": "Skill_9", "name": "Skill",
                        "args": {"skill": "check-docs", "args": "MODEL_SKILL_ARGS_SENTINEL: how to install skills"}
                    })),
                    appended(
                        "user",
                        "<kimi-skill-loaded name=\"check-docs\" trigger=\"model-tool\">\nMODEL_SKILL_FILE_SENTINEL\n</kimi-skill-loaded>",
                        json!({"kind": "skill_activation", "skillName": "check-docs", "trigger": "model-tool"}),
                    ),
                    appended(
                        "user",
                        "<kimi-skill-loaded name=\"mcp-config\" trigger=\"user-slash\">\nUSER_SKILL_FILE_SENTINEL\n</kimi-skill-loaded>",
                        json!({"kind": "skill_activation", "skillName": "mcp-config", "skillArgs": "SLASH_ARGS_SENTINEL", "trigger": "user-slash"}),
                    ),
                    appended("user", "Is the notes file empty?", json!({"kind": "user"})),
                    event(json!({
                        "type": "tool.call", "toolCallId": "Bash_9", "name": "Bash",
                        "args": {"command": "cat notes.txt"}
                    })),
                    event(
                        json!({"type": "tool.result", "toolCallId": "Bash_9", "result": {"output": ""}}),
                    ),
                    event(json!({
                        "type": "content.part",
                        "part": {"type": "text", "text": "NUMBERED_REPLY_SENTINEL: the command printed nothing."}
                    })),
                    event(json!({
                        "type": "tool.call", "toolCallId": "Agent_9", "name": "Agent",
                        "args": {
                            "description": "List the release steps",
                            "prompt": "LAUNCH_PROMPT_SENTINEL: list the release steps.",
                            "subagent_type": "explore"
                        }
                    })),
                    event(json!({
                        "type": "tool.result", "toolCallId": "Agent_9",
                        "result": {"output": "The notes list the release steps."}
                    })),
                ],
            );
            AgentSession::new(
                Source::Kimi,
                wire,
                vec![fixture("subagent-wire.jsonl")],
                vec![
                    Marker::searched("user text", "active kimi question"),
                    Marker::searched("assistant text", "kimi answer searchable"),
                    Marker::shown("thinking", "THINKING_VISIBLE"),
                    Marker::agent_searched("tool call", "TOOL_INPUT_VISIBLE")
                        .in_summary_row("Ran 1 shell command"),
                    Marker::searched("tool result", "kimi tool output searchable")
                        .in_summary_row("Ran 1 shell command"),
                    Marker::agent_searched("sub-agent message", "kimi child answer searchable"),
                    Marker::searched("task notice", "TASK_NOTICE_SENTINEL"),
                    Marker::searched("hook output", "HOOK_RESULT_SENTINEL"),
                    Marker::searched("blocking hook output", "BLOCKED_HOOK_SENTINEL"),
                    Marker::searched("scheduled reminder", "SCHEDULED_SENTINEL"),
                    Marker::agent_searched("user's shell command", "SHELL_COMMAND_SENTINEL"),
                    Marker::searched("user's shell command output", "SHELL_OUTPUT_SENTINEL"),
                    Marker::shown("model-run skill call", "MODEL_SKILL_ARGS_SENTINEL")
                        .in_summary_row("Loaded 1 skill"),
                    Marker::searched("user-run slash command", "/mcp-config SLASH_ARGS_SENTINEL"),
                    Marker::agent_searched("launch prompt", "LAUNCH_PROMPT_SENTINEL")
                        .in_summary_row("Started 1 agent"),
                ],
            )
            .with_hidden(&[
                ("injected reminder", "SYSTEM_REMINDER_SENTINEL"),
                ("model request", "LLM_REQUEST_SENTINEL"),
                ("tool result's system note", "NOTE_SENTINEL"),
                ("unknown event", "UNKNOWN_EVENT_SENTINEL"),
                ("sub-agent prompt", "SUBAGENT_PROMPT_SENTINEL"),
                ("goal continuation", "GOAL_CONTINUATION_SENTINEL"),
                ("injection", "INJECTION_SENTINEL"),
                ("model-run skill's file", "MODEL_SKILL_FILE_SENTINEL"),
                ("user-run skill's file", "USER_SKILL_FILE_SENTINEL"),
            ])
            .with_reply_after_records_without_text("NUMBERED_REPLY_SENTINEL", 20)
            // The sub-agent's turn usage: 20 input and 10 output.
            .with_subagent_tokens(30)
            .kept_in(storage)
        }

        fn write_subagent(&self, home: &Path, session: &str) -> PathBuf {
            write_subagent_wire(&main_wire_of(home, session), SUBAGENT)
        }

        /// Kimi keeps every agent's wire flat under `agents/`. Only
        /// `parentAgentId` in `state.json` names a nested agent's parent, and
        /// discovery does not read it.
        fn write_nested_subagent(&self, home: &Path, session: &str) -> PathBuf {
            write_subagent_wire(&main_wire_of(home, session), NESTED_SUBAGENT)
        }

        /// The `<session>#<agent>` name a sub-agent thread is read by.
        fn subagent_id(&self, session: &str) -> String {
            format!("{session}#{SUBAGENT}")
        }

        fn resolve_under(&self, home: &Path, id: &str) -> Result<Option<ResolvedSession>> {
            let root = self.root_under(home);
            Ok(session_stub_of(&root, id)?.map(|stub| ResolvedSession { root, stub }))
        }

        fn roots_from(&self, override_dir: Option<&str>, home: &Path) -> Vec<PathBuf> {
            session_roots_from(override_dir, home)
                .into_iter()
                .map(|root| root.path)
                .collect()
        }
    }

    #[test]
    fn the_session_roots_are_both_kimi_homes_or_the_override() {
        let home = Path::new("/home/user");
        let defaults = session_roots_from(None, home);
        assert_eq!(
            defaults
                .iter()
                .map(|root| root.path.clone())
                .collect::<Vec<_>>(),
            vec![
                home.join(".kimi-code/sessions"),
                home.join(".kimi/sessions"),
            ],
            "the legacy ~/.kimi home is still searched"
        );
        for root in &defaults {
            assert_eq!(
                root.origin(),
                RootOrigin::AgentTree,
                "both homes are Kimi's own trees"
            );
        }
        assert_eq!(
            session_roots_from(Some("/opt/kimi"), home)
                .iter()
                .map(|root| root.path.clone())
                .collect::<Vec<_>>(),
            vec![Path::new("/opt/kimi").join("sessions")],
            "KIMI_CODE_HOME replaces both defaults, as it does for Kimi itself"
        );
    }

    /// The hashes are SHA-256 prefixes worked out apart from this code.
    #[test]
    fn a_workspace_id_is_a_slug_and_a_hash_of_the_directory() {
        for (directory, id) in [
            ("/tmp/kimi-project", "wd_kimi-project_26d1fd861767"),
            (r"C:\work\proj", "wd_proj_6b8c494bc88d"),
            ("/srv/My App/", "wd_my-app_4b739b7c6f4f"),
            (r"C:\", "wd_c_82179da17054"),
            ("/", "wd_workspace_e3b0c44298fc"),
            ("/srv/项目", "wd_workspace_d965ae67b7af"),
        ] {
            assert_eq!(workspace_id_of(directory), id, "{directory}");
        }
    }

    /// A catalogue holding `workspaces` as `(id, root)`, each entry complete
    /// as Kimi writes it, in this order.
    fn write_catalogue(kimi_home: &Path, workspaces: &[(&str, &str)]) {
        let entries: Vec<String> = workspaces
            .iter()
            .map(|(id, root)| {
                format!(
                    "{}:{}",
                    json!(id),
                    json!({
                        "root": root,
                        "name": "proj",
                        "created_at": "2026-10-01T00:00:00.000Z",
                        "last_opened_at": "2026-10-02T00:00:00.000Z",
                    })
                )
            })
            .collect();
        std::fs::write(
            kimi_home.join(WORKSPACE_CATALOGUE),
            format!(r#"{{"version":1,"workspaces":{{{}}}}}"#, entries.join(",")),
        )
        .unwrap();
    }

    /// Kimi keeps the id of the spelling it catalogued first. Roots compare
    /// ignoring a trailing separator; a drive-letter or UNC root also
    /// ignores case, and a POSIX root compares case-sensitively.
    #[test]
    fn a_catalogued_workspace_keeps_its_id() {
        let kimi_home = tempfile::tempdir().unwrap();
        write_catalogue(
            kimi_home.path(),
            &[
                ("wd_proj_first", r"c:\work\proj\"),
                ("wd_other_first", "/srv/other"),
            ],
        );
        let sessions = kimi_home.path().join("sessions");

        assert_eq!(
            sessions_dir_in(kimi_home.path(), Path::new(r"C:\work\proj")),
            sessions.join("wd_proj_first")
        );
        assert_eq!(
            sessions_dir_in(kimi_home.path(), Path::new(r"C:\Work\Proj")),
            sessions.join("wd_proj_first")
        );
        assert_eq!(
            sessions_dir_in(kimi_home.path(), Path::new("/srv/other/")),
            sessions.join("wd_other_first")
        );
        assert_eq!(
            sessions_dir_in(kimi_home.path(), Path::new("/srv/Other")),
            sessions.join(workspace_id_of("/srv/Other"))
        );
    }

    /// A key-sorted reading would pick `wd_proj_aa`.
    #[test]
    fn the_first_catalogued_spelling_in_file_order_keeps_its_id() {
        let kimi_home = tempfile::tempdir().unwrap();
        write_catalogue(
            kimi_home.path(),
            &[
                ("wd_proj_zz", r"C:\work\proj"),
                ("wd_proj_aa", r"c:\work\proj"),
            ],
        );

        assert_eq!(
            sessions_dir_in(kimi_home.path(), Path::new(r"C:\WORK\proj")),
            kimi_home.path().join("sessions/wd_proj_zz")
        );
    }

    #[test]
    fn the_computed_id_wins_over_an_older_spelling() {
        let kimi_home = tempfile::tempdir().unwrap();
        let computed = workspace_id_of(r"C:\work\proj");
        write_catalogue(
            kimi_home.path(),
            &[
                ("wd_proj_older", r"c:\work\proj"),
                (&computed, r"C:\work\proj"),
            ],
        );

        assert_eq!(
            sessions_dir_in(kimi_home.path(), Path::new(r"C:\work\proj")),
            kimi_home.path().join("sessions").join(computed)
        );
    }

    /// Kimi drops an entry without `name`, `created_at` or `last_opened_at`.
    #[test]
    fn an_incomplete_catalogue_entry_is_skipped() {
        let kimi_home = tempfile::tempdir().unwrap();
        std::fs::write(
            kimi_home.path().join(WORKSPACE_CATALOGUE),
            r#"{"version":1,"workspaces":{"wd_proj_partial":{"root":"c:\\work\\proj"}}}"#,
        )
        .unwrap();

        assert_eq!(
            sessions_dir_in(kimi_home.path(), Path::new(r"C:\work\proj")),
            kimi_home
                .path()
                .join("sessions")
                .join(workspace_id_of(r"C:\work\proj"))
        );
    }

    #[test]
    fn only_wire_logs_are_kimi_transcripts() {
        let home = tempfile::tempdir().unwrap();
        let wire = write_session(home.path(), SESSION, "t", false);
        let session_dir = wire.parent().unwrap().parent().unwrap().parent().unwrap();
        std::fs::write(session_dir.join("export.jsonl"), "{}\n").unwrap();

        let root = SessionRoot::new(home.path().join("sessions"));
        let discovered = KimiStorage
            .discover(&root)
            .unwrap()
            .stubs
            .into_iter()
            .map(|stub| stub.locator)
            .collect::<Vec<_>>();
        assert_eq!(discovered, vec![wire]);
    }

    /// Nothing reads `archived` from `state.json`. A reader that starts to,
    /// and skips the session, fails here.
    #[test]
    fn an_archived_session_is_discovered_like_any_other() {
        let home = tempfile::tempdir().unwrap();
        let wire = write_session(home.path(), SESSION, "archived away", false);
        let session_dir = wire.parent().unwrap().parent().unwrap().parent().unwrap();
        let state = session_dir.join("state.json");
        let mut recorded: Value =
            serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        recorded
            .as_object_mut()
            .unwrap()
            .insert("archived".to_owned(), json!(true));
        std::fs::write(&state, serde_json::to_string(&recorded).unwrap()).unwrap();

        let root = SessionRoot::new(home.path().join("sessions"));
        let discovered = KimiStorage
            .discover(&root)
            .unwrap()
            .stubs
            .into_iter()
            .map(|stub| stub.locator)
            .collect::<Vec<_>>();
        assert_eq!(discovered, vec![wire]);
    }

    #[test]
    fn resume_takes_the_session_directory_name() {
        let home = tempfile::tempdir().unwrap();
        let wire = write_session(home.path(), SESSION, "t", false);
        let project = PathBuf::from("/tmp/kimi-project");
        let launch = SessionLaunch {
            path: &wire,
            project_path: Some(&project),
            configured_args: &["--claude-only".to_owned()],
        };
        let Capability::Capable(resumer) = Source::Kimi.provider().resume() else {
            panic!("Kimi resumes");
        };

        let resume = resumer.resume_command(&launch).unwrap();
        assert_eq!(resume.get_program(), StdOsStr::new("kimi"));
        assert_eq!(
            resume.get_args().collect::<Vec<_>>(),
            vec![StdOsStr::new("--session"), StdOsStr::new(SESSION)],
            "kimi resumes by the session_-prefixed id and must not inherit Claude's arguments"
        );
        assert_eq!(resume.get_current_dir(), Some(project.as_path()));

        let stray = PathBuf::from("/elsewhere/wire.jsonl");
        assert!(
            resumer
                .resume_command(&SessionLaunch {
                    path: &stray,
                    project_path: None,
                    configured_args: &[],
                })
                .is_err(),
            "a wire outside a session directory has no id to resume"
        );
    }

    #[test]
    fn rename_rewrites_the_state_and_keeps_unknown_fields() {
        let home = tempfile::tempdir().unwrap();
        let wire = write_session(home.path(), SESSION, "kimi generated title", false);

        KimiProvider.rename_session(&wire, "renamed\nKimi").unwrap();

        let state_path = wire
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("state.json");
        let state: Value =
            serde_json::from_str(&std::fs::read_to_string(state_path).unwrap()).unwrap();
        assert_eq!(state["title"], "renamed Kimi");
        assert_eq!(state["isCustomTitle"], true);
        assert_eq!(state["titleKind"], "custom");
        assert_eq!(
            state["custom"]["kept"], "field",
            "fields this browser does not understand must survive a rename"
        );

        let reparsed = kimi::KIMI_WIRE.parse_transcript(&wire).unwrap().unwrap();
        assert_eq!(reparsed.title.as_deref(), Some("renamed Kimi"));
    }

    /// Kimi's generated title is a summary, not a name the user chose, in a
    /// cold load and through the title overlay of a warm one.
    #[test]
    fn a_generated_title_lists_as_the_summary() {
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write_session(home.path(), SESSION, "kimi generated title", false);

        for load in ["cold", "warm"] {
            let listed = load_under(&KimiFixture, home.path(), cache.path());
            assert_eq!(
                listed[0].summary.as_deref(),
                Some("kimi generated title"),
                "{load} load"
            );
            assert_eq!(listed[0].custom_title, None, "{load} load");
        }
    }

    #[test]
    fn delete_removes_the_session_directory_and_its_index_record() {
        let home = tempfile::tempdir().unwrap();
        let doomed = write_session(home.path(), SESSION, "doomed", false);
        let kept = write_session(home.path(), OTHER_SESSION, "kept", false);
        write_session_index(home.path(), &[SESSION, OTHER_SESSION]);

        KimiProvider.delete_session(&doomed).unwrap();

        assert!(
            !session_dir_of(home.path(), SESSION).exists(),
            "the whole session directory is removed"
        );
        assert!(kept.exists());
        let index = std::fs::read_to_string(home.path().join("session_index.jsonl")).unwrap();
        assert!(!index.contains(SESSION));
        assert!(index.contains(OTHER_SESSION));
    }

    #[test]
    fn delete_reports_the_sub_agent_sessions_it_removes() {
        let home = tempfile::tempdir().unwrap();
        let wire = write_session(home.path(), SESSION, "with sub-agents", false);
        write_subagent_wire(&wire, "agent-0");
        write_subagent_wire(&wire, "agent-1");

        let deleted = KimiProvider.delete_session(&wire).unwrap();

        assert_eq!(
            deleted,
            Deleted {
                subagent_sessions: 2,
                ..Deleted::just_the_session()
            }
        );
    }

    /// A wire that parses as Kimi's but sits in a directory that is not a
    /// session — no `session_` name, no state — must not take that directory
    /// with it.
    #[test]
    fn delete_of_a_stray_wire_removes_only_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let holding = directory.path().join("loose");
        std::fs::create_dir_all(&holding).unwrap();
        let wire = holding.join("wire.jsonl");
        std::fs::copy(fixture("wire.jsonl"), &wire).unwrap();
        let sibling = holding.join("notes.txt");
        std::fs::write(&sibling, "keep").unwrap();

        KimiProvider.delete_session(&wire).unwrap();

        assert!(!wire.exists());
        assert!(sibling.exists());
        assert!(holding.exists());
    }

    /// `session_index.jsonl` in the Kimi home, one record per session.
    fn write_session_index(home: &Path, session_ids: &[&str]) {
        let records: String = session_ids
            .iter()
            .map(|id| {
                format!(
                    "{}\n",
                    json!({"sessionId": id, "workDir": "/tmp/kimi-project"})
                )
            })
            .collect();
        std::fs::write(home.join("session_index.jsonl"), records).unwrap();
    }

    /// A sub-agent wire beside the main one, returning its path.
    fn write_subagent_wire(main_wire: &Path, agent_id: &str) -> PathBuf {
        let agent_dir = main_wire.parent().unwrap().parent().unwrap().join(agent_id);
        std::fs::create_dir_all(&agent_dir).unwrap();
        let wire = agent_dir.join("wire.jsonl");
        std::fs::copy(fixture("subagent-wire.jsonl"), &wire).unwrap();
        wire
    }

    /// The prefix and a sub-agent's `#<agent>` suffix are not part of the
    /// UUID, so they are probed as typed.
    #[test]
    fn the_lookup_name_lowercases_the_uuid_alone() {
        assert_eq!(
            session_directory_name_for_lookup(
                "session_0F000000-0000-4000-8000-000000000001#agent-0"
            ),
            "session_0f000000-0000-4000-8000-000000000001#agent-0"
        );
        assert_eq!(
            session_directory_name_for_lookup("session_Custom-Name"),
            "session_Custom-Name"
        );
    }

    /// A session's sub-agent wires are named in agent order, and their text
    /// reaches the row's search text, not its own text.
    #[test]
    fn a_sessions_sub_agent_wires_are_named_in_agent_order_and_reach_its_search_text() {
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let wire = write_session(home.path(), SESSION, "kimi title", false);
        let second = write_subagent_wire(&wire, "agent-1");
        let first = write_subagent_wire(&wire, "agent-0");

        let discovered = KimiFixture.discover_under(home.path());
        assert_eq!(discovered[0].subagents, vec![first, second]);

        let listed = load_under(&KimiFixture, home.path(), cache.path());
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0]
                .agent_search_text
                .matches("kimi child answer searchable")
                .count(),
            2,
            "each sub-agent wire's text reaches the row once"
        );
        assert!(!listed[0].full_text.contains("kimi child answer searchable"));
    }

    /// Semantic and hybrid `agent search` route to a session by its
    /// `semantic_route_text`, so a phrase only a sub-agent wire holds has to
    /// reach it.
    #[test]
    fn a_sub_agent_wires_text_reaches_the_sessions_semantic_routing_text() {
        let home = tempfile::tempdir().unwrap();
        let wire = write_session(home.path(), SESSION, "kimi title", false);
        write_subagent_wire(&wire, "agent-0");
        let root = SessionRoot::new(home.path().join("sessions")).in_agent_tree();
        let stub = KimiStorage.discover(&root).unwrap().stubs.remove(0);

        let session = parser::process_session_file(&stub, &kimi::KIMI_WIRE, None, &|| {})
            .unwrap()
            .unwrap();

        assert!(!session.full_text.contains("kimi child answer searchable"));
        assert!(
            session
                .semantic_route_text
                .contains("kimi child answer searchable"),
            "{}",
            session.semantic_route_text
        );
    }

    /// A sub-agent wire that cannot be read is left out of its session: the
    /// session lists with its own text and counts, and its entry is cached.
    /// Failing the session with the wire would delist it until the wire
    /// changed on disk.
    #[test]
    fn a_session_whose_sub_agent_cannot_be_read_still_lists() {
        use crate::history::cache::SessionCacheStore;
        use crate::history::provider::load_sessions_with_cache;

        let home = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        let wire = write_session(home.path(), SESSION, "kimi title", false);
        let unreadable = write_subagent_wire(&wire, "agent-0");
        std::fs::write(&unreadable, [0xff, 0xfe]).unwrap();
        let root = SessionRoot::new(home.path().join("sessions")).in_agent_tree();
        let mut alone = KimiStorage.discover(&root).unwrap().stubs.remove(0);
        assert_eq!(alone.subagents, vec![unreadable]);
        alone.subagents.clear();
        let alone = parser::process_session_file(&alone, &kimi::KIMI_WIRE, None, &|| {})
            .unwrap()
            .unwrap();

        let storage = crate::history::provider::storage::RootedStorage {
            inner: KimiStorage,
            root: root.clone(),
        };
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let listed = load_sessions_with_cache(&storage, &cache, false, None).unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, wire);
        assert_eq!(listed[0].message_count, alone.message_count);
        assert_eq!(listed[0].total_tokens, alone.total_tokens);
        assert_eq!(listed[0].full_text, alone.full_text);
        assert_eq!(listed[0].agent_search_text, alone.agent_search_text);
        assert_eq!(
            cache.read(&root.path).len(),
            1,
            "the session is cached without the wire it could not read"
        );
    }

    /// A directory with no main wire has no session to read the others
    /// through, so each wire lists on its own rather than vanishing.
    #[test]
    fn a_session_directory_without_a_main_wire_lists_each_wire() {
        let home = tempfile::tempdir().unwrap();
        let wire = write_session(home.path(), SESSION, "kimi title", false);
        let subagent = write_subagent_wire(&wire, "agent-0");
        std::fs::remove_file(&wire).unwrap();
        let root = SessionRoot::new(home.path().join("sessions"));

        let discovered = KimiStorage.discover(&root).unwrap();

        assert_eq!(discovered.stubs.len(), 1);
        assert_eq!(discovered.stubs[0].locator, subagent);
        assert!(discovered.stubs[0].subagents.is_empty());
        assert_eq!(
            session_stub_of(&root, SESSION).unwrap(),
            None,
            "the id names a session the directory no longer holds"
        );
    }

    #[test]
    fn delete_without_a_main_wire_removes_only_the_chosen_wire() {
        let home = tempfile::tempdir().unwrap();
        let main = write_session(home.path(), SESSION, "kimi title", false);
        let doomed = write_subagent_wire(&main, "agent-0");
        let kept = write_subagent_wire(&main, "agent-1");
        std::fs::remove_file(&main).unwrap();
        write_session_index(home.path(), &[SESSION]);
        let root = SessionRoot::new(home.path().join("sessions"));

        let deleted = KimiProvider.delete_session(&doomed).unwrap();

        assert_eq!(deleted, Deleted::just_the_session());
        assert!(
            !doomed.parent().unwrap().exists(),
            "the emptied agent directory is removed"
        );
        let listed = KimiStorage
            .discover(&root)
            .unwrap()
            .stubs
            .into_iter()
            .map(|stub| stub.locator)
            .collect::<Vec<_>>();
        assert_eq!(listed, vec![kept]);
        let index = std::fs::read_to_string(home.path().join("session_index.jsonl")).unwrap();
        assert!(
            index.contains(SESSION),
            "the directory still holds a listed session"
        );
    }

    #[test]
    fn delete_of_the_last_wire_without_a_main_wire_removes_the_directory_and_its_index_record() {
        let home = tempfile::tempdir().unwrap();
        let main = write_session(home.path(), SESSION, "kimi title", false);
        let last = write_subagent_wire(&main, "agent-0");
        std::fs::remove_file(&main).unwrap();
        write_session_index(home.path(), &[SESSION, OTHER_SESSION]);

        let deleted = KimiProvider.delete_session(&last).unwrap();

        assert_eq!(deleted, Deleted::just_the_session());
        assert!(!session_dir_of(home.path(), SESSION).exists());
        let index = std::fs::read_to_string(home.path().join("session_index.jsonl")).unwrap();
        assert!(!index.contains(SESSION));
        assert!(index.contains(OTHER_SESSION));
    }

    /// An id joined to a workspace directory would otherwise be a path.
    #[test]
    fn an_id_that_is_not_a_session_directory_name_resolves_to_nothing() {
        assert_eq!(
            KimiProvider
                .resolve_session_id("session_../escape")
                .unwrap(),
            None
        );
        assert_eq!(KimiProvider.resolve_session_id("wire.jsonl").unwrap(), None);
    }
}
