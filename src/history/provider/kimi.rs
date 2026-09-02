//! Kimi Code sessions: one directory per session under
//! `~/.kimi-code/sessions/<workspace>/`, holding `state.json` and a
//! `wire.jsonl` per agent.

use super::walk::SessionFiles;
use super::{
    Deleted, DiscoveredSessions, RefNamespaces, ResolvedSession, SessionCache, SessionLaunch,
    SessionLauncher, SessionProvider, SessionRoot, SessionStorage, SessionStub, SessionTitle,
    SourceLabels, retain_index_records, walk, write_atomically,
};
use crate::cli::DebugLevel;
use crate::error::{AppError, Result};
use crate::history::format::{self, SessionFormat, kimi};
use crate::history::{Conversation, Source, parser};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub struct KimiProvider;

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
            conversation: Some("agent-kimi-v1"),
            project: "agent-kimi-project-v1",
        }
    }

    fn storage(&self) -> Option<&dyn SessionStorage> {
        Some(&KimiStorage)
    }

    fn format(&self) -> Option<&dyn SessionFormat> {
        Some(&kimi::KIMI_WIRE)
    }

    fn launcher(&self) -> &dyn SessionLauncher {
        &KimiLauncher
    }

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

    /// A Kimi session is its directory — state, wires, diagnostic logs — so
    /// delete removes the whole directory, then the session's index record.
    fn delete_session(&self, path: &Path) -> Result<Deleted> {
        format::require_owned_transcript(Source::Kimi, path)?;
        let Some(session_dir) = owned_session_dir(path) else {
            // A stray wire outside a session directory: whatever holds the
            // file is not Kimi's to remove.
            std::fs::remove_file(path)?;
            return Ok(Deleted::just_the_session());
        };
        let session_id = session_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // The session is one stored thing however many wires it holds: the
        // sub-agent threads inside are read through it, not listed beside it.
        std::fs::remove_dir_all(&session_dir)?;
        prune_index_records(&session_dir, &session_id)?;
        Ok(Deleted::just_the_session())
    }

    /// A Kimi session id names the directory holding its wires. A sub-agent
    /// thread is named `<session>#<agent>` and is read through its parent, so
    /// it resolves to no wire of its own.
    fn resolve_session_id(&self, session_id: &str) -> Result<Option<ResolvedSession>> {
        if !is_session_directory_name(session_id) {
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
    for workspace in walk::subdirectories(&root.path)? {
        let session_dir = workspace.join(session_id);
        if !session_dir.is_dir() {
            continue;
        }
        if let SessionDirectory::Session(files) = session_directory(&session_dir)? {
            return Ok(walk::session_stubs(root, vec![files]).into_iter().next());
        }
    }
    Ok(None)
}

/// A name Kimi could have given a session directory: its own prefix, and one
/// plain component, so a joined id cannot reach out of the workspace.
fn is_session_directory_name(session_id: &str) -> bool {
    session_id.starts_with("session_")
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
    if !name.starts_with("session_") || !location.session_dir.join("state.json").is_file() {
        return None;
    }
    Some(location.session_dir)
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
            schema_version: 6,
        }
    }

    fn roots(&self) -> Result<Vec<SessionRoot>> {
        let home = home::home_dir().ok_or_else(|| {
            AppError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Could not determine home directory",
            ))
        })?;
        Ok(session_roots_from(
            std::env::var("KIMI_CODE_HOME").ok().as_deref(),
            &home,
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
    ) -> Result<Option<Conversation>> {
        parser::process_session_file(stub, &kimi::KIMI_WIRE, debug_level)
    }

    /// Every wire is parsed in full, however large: the biggest sessions are
    /// the most valuable to search, and skipping one would delist it.
    fn max_session_bytes(&self) -> Option<u64> {
        None
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
    let bases = match kimi_code_home.filter(|value| !value.is_empty()) {
        Some(base) => vec![PathBuf::from(base)],
        None => vec![home.join(".kimi-code"), home.join(".kimi")],
    };
    bases
        .into_iter()
        .map(|base| SessionRoot::new(base.join("sessions")).in_agent_tree())
        .collect()
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
/// prefix included — run in the session's working directory. Its CLI cannot
/// start a forked session: forking exists only as the `/fork` command inside
/// a running session.
struct KimiLauncher;

impl SessionLauncher for KimiLauncher {
    fn resume_command(&self, launch: &SessionLaunch) -> Result<std::process::Command> {
        let mut command = std::process::Command::new("kimi");
        command.arg("--session").arg(session_id_of(launch.path)?);
        if let Some(project_path) = launch.project_path {
            command.current_dir(project_path);
        }
        Ok(command)
    }

    fn fork_command(&self, _launch: &SessionLaunch) -> Result<std::process::Command> {
        Err(AppError::UnsupportedCapability(
            "Kimi forks only from inside a session; resume it, then use /fork".to_owned(),
        ))
    }
}

fn session_id_of(path: &Path) -> Result<String> {
    kimi::wire_location(path)
        .session_dir
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| name.starts_with("session_"))
        .map(str::to_owned)
        .ok_or_else(|| {
            AppError::ConfigError(format!(
                "{} is not inside a Kimi session directory",
                path.display()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::provider::RootOrigin;
    use std::ffi::OsStr as StdOsStr;

    const SESSION: &str = "session_0f000000-0000-4000-8000-000000000001";
    const OTHER_SESSION: &str = "session_1e000000-0000-4000-8000-000000000002";

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/kimi")
            .join(name)
    }

    /// A session directory under `home` with a state file and a main wire,
    /// returning the wire's path.
    fn write_session(home: &Path, session_id: &str, title: &str, is_custom: bool) -> PathBuf {
        let session_dir = home
            .join("sessions/wd_kimi-project_abc123")
            .join(session_id);
        std::fs::create_dir_all(session_dir.join("agents/main")).unwrap();
        std::fs::write(
            session_dir.join("state.json"),
            format!(
                concat!(
                    "{{\"id\":\"{id}\",\"version\":2,\"cwd\":\"/tmp/kimi-project\",",
                    "\"createdAt\":1786010400000,\"agents\":{{\"main\":{{\"type\":\"main\"}}}},",
                    "\"custom\":{{\"kept\":\"field\"}},\"title\":\"{title}\",",
                    "\"isCustomTitle\":{custom},\"lastPrompt\":\"active kimi question\"}}",
                ),
                id = session_id,
                title = title,
                custom = is_custom,
            ),
        )
        .unwrap();
        let wire = session_dir.join("agents/main/wire.jsonl");
        std::fs::copy(fixture("wire.jsonl"), &wire).unwrap();
        wire
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
        assert_eq!(
            session_roots_from(Some(""), home).len(),
            2,
            "an empty override means unset"
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
    fn resume_takes_the_session_directory_name_and_fork_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let wire = write_session(home.path(), SESSION, "t", false);
        let project = PathBuf::from("/tmp/kimi-project");
        let launch = SessionLaunch {
            path: &wire,
            project_path: Some(&project),
            configured_args: &["--claude-only".to_owned()],
        };
        let launcher = Source::Kimi.provider().launcher();

        let resume = launcher.resume_command(&launch).unwrap();
        assert_eq!(resume.get_program(), StdOsStr::new("kimi"));
        assert_eq!(
            resume.get_args().collect::<Vec<_>>(),
            vec![StdOsStr::new("--session"), StdOsStr::new(SESSION)],
            "kimi resumes by the session_-prefixed id and must not inherit Claude's arguments"
        );
        assert_eq!(resume.get_current_dir(), Some(project.as_path()));

        let refused = launcher.fork_command(&launch).unwrap_err();
        assert!(
            matches!(refused, AppError::UnsupportedCapability(_)),
            "the kimi CLI cannot start a forked session: {refused}"
        );

        let stray = PathBuf::from("/elsewhere/wire.jsonl");
        assert!(
            launcher
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

    /// A rename rewrites only `state.json`, never the wire, so the cache's
    /// size-and-mtime check cannot see it. The title overlay is what carries
    /// it to the next warm load — including the generated-title slot, which
    /// restores as the summary.
    #[test]
    fn a_rename_reaches_the_next_load_through_a_warm_cache() {
        use crate::history::cache::SessionCacheStore;
        use crate::history::provider::load_sessions_with_cache;

        let home = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        let wire = write_session(home.path(), SESSION, "kimi generated title", false);

        let storage = crate::history::provider::storage::RootedStorage {
            inner: KimiStorage,
            root: SessionRoot::new(home.path().join("sessions")).in_agent_tree(),
        };
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let first = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert_eq!(first[0].summary.as_deref(), Some("kimi generated title"));
        assert_eq!(first[0].custom_title, None);

        KimiProvider.rename_session(&wire, "fresh name").unwrap();

        let second = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert_eq!(second[0].custom_title.as_deref(), Some("fresh name"));
    }

    #[test]
    fn delete_removes_the_session_directory_and_its_index_record() {
        let home = tempfile::tempdir().unwrap();
        let doomed = write_session(home.path(), SESSION, "doomed", false);
        let kept = write_session(home.path(), OTHER_SESSION, "kept", false);
        std::fs::write(
            home.path().join("session_index.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"sessionId": SESSION, "sessionDir": "x", "workDir": "/tmp/kimi-project"}),
                json!({"sessionId": OTHER_SESSION, "sessionDir": "y", "workDir": "/tmp/kimi-project"}),
            ),
        )
        .unwrap();

        KimiProvider.delete_session(&doomed).unwrap();

        let session_dir = home
            .path()
            .join("sessions/wd_kimi-project_abc123")
            .join(SESSION);
        assert!(!session_dir.exists(), "the whole session directory goes");
        assert!(kept.exists());
        let index = std::fs::read_to_string(home.path().join("session_index.jsonl")).unwrap();
        assert!(!index.contains(SESSION));
        assert!(index.contains(OTHER_SESSION));
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

    /// A sub-agent wire beside the main one, returning its path.
    fn write_subagent_wire(main_wire: &Path, agent_id: &str) -> PathBuf {
        let agent_dir = main_wire.parent().unwrap().parent().unwrap().join(agent_id);
        std::fs::create_dir_all(&agent_dir).unwrap();
        let wire = agent_dir.join("wire.jsonl");
        std::fs::copy(fixture("subagent-wire.jsonl"), &wire).unwrap();
        wire
    }

    #[test]
    fn a_session_id_resolves_to_the_main_wire_with_its_sub_agent_wires() {
        let home = tempfile::tempdir().unwrap();
        let wire = write_session(home.path(), SESSION, "kimi title", false);
        let subagent = write_subagent_wire(&wire, "agent-0");
        let root = SessionRoot::new(home.path().join("sessions"));

        let stub = session_stub_of(&root, SESSION).unwrap().unwrap();

        assert_eq!(stub.locator, wire);
        assert_eq!(stub.subagents, vec![subagent]);
    }

    #[test]
    fn a_session_id_kimi_never_recorded_resolves_to_nothing() {
        let home = tempfile::tempdir().unwrap();
        write_session(home.path(), SESSION, "kimi title", false);
        let root = SessionRoot::new(home.path().join("sessions"));

        assert_eq!(session_stub_of(&root, OTHER_SESSION).unwrap(), None);
    }

    /// One stub per session directory, the sub-agent wires named on it in
    /// agent order; the sub-agent's text reaches the row through them.
    #[test]
    fn discovery_names_each_sessions_sub_agent_wires() {
        use crate::history::cache::SessionCacheStore;
        use crate::history::provider::load_sessions_with_cache;

        let home = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        let wire = write_session(home.path(), SESSION, "kimi title", false);
        let second = write_subagent_wire(&wire, "agent-1");
        let first = write_subagent_wire(&wire, "agent-0");
        let root = SessionRoot::new(home.path().join("sessions")).in_agent_tree();

        let discovered = KimiStorage.discover(&root).unwrap();
        assert_eq!(discovered.stubs.len(), 1);
        assert_eq!(discovered.stubs[0].locator, wire);
        assert_eq!(discovered.stubs[0].subagents, vec![first, second]);

        let storage = crate::history::provider::storage::RootedStorage {
            inner: KimiStorage,
            root,
        };
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        let listed = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
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

        let session = parser::process_session_file(&stub, &kimi::KIMI_WIRE, None)
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
        let alone = parser::process_session_file(&alone, &kimi::KIMI_WIRE, None)
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

    #[test]
    fn a_file_kimi_does_not_own_survives_delete() {
        let directory = tempfile::tempdir().unwrap();
        let pi = directory.path().join("wire.jsonl");
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl"),
            &pi,
        )
        .unwrap();

        assert!(KimiProvider.delete_session(&pi).is_err());
        assert!(pi.exists());
    }
}
