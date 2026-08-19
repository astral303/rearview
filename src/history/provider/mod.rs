//! Per-provider behavior for the coding agents whose history this browser reads.
//!
//! [`Source`] identifies which agent recorded a conversation. Everything that
//! *differs* between those agents — where sessions live, how their transcripts
//! are shaped, how to resume or rename one — is reached through the
//! [`SessionProvider`] returned by [`Source::provider`], so adding an agent means
//! adding a provider rather than editing matches scattered across the codebase.

mod claude;
mod discovery;
mod load;
mod omp;
mod pi;
mod storage;

pub use discovery::SessionRoot;
pub use load::load_sessions;
pub use storage::{SessionCache, SessionStorage};

use super::Source;
use unicode_width::UnicodeWidthStr;

/// How a source is named in output. Widths matter: list rows align on `list`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceLabels {
    /// Lowercase identifier used in diagnostics and agent output (`claude`).
    pub name: &'static str,
    /// Short label shown in the TUI conversation list (`CC`).
    pub list: &'static str,
}

pub trait SessionProvider: Sync {
    fn source(&self) -> Source;
    fn labels(&self) -> SourceLabels;

    /// How this provider finds and reads sessions under its roots, or `None` for
    /// a provider whose transcripts are organized some other way.
    fn storage(&self) -> Option<&dyn SessionStorage>;
}

static CLAUDE: claude::ClaudeProvider = claude::ClaudeProvider;
static PI: pi::PiProvider = pi::PiProvider;
static OMP: omp::OmpProvider = omp::OmpProvider;

/// Every supported provider, in the order sources are presented to the user.
static PROVIDERS: &[&dyn SessionProvider] = &[&CLAUDE, &PI, &OMP];

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

impl Source {
    pub fn provider(self) -> &'static dyn SessionProvider {
        match self {
            Self::Claude => &CLAUDE,
            Self::Pi => &PI,
            Self::Omp => &OMP,
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
                "{} resolves to a different provider than it registers as",
                provider.labels().name
            );
        }
    }

    #[test]
    fn registry_lists_every_source_exactly_once() {
        let mut sources = providers()
            .iter()
            .map(|provider| provider.source())
            .collect::<Vec<_>>();
        let registered = sources.len();
        sources.dedup();
        assert_eq!(registered, sources.len(), "a source is registered twice");
        assert_eq!(
            registered,
            [Source::Claude, Source::Pi, Source::Omp].len(),
            "a source is missing from the registry"
        );
    }

    #[test]
    fn session_cache_identities_are_pinned() {
        assert!(Source::Claude.provider().storage().is_none());
        assert_eq!(
            Source::Pi
                .provider()
                .storage()
                .map(|storage| storage.cache()),
            Some(SessionCache {
                directory: "pi",
                magic: *b"PIHIST01",
                schema_version: 1,
            }),
            "changing a cache identity discards every cache users already have"
        );
        assert_eq!(
            Source::Omp
                .provider()
                .storage()
                .map(|storage| storage.cache()),
            Some(SessionCache {
                directory: "omp",
                magic: *b"OMHIST01",
                schema_version: 1,
            }),
            "changing a cache identity discards every cache users already have"
        );
    }

    #[test]
    fn session_caches_do_not_share_a_directory_or_magic() {
        let caches = providers()
            .iter()
            .filter_map(|provider| provider.storage().map(|storage| storage.cache()))
            .collect::<Vec<_>>();
        for (index, cache) in caches.iter().enumerate() {
            for other in &caches[index + 1..] {
                assert_ne!(cache.directory, other.directory);
                assert_ne!(
                    cache.magic, other.magic,
                    "shared magic bytes let one source read another's cache"
                );
            }
        }
    }

    #[test]
    fn labels_are_unique() {
        for (index, provider) in providers().iter().enumerate() {
            for other in &providers()[index + 1..] {
                assert_ne!(provider.labels().name, other.labels().name);
                assert_ne!(provider.labels().list, other.labels().list);
            }
        }
    }
}
