//! Per-provider behavior for the coding agents whose history this browser reads.
//!
//! [`Source`] identifies which agent recorded a conversation. Everything that
//! *differs* between those agents — where sessions live, how their transcripts
//! are shaped, how to resume or rename one — is reached through the
//! [`SessionProvider`] returned by [`Source::provider`], so adding an agent means
//! adding a provider rather than editing matches scattered across the codebase.

#[cfg(test)]
mod agent_listings;
mod capability;
#[cfg(test)]
mod capability_table;
pub(crate) mod claude;
mod codex;
#[cfg(test)]
pub(crate) mod contract_tests;
mod discovery;
mod grok;
mod kimi;
mod launcher;
mod load;
#[cfg(test)]
mod markdown_outline;
mod omp;
mod opencode;
mod outcome;
mod pi;
mod sessions_dir;
pub(crate) mod sqlite;
mod storage;
pub(crate) mod subagents;
#[cfg(test)]
mod upstream_releases;
pub(crate) mod walk;

pub(crate) use capability::refusal_message;
pub use capability::{Capability, DenialDetail, Operation};
pub use discovery::{RootEnvironment, RootOrigin, SessionRoot};
pub use launcher::{SessionForker, SessionLaunch, SessionResumer};
pub use load::{
    DiscoveredSession, FoundSession, LeftOutRows, ReadError, RecordedSession, RediscoveredSessions,
    SkippedSessions, StaleCopy, apply_external_title, cache_sessions, load_session_by_id,
    load_sessions, rediscover_sessions, reread_sessions,
};
#[cfg(test)]
pub use load::{LoadedSessions, read_resolved_session};
pub use outcome::{SessionOutcome, SessionRead};
pub use sessions_dir::{SessionsDirLookup, found_sessions_dir};
#[cfg(test)]
pub(crate) use storage::RootedStorage;
pub use storage::{
    DiscoveredSessions, Fingerprint, IgnoredSessions, ResolvedSession, SessionCache,
    SessionStorage, SessionStub, SessionTitle,
};

use launcher::PathResumeLauncher;

use super::format::SessionFormat;
use super::{Source, Workspace};
use crate::agent::refs::{AgentConversationKey, AgentConversationRef};
use crate::error::{AppError, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthStr;

/// How a source is named in output. Widths matter: list rows align on `list`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceLabels {
    /// Lowercase identifier used in diagnostics and agent output (`claude`).
    pub name: &'static str,
    /// Short label shown in the TUI conversation list (`CC`).
    pub list: &'static str,
    /// The agent's name as written in prose (`Claude`).
    pub display: &'static str,
}

/// Domain separation strings mixed into the reference digests the agent CLI emits
/// and resolves, so one agent's references cannot collide with another's.
///
/// These are a compatibility contract: changing one silently invalidates every
/// reference a user has already written down.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RefNamespaces {
    pub conversation: &'static str,
    pub project: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deleted {
    pub stored: Stored,
    /// Sub-agent sessions deleted with it, each counted once however many
    /// places it was kept.
    pub subagent_sessions: usize,
}

/// The pieces that held the session itself, counted in the agent's unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stored {
    /// More than one for a Claude fork copied across projects, or for a
    /// Codex thread continued in a new file after an edited prompt.
    SessionFiles(usize),
    /// Database rows: two for an OpenCode session made before upgrading to
    /// OpenCode 2, one in each version's table.
    Copies(usize),
}

impl Deleted {
    pub const fn just_the_session() -> Self {
        Self {
            stored: Stored::SessionFiles(1),
            subagent_sessions: 0,
        }
    }
}

pub trait SessionProvider: Sync {
    fn source(&self) -> Source;
    fn labels(&self) -> SourceLabels;

    fn ref_namespaces(&self) -> RefNamespaces;

    /// How this provider finds and reads sessions under its roots.
    fn storage(&self) -> &dyn SessionStorage;

