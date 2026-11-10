//! A session's address in the cache and a catalogue row's content location;
//! each read opens a connection of its own, unless it is handed the cache
//! to read on.

use super::files::{self, FileKey, RootKey};
use super::roots::{Places, SessionRoots, StoredPath};
use super::{CacheBase, ReaderWait, SqliteCache, StoredEntries, entries, is_storage_current};
use crate::error::{AppError, Result};
use crate::history::TranscriptEntries;
use crate::history::fingerprint::CachedFingerprint;
use crate::history::path::FileIdentity;
use crate::history::provider::{SessionCache, SessionRoot};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A listed session's place in the cache, whichever copy it holds: the
/// provider's storage, the session's roots in the spelling of the run that
/// listed it, and its cache key. The viewer, the printout and the agent
/// commands find the newest copy at a row's address without canonicalising
/// a path; checking that copy against its transcripts still reads their
/// metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionAddress {
    pub(super) storage: SessionCache,
    pub(super) source: crate::history::Source,
    /// The session's root and the directories beside it: its files' paths
    /// join them.
    pub(super) roots: SessionRoots,
    pub(super) cache_key: String,
}

impl SessionAddress {
    pub(crate) fn new(
        storage: SessionCache,
        source: crate::history::Source,
        roots: SessionRoots,
        cache_key: String,
    ) -> Self {
        Self {
            storage,
            source,
            roots,
            cache_key,
        }
    }

    /// The identity of the session file at `path`, below the session's roots
    /// as this address spells them; `None` for a path outside them all.
    pub(crate) fn file_identity(&self, path: &Path) -> Option<FileIdentity> {
        self.roots.file_identity(path)
    }
}

/// A listed row's record of its copy in the cache. It decides whether a copy
/// at the row's address is current for the row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Publication {
    /// The row is the copy of this generation: restored from it, or
    /// written from the read that built the row.
    Published { generation: i64 },
    /// No write of the row is known to have landed. The row comes from a
    /// parse whose write failed or was withheld, a parse whose write the read
    /// did not wait for, or a copy restored as it is. `discovered` holds the
    /// sub-agent transcripts discovery found for the session.
    Unpublished { discovered: Vec<PathBuf> },
}

impl Publication {
    /// The generation the command that built the row wrote, when its write
    /// landed. That command reads a copy at or after it as current without
    /// checking the copy's transcripts, and a copy before it as older (see
    /// [`CachedSession::currency_for_publisher`]): its refresh read the
    /// transcripts, and an agent's write after that parse would otherwise
    /// make the command's own copy older.
    pub(crate) fn trusted_generation(&self) -> Option<i64> {
        match self {
            Self::Published { generation } => Some(*generation),
            Self::Unpublished { .. } => None,
        }
    }
}

/// A copy's currency for a row, as [`CachedSession::currency`] finds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Currency {
    /// The row's own copy.
    Current,
    /// A copy written after a published row, or holding every transcript
    /// discovery found for an unpublished row. The reader adopts it in
    /// place of the row, as current, with its own generation, sub-agents
    /// and catalogue row.
    Adopt {
        generation: i64,
        subagents: Vec<PathBuf>,
    },
    /// A copy whose transcripts changed since it was written, written
    /// before the row, or lacking a transcript discovery found.
    Older,
}

#[derive(Clone)]
pub struct CachedSession {
    pub(super) base: CacheBase,
    pub(super) address: SessionAddress,
    pub(super) generation: i64,
    /// The wait of this session's reads: the wait of the reader that found
    /// it.
    pub(super) wait: ReaderWait,
    /// False for a copy read as it is, whether or not its transcripts
    /// changed since it was written.
    pub(super) checks_source: bool,
}

/// A read of one copy of a session.
enum CopyRead<T> {
    Read(T),
    /// Another write replaced the copy: the cache holds another copy at its
    /// address.
    Replaced,
}

/// The copy the cache holds at a session's address, read inside one read
/// snapshot: [`CachedSession::read_held_copy`].
pub(crate) struct HeldCopy<'c> {
    connection: &'c Connection,
    session: i64,
    places: &'c Places,
    generation: i64,
}

