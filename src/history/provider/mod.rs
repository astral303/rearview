//! Per-provider behavior for the coding agents whose history this browser reads.
//!
//! [`Source`] identifies which agent recorded a conversation. Everything that
//! *differs* between those agents — where sessions live, how their transcripts
//! are shaped, how to resume or rename one — is reached through the
//! [`SessionProvider`] returned by [`Source::provider`], so adding an agent means
//! adding a provider rather than editing matches scattered across the codebase.

pub(crate) mod claude;
mod codex;
#[cfg(test)]
mod contract_tests;
mod discovery;
mod kimi;
mod launcher;
mod load;
mod omp;
mod opencode;
mod pi;
pub(crate) mod sqlite;
mod storage;
pub(crate) mod subagents;
pub(crate) mod walk;

pub use discovery::{RootOrigin, SessionRoot};
pub use launcher::{SessionLaunch, SessionLauncher};
pub use load::{
    FoundSession, ReadError, RediscoveredSessions, SessionRead, SkippedSessions,
    apply_external_title, load_session_by_id, load_sessions, rediscover_sessions, reread_sessions,
};
#[cfg(test)]
pub(crate) use load::{load_sessions_with_cache, reread_session_with_cache};
pub use storage::{
    DiscoveredSessions, Fingerprint, IgnoredSessions, ResolvedSession, SessionCache,
    SessionStorage, SessionStub, SessionTitle,
};

use launcher::PathResumeLauncher;

use super::format::SessionFormat;
use super::{Source, Workspace};
use crate::agent::refs::{AgentConversationKey, AgentConversationRef};
use crate::error::{AppError, Result};
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

/// A delete's removals beyond the session the caller named: its other stored
/// copies and its sub-agent sessions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deleted {
    /// Places the session itself was kept: more than one for a Claude fork
    /// copied across projects or a Codex thread with superseded rollouts.
    pub stored_copies: usize,
    /// Sub-agent sessions deleted with it, each counted once however many
    /// places it was kept.
    pub subagent_sessions: usize,
}

impl Deleted {
    /// One stored copy and no sub-agent sessions: the session was all the
    /// agent kept.
    pub const fn just_the_session() -> Self {
        Self {
            stored_copies: 1,
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

    /// How this provider recognizes one of its transcripts and projects it into
    /// normalized entries.
    fn format(&self) -> &dyn SessionFormat;

    /// How this provider hands a session back to its agent, to resume or fork.
    fn launcher(&self) -> &dyn SessionLauncher;

    /// Give the session at `path` a user-chosen title.
    ///
    /// Every agent records titles differently — appended records, a rewritten
    /// header slot — and none of it is shared, so this is a plain method rather
    /// than another capability object.
    fn rename_session(&self, path: &Path, title: &str) -> Result<()>;

    /// Remove the session at `path`, along with whatever else the agent stores
    /// beside it — its other stored copies and its sub-agent sessions — and
    /// report the counts.
    ///
    /// A sub-agent session the agent stores apart from its parent would list
    /// as a session of its own once the parent is gone, so it is deleted with
    /// the parent. The caller reports the counts rather than deleting silently.
    fn delete_session(&self, path: &Path) -> Result<Deleted>;

    /// True when `query` has the shape of an id this provider writes, so a
    /// lookup that misses means the session is absent rather than that the
    /// query was text. A provider whose sessions resolve by id only once
    /// listed accepts nothing.
    fn is_session_id_shape(&self, query: &str) -> bool;

    /// The session `session_id` names, as the stub discovery would report for
    /// it under the root that holds it, or `None` when this provider stores no
    /// such session.
    ///
    /// Runs on a keystroke, so it answers from the provider's own index of
    /// its sessions and must not parse a transcript in full. A sub-agent
    /// transcript's id resolves too, to a stub of its own with its nested
    /// sub-agents: the one exception to every filter the list applies.
    /// A query that fails [`is_session_id_shape`](Self::is_session_id_shape)
    /// resolves to `None` without a lookup.
    fn resolve_session_id(&self, session_id: &str) -> Result<Option<ResolvedSession>>;

    /// Every session `session_id` names, found by whatever means this provider
    /// has — reading transcript headers included.
    ///
    /// For one-shot commands rather than a keystroke, so it may cost what
    /// [`resolve_session_id`](Self::resolve_session_id) may not. More than one
    /// path comes back from an agent that does not keep its ids unique, and
    /// what an ambiguous id means is the caller's to decide.
    fn find_sessions_by_id(&self, session_id: &str) -> Result<Vec<PathBuf>> {
        Ok(self
            .resolve_session_id(session_id)?
            .map(|resolved| resolved.stub.locator)
            .into_iter()
            .collect())
    }

    /// The session id the locator states, for an agent that names its
    /// transcripts by session id; `None` for an agent whose ids live only
    /// inside the transcript.
    ///
    /// When this and [`ref_project`](Self::ref_project) without a directory
    /// both name a session, the agent CLI keys it without reading it, and
    /// [`is_in_workspace`](Self::is_in_workspace) gets no directory for it.
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
    /// `project_dir`, was recorded in `workspace`: by default, when that
    /// directory is the workspace's.
    fn is_in_workspace(
        &self,
        workspace: &Workspace,
        _path: &Path,
        project_dir: Option<&Path>,
    ) -> bool {
        project_dir.is_some_and(|directory| workspace.is_directory(directory))
    }

    /// The directory this agent keeps the sessions recorded in `directory`
    /// in, for an agent that keeps one per working directory; `None` for an
    /// agent that does not.
    fn workspace_sessions_dir(&self, _directory: &Path) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    /// The environment variable this agent sets in the shells it runs,
    /// holding the session id of the session the shell belongs to; `None`
    /// for an agent that sets none.
    fn current_session_env_var(&self) -> Option<&'static str> {
        None
    }
}

static CLAUDE: claude::ClaudeProvider = claude::ClaudeProvider;
static PI: pi::PiProvider = pi::PiProvider;
static OMP: omp::OmpProvider = omp::OmpProvider;
static CODEX: codex::CodexProvider = codex::CodexProvider;
static KIMI: kimi::KimiProvider = kimi::KimiProvider;
static OPENCODE: opencode::OpenCodeProvider = opencode::OpenCodeProvider;

/// Every supported provider, in the order sources are presented to the user.
static PROVIDERS: &[&dyn SessionProvider] = &[&CLAUDE, &CODEX, &OPENCODE, &KIMI, &PI, &OMP];

pub fn providers() -> &'static [&'static dyn SessionProvider] {
    PROVIDERS
}