    /// Every environment variable [`SessionStorage::roots`] reads for this
    /// agent. Integration tests point each at a missing path to isolate the
    /// binary, and the contract suite fails for a variable read but not
    /// listed.
    fn storage_env_vars(&self) -> &'static [&'static str];

    /// How this provider recognizes one of its transcripts and projects it into
    /// normalized entries.
    fn format(&self) -> &dyn SessionFormat;

    /// Capable when the agent's command line resumes a session. A Capable
    /// declaration carries the provider's [`SessionResumer`].
    fn resume(&self) -> Capability<&dyn SessionResumer>;

    /// Capable when the agent's command line starts a session branching from
    /// another. A Capable declaration carries the provider's
    /// [`SessionForker`]. The first call can run the agent's binary (OpenCode
    /// runs `opencode --version`); [`fork_if_known`](Self::fork_if_known)
    /// does not.
    fn fork(&self) -> Capability<&dyn SessionForker>;

    /// The [`fork`](Self::fork) declaration when it is known without reading;
    /// `None` while `fork` still has to read. Override it where `fork` reads
    /// anything.
    fn fork_if_known(&self) -> Option<Capability<&dyn SessionForker>> {
        Some(self.fork())
    }

    /// Capable when a session can be given a user-chosen title. A Capable
    /// declaration carries the provider's [`SessionRenamer`].
    fn rename(&self) -> Capability<&dyn SessionRenamer>;

    /// Capable when a session and what the agent stores for it can be
    /// deleted. A Capable declaration carries the provider's
    /// [`SessionDeleter`].
    fn delete(&self) -> Capability<&dyn SessionDeleter>;

    /// Capable when discovery names sub-agent transcripts on their session's
    /// stub, and a delete removes and counts them. A Capable declaration
    /// carries the provider's [`SubagentFinder`].
    fn subagents(&self) -> Capability<&dyn SubagentFinder>;

    /// Capable when a sub-agent transcript's ID resolves through
    /// [`resolve_session_id`](SessionIdResolver::resolve_session_id) to a stub
    /// of its own.
    /// Capable only where [`subagents`](Self::subagents) and
    /// [`session_id_lookup`](Self::session_id_lookup) are.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "tests read it; app code does not")
    )]
    fn subagent_by_id(&self) -> Capability;

    /// Capable when a typed session ID resolves through
    /// [`is_session_id_shape`](SessionIdResolver::is_session_id_shape) and
    /// [`resolve_session_id`](SessionIdResolver::resolve_session_id). A
    /// Capable declaration carries the provider's [`SessionIdResolver`].
    fn session_id_lookup(&self) -> Capability<&dyn SessionIdResolver>;

    /// Capable when the agent stores the sessions recorded in each working
    /// directory in a directory of their own. A Capable declaration carries
    /// the lookup from a working directory to its sessions directory.
    fn workspace_sessions_dir(&self) -> Capability<SessionsDirLookup>;

    /// Capable when each shell the agent runs gets an environment variable
    /// holding its session's ID. A Capable declaration carries the variable's
    /// name.
    fn current_session_variable(&self) -> Capability<&'static str>;

    /// Every session `session_id` names, found by whatever means this provider
    /// has — reading transcript headers included.
    ///
    /// For one-shot commands rather than a keystroke, so it may cost what
    /// [`SessionIdResolver::resolve_session_id`] may not. More than one path
    /// comes back from an agent that does not keep its ids unique, and what an
    /// ambiguous id means is the caller's to decide.
    fn find_sessions_by_id(&self, session_id: &str) -> Result<Vec<PathBuf>>;

    /// The session id the locator states, for an agent that names its
    /// transcripts by session id; `None` for an agent whose ids live only
    /// inside the transcript.
    ///
    /// When this and [`ref_project`](Self::ref_project) without a directory
    /// both name a session, the agent CLI keys it without reading it.
    fn session_id_in_locator(&self, _locator: &Path) -> Option<String> {
        None
    }

    /// The project the agent CLI files the session at `path` under, given
    /// the directory the session recorded: that directory's canonical path,
    /// or `None` when the session recorded none. It goes into the session's
    /// [`conversation_ref`](Self::conversation_ref) and project id, so
    /// changing it changes references users have written down.
    fn ref_project(&self, _path: &Path, project_dir: Option<&Path>) -> Option<String> {
        let project = project_dir?;
        Some(
            project
                .canonicalize()
                .unwrap_or_else(|_| project.to_path_buf())
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// The `ch_` reference the agent CLI names `key` by: a digest of the
    /// conversation namespace, the agent's name, the project, the session id
    /// and the file name, naming the session by its id.
    fn conversation_ref(&self, key: &AgentConversationKey) -> AgentConversationRef {
        AgentConversationRef::from_digest_of(
            &[
                self.ref_namespaces().conversation,
                self.labels().name,
                &key.project_dir_name,
                &key.session_id,
                &key.session_filename,
            ],
            key.session_id.clone(),
        )
    }

    /// True when the session at `path`, which recorded the directory
    /// `project_dir`, is in `workspace`: by default, when the workspace
    /// includes that directory.
    fn is_in_workspace(
        &self,
        workspace: &Workspace,
        _path: &Path,
        project_dir: Option<&Path>,
    ) -> bool {
        project_dir.is_some_and(|directory| workspace.includes(directory))
    }
}

/// Gives a session a user-chosen title, for a provider declaring
/// [`SessionProvider::rename`] Capable.
pub trait SessionRenamer: Sync {
    /// Title the session at `path`. A format that records when a session
    /// was renamed records `renamed_at`.
    fn rename_session(&self, path: &Path, title: &str, renamed_at: DateTime<Utc>) -> Result<()>;
}

/// Deletes a session, for a provider declaring [`SessionProvider::delete`]
/// Capable.
pub trait SessionDeleter: Sync {
    /// Remove the session at `path`, its other stored pieces and its sub-agent
    /// sessions, and report the counts. Remove nothing and fail while another
    /// session shows turns from it.
    ///
    /// A sub-agent session the agent stores apart from its parent would list
    /// as a session of its own once the parent is gone, so it is deleted with
    /// the parent. The caller reports the counts rather than deleting silently.
    fn delete_session(&self, path: &Path) -> Result<Deleted>;
}

/// Names the sub-agent transcripts of one session, for a provider declaring
/// [`SessionProvider::subagents`] Capable.
pub trait SubagentFinder: Sync {
    /// The sub-agent transcripts of the session at `locator`, nested ones
    /// included, as discovery names them on the session's stub.
    fn subagent_transcripts(&self, locator: &Path) -> Result<Vec<PathBuf>>;
}

/// Looks up a typed session id, for a provider declaring
/// [`SessionProvider::session_id_lookup`] Capable.
pub trait SessionIdResolver: Sync {
    /// True when `query` has the shape of an id this provider writes, so a
    /// lookup that misses means the session is absent rather than that the
    /// query was text.
    fn is_session_id_shape(&self, query: &str) -> bool;

    /// The session `session_id` names under `root`, as the stub discovery
    /// would report for it there; `None` when it is not under `root`.
    /// Called only with a query of the session id shape.
    ///
    /// Runs on a keystroke, so it answers from the provider's own index of
    /// its sessions and must not parse a transcript in full. Where
    /// [`SessionProvider::subagent_by_id`] is Capable, a sub-agent
    /// transcript's id resolves too, to a stub of its own with its nested
    /// sub-agents: the one exception to every filter the list applies.
    fn resolve_session_id_in(
        &self,
        root: &SessionRoot,
        session_id: &str,
    ) -> Result<Option<ResolvedSession>>;
}

/// The session `session_id` names under the first of `storage`'s roots that
/// holds it, as `resolver` finds it. A query that fails
/// [`SessionIdResolver::is_session_id_shape`] resolves to `None` without a
/// lookup.
pub(crate) fn resolve_in_storage(
    resolver: &dyn SessionIdResolver,
    storage: &dyn SessionStorage,
    session_id: &str,
) -> Result<Option<ResolvedSession>> {
    if !resolver.is_session_id_shape(session_id) {
        return Ok(None);
    }
    for root in storage.roots()? {
        if let Some(resolved) = resolver.resolve_session_id_in(&root, session_id)? {
            return Ok(Some(resolved));
        }
    }
    Ok(None)
}

/// [`SessionProvider::find_sessions_by_id`] for a provider whose ids are
/// unique: the session `resolver` resolves `session_id` to in `storage`.
fn resolved_session_paths(
    resolver: &dyn SessionIdResolver,
    storage: &dyn SessionStorage,
    session_id: &str,
) -> Result<Vec<PathBuf>> {
    Ok(resolve_in_storage(resolver, storage, session_id)?
        .map(|resolved| resolved.stub.locator)
        .into_iter()
        .collect())
}

static CLAUDE: claude::ClaudeProvider = claude::ClaudeProvider;
static PI: pi::PiProvider = pi::PiProvider;
static OMP: omp::OmpProvider = omp::OmpProvider;
static CODEX: codex::CodexProvider = codex::CodexProvider;
static KIMI: kimi::KimiProvider = kimi::KimiProvider;
static GROK: grok::GrokProvider = grok::GrokProvider;
static OPENCODE: opencode::OpenCodeProvider = opencode::OpenCodeProvider;

/// Every supported provider, in the order sources are presented to the user.
static PROVIDERS: &[&dyn SessionProvider] = &[&CLAUDE, &CODEX, &OPENCODE, &KIMI, &GROK, &PI, &OMP];

pub fn providers() -> &'static [&'static dyn SessionProvider] {
    PROVIDERS
}

