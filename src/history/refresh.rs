//! Refreshing a loaded list: reading only the sessions new or changed since
//! it was loaded.

use super::cache::CachedFingerprint;
use super::loader::{SeenPaths, sessions_not_loaded_term};
use super::provider::{self, RediscoveredSessions, SessionRead, SessionStub, SessionTitle};
use super::{Conversation, FilterTerm, Source};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::{AppError, Result};
use crate::time_filter::TimeFilter;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;

/// A session discovery found: the agent whose discovery found it, and the
/// fingerprint (transcript size and modification time) it found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FoundSession {
    pub source: Source,
    pub fingerprint: Option<CachedFingerprint>,
}

impl FoundSession {
    /// The session `row` was read from, as discovery found it.
    pub fn of(row: &Conversation) -> Self {
        Self {
            source: row.source,
            fingerprint: row.fingerprint,
        }
    }

    /// True when `source`'s discovery finds `fingerprint` for this session
    /// again. A session without a fingerprint is always read again; a load
    /// caches no entry for it.
    fn is_unchanged(&self, source: Source, fingerprint: Option<CachedFingerprint>) -> bool {
        self.source == source && self.fingerprint.is_some() && self.fingerprint == fingerprint
    }
}

/// Every session the last load or refresh found, by locator, for a refresh to
/// compare discovery against.
#[derive(Clone, Debug, Default)]
pub struct KnownSessions {
    /// The sessions the list shows a row for.
    pub listed: HashMap<PathBuf, FoundSession>,
    /// The unlisted sessions: ones holding no conversation, ones outside the
    /// time filter, and a second agent's view of a listed file.
    pub unlisted: HashMap<PathBuf, FoundSession>,
}

impl KnownSessions {
    fn get(&self, locator: &Path) -> Option<&FoundSession> {
        self.listed
            .get(locator)
            .or_else(|| self.unlisted.get(locator))
    }

    /// True when the list shows `source`'s row for `locator`.
    fn is_listed_by(&self, source: Source, locator: &Path) -> bool {
        self.listed
            .get(locator)
            .is_some_and(|found| found.source == source)
    }
}

/// The options the list was loaded with, so a refresh reads sessions as the
/// load read them.
#[derive(Clone, Copy, Debug, Default)]
pub struct RefreshOptions {
    pub show_last: bool,
    pub debug_level: Option<DebugLevel>,
    pub time: TimeFilter,
}

/// The changes a refresh found on disk, for the list to apply.
#[derive(Default)]
pub struct SessionChanges {
    /// New sessions, and listed sessions whose transcripts changed.
    pub updated: Vec<UpdatedSession>,
    /// Listed sessions that are gone, or now hold no conversation, by path.
    pub removed: Vec<PathBuf>,
    /// The titles each agent stores beside its transcripts, by session id.
    pub external_titles: Vec<(Source, HashMap<String, SessionTitle>)>,
    /// The unlisted sessions the next refresh compares discovery against.
    pub unlisted: HashMap<PathBuf, FoundSession>,
    /// One term per reason sessions were ignored for, as a load names them.
    pub ignored: Vec<FilterTerm>,
}

/// A row to list: a new session's, or a changed session's in place of its
/// listed row.
pub struct UpdatedSession {
    pub row: Conversation,
    /// True when the row replaces one the list showed; false for a new
    /// session.
    pub replaces_listed_row: bool,
}

/// Start [`refresh_sessions`] on a thread of its own; the receiver gets its
/// one outcome.
pub fn refresh_in_background(
    known: KnownSessions,
    options: RefreshOptions,
) -> Receiver<Result<SessionChanges>> {
    let (outcome, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = outcome.send(refresh_sessions(&known, options));
    });
    receiver
}

/// The changes on disk since `known`: every agent's sessions discovered
/// again, and only new and changed ones read.
///
/// An agent whose discovery fails keeps what the list holds of it. The
/// refresh fails when every agent's discovery does.
pub(crate) fn refresh_sessions(
    known: &KnownSessions,
    options: RefreshOptions,
) -> Result<SessionChanges> {
    let sources = provider::providers()
        .iter()
        .map(|provider| provider.source())
        .collect::<Vec<_>>();
    refresh_sources(known, &sources, options, &|source, needs_reading| {
        provider::rediscover_sessions(
            source.provider().storage(),
            options.show_last,
            options.debug_level,
            needs_reading,
        )
    })
}

/// One agent's discovery, reading the sessions its second argument picks.
type Rediscover<'a> =
    dyn Fn(Source, &dyn Fn(&SessionStub) -> bool) -> Result<RediscoveredSessions> + 'a;

