//! Per-provider behavior for the coding agents whose history this browser reads.
//!
//! [`Source`] identifies which agent recorded a conversation. Everything that
//! *differs* between those agents — where sessions live, how their transcripts
//! are shaped, how to resume or rename one — is reached through the
//! [`SessionProvider`] returned by [`Source::provider`], so adding an agent means
//! adding a provider rather than editing matches scattered across the codebase.

mod claude;
mod codex;
mod discovery;
mod kimi;
mod launcher;
mod load;
mod omp;
mod opencode;
mod pi;
mod storage;
pub(crate) mod walk;

pub(crate) use claude::assign_canonical_tools;
pub use discovery::{RootOrigin, SessionRoot};
pub use launcher::{SessionLaunch, SessionLauncher};
pub(crate) use load::fold_targets;
pub use load::load_sessions;
#[cfg(test)]
pub(crate) use load::load_sessions_with_cache;
pub use storage::{Fingerprint, SessionCache, SessionStorage, SessionStub, SessionTitle};

use launcher::PathResumeLauncher;

use super::Source;
use super::format::SessionFormat;
use crate::error::{AppError, Result};
use serde_json::Value;
use std::io::Write;
use std::path::Path;
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
    /// `None` for Claude, whose conversation references predate per-source
    /// digests and are derived from the project directory and session filename.
    pub conversation: Option<&'static str>,
    pub project: &'static str,
}

pub trait SessionProvider: Sync {
    fn source(&self) -> Source;
    fn labels(&self) -> SourceLabels;

    fn ref_namespaces(&self) -> RefNamespaces;

    /// How this provider finds and reads sessions under its roots, or `None` for
    /// a provider whose transcripts are organized some other way.
    fn storage(&self) -> Option<&dyn SessionStorage>;

    /// How this provider recognizes one of its transcripts and projects it into
    /// normalized entries, or `None` for a provider whose files carry no session
    /// header to project from.
    fn format(&self) -> Option<&dyn SessionFormat>;

    /// How this provider hands a session back to its agent, to resume or fork.
    fn launcher(&self) -> &dyn SessionLauncher;

    /// Give the session at `path` a user-chosen title.
    ///
    /// Every agent records titles differently — appended records, a rewritten
    /// header slot — and none of it is shared, so this is a plain method rather
    /// than another capability object.
    fn rename_session(&self, path: &Path, title: &str) -> Result<()>;

    /// Remove the session at `path`, along with whatever else the agent stores
    /// beside it.
    fn delete_session(&self, path: &Path) -> Result<()>;
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
                if let (Some(left), Some(right)) = (namespace.conversation, other.conversation) {
                    assert_ne!(
                        left, right,
                        "providers {name} and {other_name} must have different conversation namespaces"
                    );
                }
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
        assert!(
            Source::Claude.provider().storage().is_none(),
            "Claude caches per project directory, not per session root"
        );
        let pinned = [
            (Source::Pi, "pi", *b"PIHIST01", 3),
            (Source::Omp, "omp", *b"OMHIST01", 3),
            (Source::Codex, "codex", *b"CXHIST01", 3),
            (Source::Kimi, "kimi", *b"KIHIST01", 3),
            (Source::OpenCode, "opencode", *b"OCHIST01", 2),
        ];
        assert_eq!(
            pinned.len(),
            providers()
                .iter()
                .filter(|provider| provider.storage().is_some())
                .count(),
            "a provider with a session cache is missing from the pinned list"
        );
        for (source, directory, magic, schema_version) in pinned {
            assert_eq!(
                source.provider().storage().map(|storage| storage.cache()),
                Some(SessionCache {
                    directory,
                    magic,
                    schema_version,
                }),
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
            let Some(storage) = provider.storage() else {
                continue;
            };
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
            .filter_map(|provider| {
                provider
                    .storage()
                    .map(|storage| (provider.labels().name, storage.cache()))
            })
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