/// The session-id resolver of each provider declaring `session_id_lookup`
/// Capable, in registration order.
fn session_id_resolvers() -> impl Iterator<Item = (Source, &'static dyn SessionIdResolver)> {
    providers()
        .iter()
        .filter_map(|provider| match provider.session_id_lookup() {
            Capability::Capable(resolver) => Some((provider.source(), resolver)),
            Capability::NotCapable(_) => None,
        })
}

/// True when `query` has the shape of a session id some agent writes, so a
/// lookup that misses is an unknown session id rather than text to search for.
pub fn is_session_id_shape(query: &str) -> bool {
    session_id_resolvers().any(|(_, resolver)| resolver.is_session_id_shape(query))
}

/// The agent that recorded `session_id` and the session as it stored it,
/// among the storages `environment` reads.
///
/// Storages answer in presentation order, first match wins. A failing
/// storage is passed over: an unreadable directory for one agent must not
/// hide a session another agent stores.
pub fn resolve_session_id(
    environment: &crate::history::Environment,
    session_id: &str,
) -> Option<(Source, ResolvedSession)> {
    environment.storages().iter().find_map(|storage| {
        let source = storage.source();
        let Capability::Capable(resolver) = source.provider().session_id_lookup() else {
            return None;
        };
        let resolved = resolve_in_storage(resolver, *storage, session_id)
            .ok()
            .flatten()?;
        Some((source, resolved))
    })
}