impl HeldCopy<'_> {
    pub(crate) fn generation(&self) -> i64 {
        self.generation
    }

    /// Calls `visit` with each search text in `fields` within `window`, as
    /// [`super::search_texts::visit_texts_within`] does.
    pub(crate) fn visit_search_texts(
        &self,
        fields: super::SearchedFields,
        window: &crate::time_filter::TimeFilter,
        visit: impl FnMut(super::search_texts::SearchText<'_>) -> std::ops::ControlFlow<()>,
    ) -> Result<()> {
        super::search_texts::visit_texts_within(
            self.connection,
            self.session,
            fields,
            window,
            visit,
        )
    }

    /// Calls `visit` with each search text and semantic turn the copy
    /// stores, as [`super::search_parts::visit_search_parts`] lays them out.
    pub(crate) fn stored_search_parts(&self, visit: impl FnMut(super::SearchPart)) -> Result<()> {
        super::search_parts::visit_search_parts(self.connection, self.session, visit)
    }

    /// [`Self::stored_search_parts`]'s search texts alone.
    pub(crate) fn stored_agent_search_texts(
        &self,
        mut visit: impl FnMut(super::SearchPart),
    ) -> Result<()> {
        super::search_parts::visit_agent_search_texts(self.connection, self.session, &mut visit)
    }

    /// [`Self::stored_search_parts`]'s semantic turns alone.
    pub(crate) fn stored_semantic_turns(
        &self,
        mut visit: impl FnMut(super::SearchPart),
    ) -> Result<()> {
        super::search_parts::visit_semantic_turns(self.connection, self.session, &mut visit)
    }

    /// The agent's semantic route text stored for this copy with
    /// `custom_title` and `summary`; `None` when none is.
    pub(crate) fn stored_semantic_route(
        &self,
        custom_title: Option<&str>,
        summary: Option<&str>,
    ) -> Result<Option<String>> {
        super::semantic_routes::stored_route(
            self.connection,
            self.session,
            self.generation,
            custom_title,
            summary,
        )
    }

    /// The key a route text computed from this copy's texts is stored under.
    pub(crate) fn semantic_route_key(&self) -> super::RouteKey {
        super::RouteKey::new(self.session, self.generation)
    }

    fn message_count(&self) -> Result<usize> {
        let message_count: i64 = self
            .connection
            .prepare_cached("SELECT message_count FROM sessions WHERE id = ?1")?
            .query_row([self.session], |row| row.get(0))?;
        Ok(message_count as usize)
    }

    /// The copy's message count and the times of each message.
    pub(crate) fn message_times(&self) -> Result<(usize, crate::history::MessageTimes)> {
        let message_count = self.message_count()?;
        let times = super::read_catalogue::stored_message_times(
            self.connection,
            self.session,
            message_count,
        )?;
        Ok((message_count, times))
    }

    /// The messages numbered `ordinals`, each whole, as the viewer shows
    /// them, read through the index on the message numbers. `path` is the
    /// session's transcript.
    pub(crate) fn agent_messages(
        &self,
        path: &Path,
        ordinals: &[usize],
    ) -> Result<crate::agent::search::SearchedTranscript> {
        let stored = entries::read_messages(self.connection, self.session, self.places, ordinals)?;
        Ok(crate::agent::search::SearchedTranscript {
            transcript: crate::agent::transcript::AgentTranscript::from_stored(path, stored)?,
            message_count: self.message_count()?,
            copy_generation: Some(self.generation),
        })
    }
}

/// One copy of a session in the cache. Every write of the session gives its
/// copy another key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CopyKey {
    pub(crate) session: SessionKey,
    pub(crate) generation: i64,
}

/// A session in the cache, whichever copy it holds.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SessionKey {
    root_key: Option<RootKey>,
    cache_key: String,
}

impl CachedSession {
    /// The generation of the copy this session reads.
    pub(crate) fn generation(&self) -> i64 {
        self.generation
    }

    /// This session, reading the copy of `generation`.
    pub(crate) fn of_generation(self, generation: i64) -> Self {
        Self { generation, ..self }
    }

    pub(crate) fn address(&self) -> &SessionAddress {
        &self.address
    }