/// [`refresh_sessions`] over `sources`, in registry order.
fn refresh_sources(
    known: &KnownSessions,
    sources: &[Source],
    options: RefreshOptions,
    rediscover: &Rediscover<'_>,
) -> Result<SessionChanges> {
    let mut changes = SessionChanges::default();
    // Locators new to the list that an earlier agent in the registry already
    // found in this refresh. The first agent to find a file lists it.
    let mut claimed = HashSet::new();
    let mut failures = Vec::new();
    for &source in sources {
        let rediscovered = {
            let needs_reading = |stub: &SessionStub| match known.get(&stub.locator) {
                Some(found) => {
                    found.source == source && !found.is_unchanged(source, stub.fingerprint.stamp())
                }
                None => !claimed.contains(&stub.locator),
            };
            rediscover(source, &needs_reading)
        };
        match rediscovered {
            Ok(rediscovered) => {
                changes.absorb(source, rediscovered, known, &mut claimed, options.time);
            }
            Err(error) => {
                changes
                    .ignored
                    .extend(sessions_not_loaded_term(source, &error));
                changes.unlisted.extend(
                    known
                        .unlisted
                        .iter()
                        .filter(|(_, found)| found.source == source)
                        .map(|(locator, found)| (locator.clone(), *found)),
                );
                failures.push((source, error));
            }
        }
    }
    if failures.len() == sources.len() {
        return Err(failures
            .into_iter()
            .next()
            .map(|(_, error)| error)
            .unwrap_or_else(|| AppError::NoHistoryFound(provider::display_names_in_prose())));
    }
    for (source, error) in &failures {
        debug::warn(
            options.debug_level,
            &format!(
                "Failed to refresh {} history: {error}",
                source.display_label()
            ),
        );
    }
    changes.drop_rows_listed_elsewhere();
    Ok(changes)
}

impl SessionChanges {
    /// Take in what `source`'s discovery found now.
    fn absorb(
        &mut self,
        source: Source,
        rediscovered: RediscoveredSessions,
        known: &KnownSessions,
        claimed: &mut HashSet<PathBuf>,
        time: TimeFilter,
    ) {
        let mut found_now = HashMap::new();
        for (locator, fingerprint) in rediscovered.found {
            match known.get(&locator) {
                Some(found) if found.source != source => continue,
                Some(found) => {
                    let unlisted_unchanged = !known.listed.contains_key(&locator)
                        && found.is_unchanged(source, fingerprint);
                    if unlisted_unchanged {
                        self.unlisted.insert(locator.clone(), *found);
                    }
                }
                None if claimed.contains(&locator) => continue,
                None => {
                    claimed.insert(locator.clone());
                }
            }
            found_now.insert(locator, fingerprint);
        }

        for (locator, read) in rediscovered.read {
            let is_listed = known.is_listed_by(source, &locator);
            match read {
                SessionRead::Listed(row) => {
                    // A listed session stays listed as it changes; the time
                    // filter decides only whether a new one joins.
                    if is_listed || time.matches(row.timestamp) {
                        self.updated.push(UpdatedSession {
                            row: *row,
                            replaces_listed_row: is_listed,
                        });
                    } else {
                        self.unlisted.insert(locator, FoundSession::of(&row));
                    }
                }
                SessionRead::Empty => {
                    let fingerprint = found_now.get(&locator).copied().flatten();
                    self.unlisted.insert(
                        locator.clone(),
                        FoundSession {
                            source,
                            fingerprint,
                        },
                    );
                    if is_listed {
                        self.removed.push(locator);
                    }
                }
                // Keep the listed row. Its fingerprint no longer matches the
                // file, so the next refresh reads it again.
                SessionRead::Unreadable => {}
            }
        }

        self.removed.extend(
            known
                .listed
                .iter()
                .filter(|(locator, found)| {
                    found.source == source && !found_now.contains_key(*locator)
                })
                .map(|(locator, _)| locator.clone()),
        );
        if !rediscovered.external_titles.is_empty() {
            self.external_titles
                .push((source, rediscovered.external_titles));
        }
        self.ignored.extend(rediscovered.ignored);
    }

    /// A file two agents reach under two spellings of its path lists once,
    /// under the first agent in the registry. Drop a new row whose file an
    /// earlier row in this refresh already names, and record it as unlisted.
    fn drop_rows_listed_elsewhere(&mut self) {
        let mut seen = SeenPaths::default();
        let mut kept = Vec::with_capacity(self.updated.len());
        for updated in std::mem::take(&mut self.updated) {
            let first_to_name_it = seen.insert(&updated.row.path);
            if first_to_name_it || updated.replaces_listed_row {
                kept.push(updated);
            } else {
                self.unlisted
                    .insert(updated.row.path.clone(), FoundSession::of(&updated.row));
            }
        }
        self.updated = kept;
    }
}

#[cfg(test)]
mod tests;