/// Every session any agent stored under `session_id`.
///
/// Unlike [`resolve_session_id`] this asks every provider, since an id that
/// two of them could answer for is exactly the case a caller must not resolve
/// by guessing.
pub fn find_sessions_by_id(session_id: &str) -> Vec<(Source, PathBuf)> {
    providers()
        .iter()
        .flat_map(|provider| {
            provider
                .find_sessions_by_id(session_id)
                .unwrap_or_default()
                .into_iter()
                .map(|locator| (provider.source(), locator))
        })
        .collect()
}

/// True when `path` is a session `source` stores under `session_id`, not a
/// copy outside the agent's tree. Asks `find_sessions_by_id`, not
/// `resolve_session_id`: a provider declaring `session_id_lookup` Not Capable
/// has no resolver. False when the lookup fails.
pub fn is_stored_session(source: Source, session_id: &str, path: &Path) -> bool {
    let stored = source
        .provider()
        .find_sessions_by_id(session_id)
        .unwrap_or_default();
    contains_file(&stored, path)
}

/// The `current_session_variable` of every provider declaring it Capable.
pub fn current_session_variables() -> impl Iterator<Item = &'static str> {
    providers()
        .iter()
        .filter_map(|provider| match provider.current_session_variable() {
            Capability::Capable(variable) => Some(variable),
            Capability::NotCapable(_) => None,
        })
}