    pub(crate) fn copy_key(&self) -> CopyKey {
        CopyKey {
            session: SessionKey {
                root_key: self.address.roots.key().cloned(),
                cache_key: self.address.cache_key.clone(),
            },
            generation: self.generation,
        }
    }

    /// Other message bodies contain only the context needed to render this
    /// range's links, handles and header. Do not render outside `range`.
    pub(crate) fn agent_range(
        &self,
        path: &Path,
        range: crate::agent::refs::MessageRange,
    ) -> Result<Option<crate::agent::transcript::AgentTranscript>> {
        self.read(|connection, session, places| {
            entries::read_range(
                connection,
                session,
                places,
                path,
                range.start as u64..=range.end as u64,
            )
        })
    }

    /// This session, whose reads return the copy as the cache holds it,
    /// whether or not its transcripts changed since it was written.
    pub(crate) fn ignoring_source_changes(self) -> Self {
        Self {
            checks_source: false,
            ..self
        }
    }

    /// This session, whose reads wait for another connection's lock as the
    /// waiting reader does. A session a probe found reads without waiting,
    /// and would fail at once while a writer commits.
    pub(crate) fn waiting(self) -> Self {
        Self {
            wait: ReaderWait::Waiting(super::WAITING_BUSY_TIMEOUT),
            ..self
        }
    }

    /// This copy's currency for a row built by the reading command, whose
    /// publication is `publication`: a copy at or after the generation the
    /// command published is current without a check of its transcripts (see
    /// [`Publication::trusted_generation`]). `None` when the cache no longer
    /// holds the copy of an unpublished row.
    pub(crate) fn currency_for_publisher(
        &self,
        publication: &Publication,
    ) -> Result<Option<Currency>> {
        match publication.trusted_generation() {
            Some(generation) if self.generation >= generation => Ok(Some(Currency::Current)),
            Some(_) => Ok(Some(Currency::Older)),
            None => self.currency(publication),
        }
    }

    /// This copy's currency for a row whose publication is `publication`;
    /// `None` when the copy is no longer in the session cache. The copy's
    /// transcripts are checked whether or not this session reads its copy
    /// as it is.
    pub(crate) fn currency(&self, publication: &Publication) -> Result<Option<Currency>> {
        let Some(cache) = self.wait.open(&self.base)? else {
            return Ok(None);
        };
        Ok(self
            .read_with_currency(&cache, publication, |_, _, _| Ok(()))?
            .map(|((), currency)| currency))
    }

    /// The copy's entries as it holds them, with its currency for a row whose
    /// publication is `publication`, in one read snapshot of `cache`, as the
    /// viewer and the export read them.
    pub(crate) fn entries_with_currency(
        &self,
        cache: &SqliteCache,
        publication: &Publication,
    ) -> Result<Option<(TranscriptEntries, Currency)>> {
        self.read_with_currency(cache, publication, |connection, session, places| {
            entries::read(connection, session, places, None)
        })
    }

    /// `read`'s result over the copy, as it holds it, and the copy's currency
    /// for a row whose publication is `publication`, in one read snapshot of
    /// `cache`. The copy's transcripts are checked once, whether or not this
    /// session reads its copy as it is. `None` as for [`Self::read`].
    fn read_with_currency<T>(
        &self,
        cache: &SqliteCache,
        publication: &Publication,
        read: impl FnOnce(&Connection, i64, &Places) -> Result<T>,
    ) -> Result<Option<(T, Currency)>> {
        let as_is = Self {
            checks_source: false,
            ..self.clone()
        };
        let read = cache.with_read_snapshot(|connection| {
            as_is.read_copy_in(connection, |connection, session, places| {
                let currency = match self.validate_source(connection, places, session) {
                    Ok(()) => self.stored_currency(connection, session, places, publication)?,
                    Err(error) if error.is_changed_source() => Currency::Older,
                    Err(error) => return Err(error),
                };
                Ok((read(connection, session, places)?, currency))
            })
        })?;
        self.read_result(read)
    }

