//! Providers that keep whole session transcripts under one or more roots.

use super::SessionRoot;
use crate::cli::DebugLevel;
use crate::error::Result;
use crate::history::cache::CachedFingerprint;
use crate::history::{Conversation, FilterTerm, Source};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

/// The stub of the session `session_id` names, under the root that holds it,
/// as [`SessionProvider::resolve_session_id`](super::SessionProvider::resolve_session_id)
/// answers. The cache-or-parse step needs the root beside the stub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSession {
    pub root: SessionRoot,
    pub stub: SessionStub,
}

/// On-disk identity of a provider's whole-root session cache.
///
/// All three fields are a compatibility contract with caches users already have:
/// `directory` names the folder under `~/.cache/rearview`, and `magic` plus
/// `schema_version` stamp the file so an incompatible one is discarded rather
/// than misread. Changing any of them silently invalidates existing caches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionCache {
    pub directory: &'static str,
    pub magic: [u8; 8],
    pub schema_version: u32,
}

/// What discovery reports about one session before it is parsed.
///
/// The load loop consumes stubs as given: it never stats, opens, or interprets
/// a locator itself, which is what lets a provider back a session with
/// something other than a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStub {
    /// Where the session lives, in whatever encoding the provider's own
    /// format, launcher, rename and delete understand. A file path for
    /// file-backed providers. Its final component names the session and feeds
    /// the agent CLI's reference digests, so it must be stable and meaningful.
    pub locator: PathBuf,
    /// The locators of every sub-agent transcript the session's agent wrote
    /// for a sub-agent thread, nested ones flattened, in the order the
    /// viewer shows them. Each parses in the session's own format and is
    /// merged into the session at parse time; none is a session of its own.
    pub subagents: Vec<PathBuf>,
    /// Cache entry key, stable while the session stays under this root.
    /// File providers use the locator relative to the root.
    pub cache_key: String,
    /// See [`Fingerprint::spanning`].
    pub fingerprint: Fingerprint,
}

/// The sessions found under one root: the stubs to load, and the ones the
/// provider ignores.
#[derive(Clone, Debug)]
pub struct DiscoveredSessions {
    pub stubs: Vec<SessionStub>,
    /// One entry per reason the provider ignores sessions for, counts of
    /// zero included.
    pub ignored: Vec<IgnoredSessions>,
    /// Transcripts the root holds that are neither sessions nor sub-agent
    /// transcripts of one — threads the agent ran for itself, which its own
    /// list excludes too. Not reported to the user as ignored, since they are
    /// not sessions the user started; the load reports the count under
    /// `--debug`.
    pub skipped: usize,
}

impl DiscoveredSessions {
    /// Every session found is loaded.
    pub fn complete(stubs: Vec<SessionStub>) -> Self {
        Self {
            stubs,
            ignored: Vec::new(),
            skipped: 0,
        }
    }
}

/// Sessions a root holds that the provider ignores, with the reason in the
/// user's words.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IgnoredSessions {
    pub count: usize,
    /// A fixed phrase the provider owns, `compressed sessions unsupported`;
    /// the core shows it and never matches on it.
    pub reason: &'static str,
}

impl IgnoredSessions {
    /// The list's term for these sessions,
    /// `Codex │ 12 ignored: compressed sessions unsupported`, or `None` when
    /// the count is zero.
    pub fn filter_term(&self, source: Source) -> Option<FilterTerm> {
        (self.count > 0).then(|| {
            FilterTerm::new(
                source.display_label(),
                format!("{} ignored: {}", self.count, self.reason),
            )
        })
    }
}

/// Change detector: a cached session is reused while its fingerprint is
/// unchanged. Deliberately the `(size, mtime)` pair the cache already stores,
/// so caches written before this type existed stay valid.
///
/// The fields need not come from a filesystem — a database-backed provider can
/// derive them from content (total payload bytes, newest row timestamp) — but
/// `size` also feeds [`SessionStorage::max_session_bytes`], and a session with
/// no `modified` is parsed on every load rather than cached.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fingerprint {
    pub size: u64,
    pub modified: Option<SystemTime>,
}

impl Fingerprint {
    /// What a cache entry for this session would be stamped with, or `None`
    /// when discovery reported no `modified` and there is nothing to validate
    /// a later entry against.
    pub fn stamp(&self) -> Option<CachedFingerprint> {
        Some(CachedFingerprint::of(self.size, self.modified?))
    }

    /// One fingerprint over a session and its sub-agent transcripts: the sizes
    /// summed and the newest `modified`, so the `(size, mtime)` stamp changes
    /// when any of them does. A session with no `modified` of its own stays
    /// uncacheable whatever its sub-agents report.
    pub fn spanning(session: Self, subagents: impl IntoIterator<Item = Self>) -> Self {
        let mut spanned = session;
        for subagent in subagents {
            spanned.size += subagent.size;
            spanned.modified = match (spanned.modified, subagent.modified) {
                (Some(newest), Some(candidate)) => Some(newest.max(candidate)),
                (newest, _) => newest,
            };
        }
        spanned
    }
}

/// A session's user-visible name, when the agent stores it outside the
/// transcript itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionTitle {
    /// Chosen by the user; restores as the conversation's custom title.
    Custom(String),
    /// Generated by the agent; restores as the conversation's summary.
    Generated(String),
}