/// Delete the session at `path` through `source`'s `delete` declaration, or
/// refuse where it is Not Capable.
pub fn delete_session(source: Source, path: &Path) -> Result<Deleted> {
    let provider = source.provider();
    provider
        .delete()
        .or_refuse(provider.labels().display, Operation::Delete)?
        .delete_session(path)
}

/// The command that resumes or forks `launch`'s session through `source`'s
/// `resume` or `fork` declaration, or the refusal where it is Not Capable.
pub fn launch_command(
    source: Source,
    launch: &SessionLaunch,
    fork_session: bool,
) -> Result<std::process::Command> {
    let provider = source.provider();
    let agent = provider.labels().display;
    if fork_session {
        provider
            .fork()
            .or_refuse(agent, Operation::Fork)?
            .fork_command(launch)
    } else {
        provider
            .resume()
            .or_refuse(agent, Operation::Resume)?
            .resume_command(launch)
    }
}

/// True when one of `stored` is the file at `path`, however each was spelled.
fn contains_file(stored: &[PathBuf], path: &Path) -> bool {
    stored
        .iter()
        .any(|locator| super::format::same_file(locator, path))
}

/// Column width that keeps mixed-source list rows aligned: the widest list
/// label any registered provider can print.
pub fn list_label_column_width() -> usize {
    providers()
        .iter()
        .map(|provider| UnicodeWidthStr::width(provider.labels().list))
        .max()
        .unwrap_or(0)
}

/// Every provider named as prose would name them: `"Claude, Codex, or OMP"`. Used by
/// messages about history that is missing everywhere, which must stay accurate as
/// providers are added.
pub fn display_names_in_prose() -> String {
    let names = providers()
        .iter()
        .map(|provider| provider.labels().display)
        .collect::<Vec<_>>();
    names_in_prose(&names)
}

fn names_in_prose(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [first, last] => format!("{first} or {last}"),
        [leading @ .., last] => format!("{}, or {last}", leading.join(", ")),
    }
}

/// Replace `path`'s contents in one step, so a crash mid-write cannot leave a
/// half-written file where an agent's records used to be.
pub(crate) fn write_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        AppError::ConfigError(format!("{} has no parent directory", path.display()))
    })?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(contents)?;
    temp.as_file_mut().sync_all()?;
    temp.persist(path)
        .map_err(|error| AppError::Io(error.error))?;
    Ok(())
}