    /// The copy's currency for a row whose publication is `publication`,
    /// once its transcripts are found unchanged.
    fn stored_currency(
        &self,
        connection: &Connection,
        session: i64,
        places: &Places,
        publication: &Publication,
    ) -> Result<Currency> {
        let discovered: &[PathBuf] = match publication {
            Publication::Unpublished { discovered } => discovered,
            Publication::Published { .. } => &[],
        };
        let may_adopt = match publication {
            Publication::Published { generation } => self.generation > *generation,
            Publication::Unpublished { .. } => true,
        };
        let stored = if may_adopt {
            stored_subagents(connection, session)?
        } else {
            Vec::new()
        };
        let holds_discovered = discovered.is_empty() || {
            let stored_keys: HashSet<(i64, FileKey)> = stored
                .iter()
                .map(|subagent| (subagent.root, subagent.key()))
                .collect();
            discovered.iter().all(|path| {
                places
                    .place(path)
                    .is_some_and(|place| stored_keys.contains(&(place.root, place.key())))
            })
        };
        let adopted = || -> Result<Currency> {
            Ok(Currency::Adopt {
                generation: self.generation,
                subagents: stored
                    .into_iter()
                    .map(|subagent| places.path(subagent))
                    .collect::<Result<Vec<PathBuf>>>()?,
            })
        };
        Ok(match publication {
            Publication::Published { generation } if self.generation == *generation => {
                Currency::Current
            }
            Publication::Published { generation } if self.generation > *generation => adopted()?,
            // A copy before the row's own: the cache was rebuilt since.
            Publication::Published { .. } => Currency::Older,
            Publication::Unpublished { .. } if !holds_discovered => Currency::Older,
            Publication::Unpublished { .. } => adopted()?,
        })
    }

    pub(crate) fn entries(&self) -> Result<Option<TranscriptEntries>> {
        self.read(|connection, session, places| entries::read(connection, session, places, None))
    }

    pub(crate) fn numbered_entries(
        &self,
        messages: Option<std::ops::RangeInclusive<u64>>,
    ) -> Result<Option<StoredEntries>> {
        self.read(|connection, session, places| {
            entries::read_numbered(connection, session, places, messages)
        })
    }

    /// `read`'s result over the copy's row and the places of its roots,
    /// inside one read snapshot; `None` when the cache does not hold the
    /// copy, or the copy stores a file below a root this address lacks: a
    /// directory beside the root that did not canonicalise when the address
    /// was made. A later read that lists the session with this run's roots
    /// reads the copy.
    pub(super) fn read<T>(
        &self,
        read: impl FnOnce(&Connection, i64, &Places) -> Result<T>,
    ) -> Result<Option<T>> {
        self.read_result(self.read_copy(read)?)
    }

    /// [`Self::read`]'s result of `read`: a replaced copy fails it.
    fn read_result<T>(&self, read: Option<CopyRead<T>>) -> Result<Option<T>> {
        match read {
            Some(CopyRead::Read(result)) => Ok(Some(result)),
            Some(CopyRead::Replaced) => Err(AppError::SessionCacheNotReady {
                session: self.address.cache_key.clone(),
                reason: "catalogue generation changed".to_owned(),
            }),
            None => Ok(None),
        }
    }