/// True when `query` has the shape of a session id some agent writes, so a
/// lookup that misses is an unknown session id rather than text to search for.
pub fn is_session_id_shape(query: &str) -> bool {
    providers()
        .iter()
        .any(|provider| provider.is_session_id_shape(query))
}

/// The agent that recorded `session_id` and the session as it stored it.
///
/// Providers answer in registration order, first match wins. One that fails is
/// passed over: an unreadable directory for one agent must not hide a session
/// another agent stores.
pub fn resolve_session_id(session_id: &str) -> Option<(Source, ResolvedSession)> {
    providers().iter().find_map(|provider| {
        let resolved = provider.resolve_session_id(session_id).ok().flatten()?;
        Some((provider.source(), resolved))
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
/// copy outside the agent's tree. Asks `find_sessions_by_id`, the one id
/// lookup that finds sessions for every agent: `resolve_session_id` finds none
/// for Pi or OMP. False when the lookup fails.
pub fn is_stored_session(source: Source, session_id: &str, path: &Path) -> bool {
    let stored = source
        .provider()
        .find_sessions_by_id(session_id)
        .unwrap_or_default();
    contains_file(&stored, path)
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
    match names.as_slice() {
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

    /// The default answers from the path-based lookup, so a provider that
    /// reads headers must override it or its sessions are unreachable by id
    /// from one-shot commands.
    #[test]
    fn every_provider_answering_no_path_lookup_reads_ids_some_other_way() {
        for provider in providers() {
            let resolved = provider.resolve_session_id("019f0000-0000-7000-8000-00000000000a");
            let found = provider.find_sessions_by_id("019f0000-0000-7000-8000-00000000000a");
            assert!(
                resolved.is_ok() && found.is_ok(),
                "provider {} failed a lookup for an id it does not hold",
                provider.labels().name
            );
        }
    }

    /// The registry's answer is the union of the providers', so a miss on a
    /// query shaped like any agent's id reports the session absent.
    #[test]
    fn a_query_has_a_session_id_shape_when_any_provider_writes_ids_like_it() {
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
    fn provider_names_read_as_prose() {
        assert_eq!(
            display_names_in_prose(),
            "Claude, Codex, OpenCode, Kimi, Pi, or OMP",
            "expected prose is stale; a provider was added or renamed"
        );
    }

    #[test]
    fn registry_lists_every_source_exactly_once() {
        const EVERY_SOURCE: [Source; 6] = [
            Source::Claude,
            Source::Pi,
            Source::Omp,
            Source::Codex,
            Source::Kimi,
            Source::OpenCode,
        ];

        let registered = providers()
            .iter()
            .map(|provider| provider.source())
            .collect::<Vec<_>>();
        let distinct = registered
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();

        assert_eq!(
            registered.len(),
            distinct.len(),
            "PROVIDERS must list each source once: {registered:?}"
        );
        for source in EVERY_SOURCE {
            assert!(
                distinct.contains(&source),
                "{source:?} has no entry in PROVIDERS"
            );
        }
        assert_eq!(
            registered.len(),
            EVERY_SOURCE.len(),
            "PROVIDERS and EVERY_SOURCE must list the same sources"
        );
    }

    /// `directory` and `magic` must never change: a new value orphans the file
    /// a user already has instead of replacing it. `schema_version` changes
    /// when the meaning of an entry changes, and every bump discards that
    /// provider's cache and costs one cold load.
    #[test]
    fn session_cache_identities_are_pinned() {
        let pinned = [
            (Source::Claude, "claude", *b"CLHIST02", 2),
            (Source::Pi, "pi", *b"PIHIST01", 7),
            (Source::Omp, "omp", *b"OMHIST01", 7),
            (Source::Codex, "codex", *b"CXHIST01", 9),
            (Source::Kimi, "kimi", *b"KIHIST01", 7),
            (Source::OpenCode, "opencode", *b"OCHIST01", 6),
        ];
        assert_eq!(
            pinned.len(),
            providers().len(),
            "a provider is missing from the pinned list"
        );
        for (source, directory, magic, schema_version) in pinned {
            assert_eq!(
                source.provider().storage().cache(),
                SessionCache {
                    directory,
                    magic,
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
    fn storage_collects_the_sessions_of_the_provider_that_offers_it() {
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
    fn session_caches_do_not_share_a_directory_or_magic() {
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
                assert_ne!(
                    cache.magic, other.magic,
                    "providers {name} and {other_name} must have different magic bytes"
                );
            }
        }
    }

    #[test]
    fn retain_index_records_drops_only_what_the_predicate_rejects() {
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
}