/// Rewrite the JSONL index at `index`, keeping the records `keep` accepts.
///
/// Lines that do not parse as JSON are kept — they are not this browser's to
/// judge — and a missing index is nothing to prune. The rewrite is atomic, so
/// a crash cannot leave the agent's index half-written.
pub(crate) fn retain_index_records(index: &Path, keep: impl Fn(&Value) -> bool) -> Result<()> {
    let contents = match std::fs::read_to_string(index) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut kept = contents
        .lines()
        .filter(|line| {
            serde_json::from_str::<Value>(line)
                .map(|record| keep(&record))
                .unwrap_or(true)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !kept.is_empty() {
        kept.push('\n');
    }
    write_atomically(index, kept.as_bytes())
}

impl Source {
    /// A match rather than a scan of [`PROVIDERS`], so that a new [`Source`]
    /// variant fails to compile until it is mapped here. That the two lists say
    /// the same thing is checked by `registry_and_source_lookup_agree`.
    pub fn provider(self) -> &'static dyn SessionProvider {
        match self {
            Self::Claude => &CLAUDE,
            Self::Pi => &PI,
            Self::Omp => &OMP,
            Self::Codex => &CODEX,
            Self::Kimi => &KIMI,
            Self::Grok => &GROK,
            Self::OpenCode => &OPENCODE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_and_source_lookup_agree() {
        for provider in providers() {
            assert_eq!(
                provider.source().provider().labels(),
                provider.labels(),
                "provider {} resolves to a different provider than it registers as",
                provider.labels().name
            );
        }
    }

    #[test]
    fn every_provider_looks_up_and_searches_unknown_id_without_failing() {
        let unknown = "019f0000-0000-7000-8000-00000000000a";
        for (source, resolver) in session_id_resolvers() {
            assert!(
                resolve_in_storage(resolver, source.provider().storage(), unknown).is_ok(),
                "provider {} failed a lookup for an id it does not hold",
                source.provider().labels().name
            );
        }
        for provider in providers() {
            assert!(
                provider.find_sessions_by_id(unknown).is_ok(),
                "provider {} failed a search for an id it does not hold",
                provider.labels().name
            );
        }
    }

    /// The registry's answer is the union of the providers', so a miss on a
    /// query shaped like any agent's id reports the session absent.
    #[test]
    fn query_has_session_id_shape_when_any_provider_writes_ids_like_it() {
        assert!(is_session_id_shape("019f0000-0000-7000-8000-00000000000a"));
        assert!(is_session_id_shape("ses_019b3a2f6c1eVn8tQxL0mZ4kRp"));
        assert!(is_session_id_shape("session_20260907_abcdef"));
        assert!(!is_session_id_shape("deployment"));
        assert!(!is_session_id_shape("ses_019b"));
    }

    #[test]
    fn ref_namespaces_are_unique_across_providers() {
        let registered = providers()
            .iter()
            .map(|provider| (provider.labels().name, provider.ref_namespaces()))
            .collect::<Vec<_>>();
        for (index, (name, namespace)) in registered.iter().enumerate() {
            for (other_name, other) in &registered[index + 1..] {
                assert_ne!(
                    namespace.project, other.project,
                    "providers {name} and {other_name} must have different project namespaces"
                );
                assert_ne!(
                    namespace.conversation, other.conversation,
                    "providers {name} and {other_name} must have different conversation namespaces"
                );
            }
        }
    }

    #[test]
    fn names_read_as_prose() {
        assert_eq!(names_in_prose(&[]), "");
        assert_eq!(names_in_prose(&["Alpha"]), "Alpha");
        assert_eq!(names_in_prose(&["Alpha", "Beta"]), "Alpha or Beta");
        assert_eq!(
            names_in_prose(&["Alpha", "Beta", "Gamma"]),
            "Alpha, Beta, or Gamma"
        );
    }

    #[test]
    fn registry_lists_every_source_exactly_once() {
        use strum::IntoEnumIterator;

        let registered = providers()
            .iter()
            .map(|provider| provider.source())
            .collect::<Vec<_>>();

        for source in Source::iter() {
            let entries = registered.iter().filter(|&&entry| entry == source).count();
            assert_eq!(entries, 1, "PROVIDERS lists {source:?} {entries} times");
        }
    }

    /// `directory` must never change: a new value orphans the sessions a user
    /// already has cached instead of replacing them. `schema_version` changes
    /// when the meaning of a recorded session changes, and every bump costs
    /// one cold load of that provider.
    #[test]
    fn session_cache_identities_are_pinned() {
        let pinned = [
            (Source::Claude, "claude", 19),
            (Source::Pi, "pi", 17),
            (Source::Omp, "omp", 19),
            (Source::Codex, "codex", 26),
            (Source::Kimi, "kimi", 18),
            (Source::Grok, "grok", 10),
            (Source::OpenCode, "opencode", 16),
        ];
        assert_eq!(
            pinned.len(),
            providers().len(),
            "a provider is missing from the pinned list"
        );
        for (source, directory, schema_version) in pinned {
            assert_eq!(
                source.provider().storage().cache(),
                SessionCache {
                    directory,
                    schema_version,
                },
                "{source:?} cache identity"
            );
        }
    }

    /// The load loop stamps, caches and reports under the storage's own source,
    /// so a storage that named a different one would file its sessions under a
    /// provider that never collected them.
    #[test]
    fn storage_collects_sessions_of_provider_that_offers_it() {
        for provider in providers() {
            let storage = provider.storage();
            assert_eq!(
                storage.source(),
                provider.source(),
                "provider {} offers storage for {:?}",
                provider.labels().name,
                storage.source()
            );
        }
    }

    #[test]
    fn session_caches_do_not_share_a_directory() {
        let caches = providers()
            .iter()
            .map(|provider| (provider.labels().name, provider.storage().cache()))
            .collect::<Vec<_>>();
        for (index, (name, cache)) in caches.iter().enumerate() {
            for (other_name, other) in &caches[index + 1..] {
                assert_ne!(
                    cache.directory, other.directory,
                    "providers {name} and {other_name} must have different cache directories"
                );
            }
        }
    }

    #[test]
    fn retain_index_records_drops_only_what_predicate_rejects() {
        let directory = tempfile::tempdir().unwrap();
        let index = directory.path().join("session_index.jsonl");
        std::fs::write(
            &index,
            "{\"id\":\"doomed\"}\nnot json at all\n{\"id\":\"kept\"}\n",
        )
        .unwrap();

        retain_index_records(&index, |record| {
            record.get("id").and_then(Value::as_str) != Some("doomed")
        })
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&index).unwrap(),
            "not json at all\n{\"id\":\"kept\"}\n",
            "unparseable lines are not this browser's to drop"
        );

        let missing = directory.path().join("absent.jsonl");
        retain_index_records(&missing, |_| false).unwrap();
        assert!(!missing.exists(), "a missing index is nothing to prune");
    }

    #[test]
    fn labels_are_unique() {
        for (index, provider) in providers().iter().enumerate() {
            for other in &providers()[index + 1..] {
                let (left, right) = (provider.labels(), other.labels());
                assert_ne!(
                    left.name, right.name,
                    "two providers must not share the name {:?}",
                    left.name
                );
                assert_ne!(
                    left.list, right.list,
                    "providers {} and {} must have different list labels",
                    left.name, right.name
                );
                assert_ne!(
                    left.display, right.display,
                    "providers {} and {} must have different display names",
                    left.name, right.name
                );
            }
        }
    }

    /// `subagent_by_id` needs the session-id lookup to resolve a sub-agent's
    /// id, and discovery to name the sub-agent.
    #[test]
    fn subagent_by_id_is_capable_only_where_subagents_and_session_id_lookup_are() {
        let breaking_the_rule: Vec<_> = providers()
            .iter()
            .filter(|provider| {
                provider.subagent_by_id().is_capable()
                    && !(provider.subagents().is_capable()
                        && provider.session_id_lookup().is_capable())
            })
            .map(|provider| provider.labels().name)
            .collect();

        assert!(
            breaking_the_rule.is_empty(),
            "subagent_by_id is Capable where subagents or session_id_lookup is not: \
             {breaking_the_rule:?}"
        );
    }
}