    /// `read`'s result over the copy the cache holds at this copy's address,
    /// whichever generation it is, in one read snapshot: another write cannot
    /// replace the copy during the read. `None` as for [`Self::read`].
    pub(crate) fn read_held_copy<T>(
        &self,
        read: impl FnOnce(&HeldCopy<'_>) -> Result<T>,
    ) -> Result<Option<T>> {
        let Some(cache) = self.wait.open(&self.base)? else {
            return Ok(None);
        };
        cache.with_read_snapshot(|connection| {
            let Some(stored) = self.listed_in(connection)? else {
                return Ok(None);
            };
            self.read_listed(connection, stored.id, |connection, session, places| {
                read(&HeldCopy {
                    connection,
                    session,
                    places,
                    generation: stored.generation,
                })
            })
        })
    }

    /// [`Self::read`], reporting a copy another write replaced at the
    /// address.
    fn read_copy<T>(
        &self,
        read: impl FnOnce(&Connection, i64, &Places) -> Result<T>,
    ) -> Result<Option<CopyRead<T>>> {
        let Some(cache) = self.wait.open(&self.base)? else {
            return Ok(None);
        };
        cache.with_read_snapshot(|connection| self.read_copy_in(connection, read))
    }

    /// [`Self::read_copy`] on `connection`, inside its read snapshot.
    fn read_copy_in<T>(
        &self,
        connection: &Connection,
        read: impl FnOnce(&Connection, i64, &Places) -> Result<T>,
    ) -> Result<Option<CopyRead<T>>> {
        let Some(stored) = self.listed_in(connection)? else {
            return Ok(None);
        };
        if stored.generation != self.generation {
            return Ok(Some(CopyRead::Replaced));
        }
        Ok(self
            .read_listed(connection, stored.id, read)?
            .map(CopyRead::Read))
    }

    /// The listed session row at this copy's address, whichever copy it
    /// holds; `None` when the cache holds none there.
    pub(super) fn listed_in(
        &self,
        connection: &Connection,
    ) -> Result<Option<entries::StoredSession>> {
        let address = &self.address;
        if !is_storage_current(connection, address.storage)? {
            return Ok(None);
        }
        entries::listed_session_of(
            connection,
            address.storage.directory,
            address.roots.key(),
            &address.cache_key,
        )
    }

    /// `read`'s result over the session row `session` and the places of its
    /// roots; `None` when the row stores a file below a root this address
    /// lacks.
    fn read_listed<T>(
        &self,
        connection: &Connection,
        session: i64,
        read: impl FnOnce(&Connection, i64, &Places) -> Result<T>,
    ) -> Result<Option<T>> {
        let places = Places::stored(connection, &self.address.roots)?;
        let result = (|| {
            if self.checks_source {
                self.validate_source(connection, &places, session)?;
            }
            let result = read(connection, session, &places)?;
            if self.checks_source {
                self.validate_source(connection, &places, session)?;
            }
            Ok(result)
        })();
        match result {
            Ok(result) => Ok(Some(result)),
            Err(AppError::StoredRootUnknown { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn validate_source(
        &self,
        connection: &Connection,
        places: &Places,
        session: i64,
    ) -> Result<()> {
        files::validate(connection, places, session)?;
        let logical: Option<(StoredPath, CachedFingerprint)> = connection
            .query_row(
                "SELECT f.root, f.path, s.file_size, s.mtime_secs, s.mtime_nsecs FROM sessions s \
                 JOIN session_files f ON f.session = s.id \
                 WHERE s.id = ?1 AND f.kind = 'locator' AND f.file_size IS NULL",
                [session],
                |row| {
                    Ok((
                        StoredPath {
                            root: row.get(0)?,
                            path: row.get(1)?,
                        },
                        CachedFingerprint {
                            file_size: row.get::<_, i64>(2)? as u64,
                            mtime_secs: row.get::<_, i64>(3)? as u64,
                            mtime_nsecs: row.get(4)?,
                        },
                    ))
                },
            )
            .optional()?;
        if let Some((locator, expected)) = logical {
            let locator = places.path(locator)?;
            let current = self
                .address
                .source
                .provider()
                .storage()
                .current_fingerprint(&SessionRoot::new(self.address.roots.path()), &locator)?;
            if current != Some(expected) {
                return Err(AppError::SessionCacheSourceChanged(locator));
            }
        }
        Ok(())
    }
}

/// The newest copy of the listed session at `address` in the cache under
/// `base`, through [`SqliteCache::open_waiting`]; `None` when the cache does
/// not hold it.
pub(crate) fn find_waiting(
    base: &CacheBase,
    address: &SessionAddress,
) -> Result<Option<CachedSession>> {
    let Some(cache) = SqliteCache::open_waiting(base)? else {
        return Ok(None);
    };
    cache.copy_at(address)
}

/// The newest copy of the listed session at `address` in the cache under
/// `base`; `None` when the cache does not hold it. The lookup and the
/// copy's reads fail at once with a busy error while another connection
/// holds an exclusive lock, as a rebuild does: callers wait through it with
/// [`super::read_when_unlocked`].
pub(crate) fn find_copy_without_waiting(
    base: &CacheBase,
    address: &SessionAddress,
) -> Result<Option<CachedSession>> {
    let Some(cache) = SqliteCache::open_waiting_at_most(base, std::time::Duration::ZERO)? else {
        return Ok(None);
    };
    cache.copy_at(address)
}

/// [`find_copy_without_waiting`], whose reads return the copy as the cache
/// holds it, whether or not its transcripts changed since it was written.
/// The reads return the sub-agents the copy lists.
pub(crate) fn find_as_is_without_waiting(
    base: &CacheBase,
    address: &SessionAddress,
) -> Result<Option<CachedSession>> {
    Ok(find_copy_without_waiting(base, address)?.map(CachedSession::ignoring_source_changes))
}

impl SqliteCache {
    /// The newest copy of the listed session at `address`; `None` when the
    /// database holds none.
    pub(crate) fn copy_at(&self, address: &SessionAddress) -> Result<Option<CachedSession>> {
        let generation: Option<i64> = self.with_read_snapshot(|connection| {
            Ok(connection
                .query_row(
                    "SELECT generation FROM sessions WHERE storage = ?1 \
                     AND root = (SELECT id FROM roots WHERE key = ?2) AND cache_key = ?3 \
                     AND status = 'listed'",
                    params![
                        address.storage.directory,
                        address.roots.key().map(RootKey::as_str),
                        address.cache_key
                    ],
                    |row| row.get(0),
                )
                .optional()?)
        })?;
        Ok(generation.map(|generation| CachedSession {
            base: self.base().clone(),
            address: address.clone(),
            generation,
            wait: self.wait,
            checks_source: true,
        }))
    }

    /// Finds a listed session by its own transcript: `below_key` is the key
    /// of the transcript's path below `root`. Returns the session's cache
    /// key when its copy matches the transcripts as they are now, and `None`
    /// when the database doesn't hold a listed session there or holds an
    /// older copy.
    pub(crate) fn current_cache_key(
        &self,
        source: crate::history::Source,
        root: &SessionRoot,
        below_key: &str,
    ) -> Result<Option<String>> {
        let roots = self.session_roots(root);
        self.with_read_snapshot(|connection| {
            let places = Places::stored(connection, &roots)?;
            let Some(listing) = places.listing_root() else {
                return Ok(None);
            };
            let key = FileKey::of_path_below_root(Path::new(below_key));
            let Some((copy, session)) =
                self.listed_copy(connection, source, roots, listing, &key)?
            else {
                return Ok(None);
            };
            match copy.validate_source(connection, &places, session) {
                Ok(()) => Ok(Some(copy.address.cache_key)),
                Err(error) if error.is_changed_source() => Ok(None),
                Err(error) => Err(error),
            }
        })
    }

    /// The listed session `source` stores under the root of the row `root`,
    /// whose own transcript's path below that root has `key`, with its row's
    /// id. Its files are read through `roots`.
    fn listed_copy(
        &self,
        connection: &Connection,
        source: crate::history::Source,
        roots: SessionRoots,
        root: i64,
        key: &FileKey,
    ) -> Result<Option<(CachedSession, i64)>> {
        let storage = source.provider().storage().cache();
        let stored: Option<(i64, i64, String)> = connection
            .query_row(
                "SELECT s.id, s.generation, s.cache_key FROM session_files f \
                 JOIN sessions s ON s.id = f.session \
                 WHERE f.kind = 'locator' AND f.key = ?1 AND f.root = ?2 AND s.root = ?2 \
                 AND s.storage = ?3 AND s.status = 'listed' ORDER BY s.id LIMIT 1",
                params![key.as_str(), root, storage.directory],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        Ok(stored.map(|(session, generation, cache_key)| {
            (
                CachedSession {
                    base: self.base().clone(),
                    address: SessionAddress::new(storage, source, roots, cache_key),
                    generation,
                    wait: self.wait,
                    checks_source: true,
                },
                session,
            )
        }))
    }
}

/// The stored path of each sub-agent transcript the copy `session` lists,
/// in order.
fn stored_subagents(connection: &Connection, session: i64) -> Result<Vec<StoredPath>> {
    Ok(connection
        .prepare_cached(
            "SELECT root, path FROM session_files WHERE session = ?1 AND kind = 'subagent' \
             ORDER BY position",
        )?
        .query_map([session], |row| {
            Ok(StoredPath {
                root: row.get(0)?,
                path: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}