/// How a provider finds, bounds and parses the sessions under its roots.
///
/// Claude is deliberately absent: its transcripts are partitioned by project
/// directory and loaded through machinery that predates and outgrows this shape,
/// so it reports no storage rather than pretending to fit.
pub trait SessionStorage: Sync {
    /// Whose sessions these are. A redirected root can hold a sibling agent's
    /// transcripts, so the load loop keeps only what this source claims — and
    /// stamping, caching and diagnostics all follow from here rather than from a
    /// second argument that could disagree.
    fn source(&self) -> Source;

    fn cache(&self) -> SessionCache;

    /// Every root this provider stores sessions under, in the order they
    /// should be searched.
    fn roots(&self) -> Result<Vec<SessionRoot>>;

    /// Every session under `root`, as stubs the load loop consumes without
    /// touching the sessions themselves, and what the root holds that this
    /// provider ignores.
    ///
    /// There is deliberately no default: how sessions are arranged inside a
    /// root is the provider's own knowledge. File-backed providers compose the
    /// helpers in [`walk`](super::walk).
    fn discover(&self, root: &SessionRoot) -> Result<DiscoveredSessions>;

    /// Parse one session into a conversation, its sub-agent transcripts
    /// merged in, or `None` if the locator holds nothing this provider
    /// recognizes.
    ///
    /// `root` is supplied because the user can redirect a provider's session
    /// directory outside the agent's own tree, where transcripts may be written
    /// in a sibling agent's format.
    fn parse_session(
        &self,
        stub: &SessionStub,
        root: &SessionRoot,
        debug_level: Option<DebugLevel>,
    ) -> Result<Option<Conversation>>;

    /// Largest session worth parsing, or `None` to accept any size.
    ///
    /// A cap trades completeness for a bounded worst case: a single rollout can
    /// run to hundreds of megabytes, and its parsed text is held in memory and
    /// written to the cache for the lifetime of the process.
    fn max_session_bytes(&self) -> Option<u64>;

    /// Titles stored beside the transcripts rather than in them, by session id.
    ///
    /// A rename that only rewrites such a sidecar leaves the session's
    /// fingerprint unchanged, so a cached session would keep its old name until
    /// the transcript itself changed. The load loop overlays these on every
    /// cache hit; a parse reads the sidecar itself. The default is for providers
    /// whose titles live in the transcript, where a rename already invalidates
    /// the cache.
    fn external_titles(&self, _root: &SessionRoot) -> HashMap<String, SessionTitle> {
        HashMap::new()
    }
}

/// A storage that answers as `inner` but pins its roots.
///
/// For tests of storages that resolve their roots from the process
/// environment, which tests must not touch. Extracted once codex, kimi and
/// opencode each carried a copy — the third copy their twin notes named as
/// the cue.
#[cfg(test)]
pub(crate) struct RootedStorage<S: SessionStorage> {
    pub(crate) inner: S,
    pub(crate) root: SessionRoot,
}

#[cfg(test)]
impl<S: SessionStorage> SessionStorage for RootedStorage<S> {
    fn source(&self) -> Source {
        self.inner.source()
    }

    fn cache(&self) -> SessionCache {
        self.inner.cache()
    }

    fn roots(&self) -> Result<Vec<SessionRoot>> {
        Ok(vec![self.root.clone()])
    }

    fn discover(&self, root: &SessionRoot) -> Result<DiscoveredSessions> {
        self.inner.discover(root)
    }

    fn parse_session(
        &self,
        stub: &SessionStub,
        root: &SessionRoot,
        debug_level: Option<DebugLevel>,
    ) -> Result<Option<Conversation>> {
        self.inner.parse_session(stub, root, debug_level)
    }

    fn max_session_bytes(&self) -> Option<u64> {
        self.inner.max_session_bytes()
    }

    fn external_titles(&self, root: &SessionRoot) -> HashMap<String, SessionTitle> {
        self.inner.external_titles(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn fingerprint(size: u64, modified_secs: Option<u64>) -> Fingerprint {
        Fingerprint {
            size,
            modified: modified_secs.map(|secs| UNIX_EPOCH + Duration::from_secs(secs)),
        }
    }

    /// The stamp must move when a sub-agent transcript grows after the
    /// session's own last write, or the cached row would keep the old counts.
    #[test]
    fn a_spanning_fingerprint_sums_sizes_and_keeps_the_newest_modified() {
        let spanned = Fingerprint::spanning(
            fingerprint(100, Some(1_000)),
            [fingerprint(20, Some(3_000)), fingerprint(5, Some(2_000))],
        );

        assert_eq!(spanned, fingerprint(125, Some(3_000)));
        assert_eq!(
            Fingerprint::spanning(fingerprint(100, Some(1_000)), []),
            fingerprint(100, Some(1_000)),
            "a session without sub-agents keeps its own fingerprint"
        );
    }

    #[test]
    fn a_session_without_a_modified_time_stays_uncacheable() {
        let spanned = Fingerprint::spanning(fingerprint(100, None), [fingerprint(20, Some(3_000))]);

        assert_eq!(spanned.modified, None);
        assert_eq!(spanned.stamp(), None);
    }

    #[test]
    fn ignored_sessions_make_a_term_only_when_some_were_ignored() {
        let none = IgnoredSessions {
            count: 0,
            reason: "compressed sessions unsupported",
        };
        assert_eq!(none.filter_term(Source::Codex), None);

        let some = IgnoredSessions { count: 12, ..none };
        assert_eq!(
            some.filter_term(Source::Codex),
            Some(FilterTerm::new(
                "Codex",
                "12 ignored: compressed sessions unsupported"
            ))
        );
    }
}
