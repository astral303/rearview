//! A session's address in the cache and a catalogue row's content location;
//! each read opens a connection of its own, unless it is handed the cache
//! to read on.

use super::copy_state::{CopyMiss, CopyRead, RowState};
use super::files::{self, FileKey, RootKey};
use super::roots::{Places, SessionRoots, StoredPath};
use super::{CacheBase, ReaderWait, SqliteCache, StoredEntries, entries};
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
    /// [`HeldCopy::currency_for_publisher`]): its refresh read the
    /// transcripts, and an agent's write after that parse would otherwise
    /// make the command's own copy older.
    pub(crate) fn trusted_generation(&self) -> Option<i64> {
        match self {
            Self::Published { generation } => Some(*generation),
            Self::Unpublished { .. } => None,
        }
    }
}

/// A copy's currency for a row, as [`HeldCopy::currency`] finds it.
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
}

/// The copy the cache holds at a session's address, read as it is inside one
/// read snapshot: [`SqliteCache::read_copy_at`]. Its transcripts are checked
/// when a reader asks for its [`Self::currency`].
pub(crate) struct HeldCopy<'c> {
    pub(super) connection: &'c Connection,
    session: i64,
    places: &'c Places,
    generation: i64,
    pub(super) address: &'c SessionAddress,
}

impl HeldCopy<'_> {
    pub(crate) fn generation(&self) -> i64 {
        self.generation
    }

    pub(crate) fn entries(&self) -> Result<TranscriptEntries> {
        entries::read(self.connection, self.session, self.places, None)
    }

    pub(crate) fn numbered_entries(
        &self,
        messages: Option<std::ops::RangeInclusive<u64>>,
    ) -> Result<StoredEntries> {
        entries::read_numbered(self.connection, self.session, self.places, messages)
    }

    /// Other message bodies contain only the context needed to render this
    /// range's links, handles and header. Do not render outside `range`.
    pub(crate) fn agent_range(
        &self,
        path: &Path,
        range: crate::agent::refs::MessageRange,
    ) -> Result<crate::agent::transcript::AgentTranscript> {
        entries::read_range(
            self.connection,
            self.session,
            self.places,
            path,
            range.start as u64..=range.end as u64,
        )
    }

    /// This copy's currency for a row built by the reading command, whose
    /// publication is `publication`: a copy at or after the generation the
    /// command published is current without a check of its transcripts (see
    /// [`Publication::trusted_generation`]).
    pub(crate) fn currency_for_publisher(&self, publication: &Publication) -> Result<Currency> {
        match publication.trusted_generation() {
            Some(generation) if self.generation >= generation => Ok(Currency::Current),
            Some(_) => Ok(Currency::Older),
            None => self.currency(publication),
        }
    }

    /// This copy's currency for a row whose publication is `publication`,
    /// after a check of its transcripts: a copy whose transcripts changed
    /// since it was written is older.
    pub(crate) fn currency(&self, publication: &Publication) -> Result<Currency> {
        match validate_source(self.connection, self.address, self.places, self.session) {
            Ok(()) => self.stored_currency(publication),
            Err(error) if error.is_changed_source() => Ok(Currency::Older),
            Err(error) => Err(error),
        }
    }

    /// The copy's currency for a row whose publication is `publication`,
    /// once its transcripts are found unchanged.
    fn stored_currency(&self, publication: &Publication) -> Result<Currency> {
        let discovered: &[PathBuf] = match publication {
            Publication::Unpublished { discovered } => discovered,
            Publication::Published { .. } => &[],
        };
        let may_adopt = match publication {
            Publication::Published { generation } => self.generation > *generation,
            Publication::Unpublished { .. } => true,
        };
        let stored = if may_adopt {
            stored_subagents(self.connection, self.session)?
        } else {
            Vec::new()
        };
        let holds_discovered = discovered.is_empty() || {
            let stored_keys: HashSet<(i64, FileKey)> = stored
                .iter()
                .map(|subagent| (subagent.root, subagent.key()))
                .collect();
            discovered.iter().all(|path| {
                self.places
                    .place(path)
                    .is_some_and(|place| stored_keys.contains(&(place.root, place.key())))
            })
        };
        let adopted = || -> Result<Currency> {
            Ok(Currency::Adopt {
                generation: self.generation,
                subagents: stored
                    .into_iter()
                    .map(|subagent| self.places.path(subagent))
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

/// The cache base and wait of one reader. Every copy the reader hands out
/// reads with them.
#[derive(Clone)]
pub(crate) struct CopyHandles {
    pub(super) base: CacheBase,
    pub(super) wait: ReaderWait,
}

impl CopyHandles {
    /// The copy of `generation` at `address`.
    pub(crate) fn at(&self, address: SessionAddress, generation: i64) -> CachedSession {
        CachedSession {
            base: self.base.clone(),
            address,
            generation,
            wait: self.wait,
        }
    }
}

impl CachedSession {
    pub(crate) fn generation(&self) -> i64 {
        self.generation
    }

    /// This session, reading the copy of `generation`.
    pub(crate) fn of_generation(self, generation: i64) -> Self {
        Self { generation, ..self }
    }

    /// This session, reading the cache under `base`, a file copy of its own.
    #[cfg(test)]
    pub(crate) fn in_copied_cache(self, base: CacheBase) -> Self {
        Self { base, ..self }
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

    /// This session, whose reads wait for another connection's lock as the
    /// waiting reader does. A session a probe found reads without waiting,
    /// and would fail at once while a writer commits.
    pub(crate) fn waiting(self) -> Self {
        Self {
            wait: ReaderWait::Waiting(super::WAITING_BUSY_TIMEOUT),
            ..self
        }
    }

    /// `read`'s result over the copy the cache holds at this copy's address
    /// now, whichever generation it is, in one read snapshot, as
    /// [`SqliteCache::read_copy_at`] reads it.
    pub(crate) fn read_copy<T>(
        &self,
        read: impl FnOnce(&HeldCopy<'_>) -> Result<T>,
    ) -> CopyRead<T> {
        read_copy_with(self.wait, &self.base, &self.address, read)
    }
}

/// Fails with [`AppError::SessionCacheSourceChanged`] when a transcript of
/// the copy `session`, kept at `address`, changed since the copy was written.
fn validate_source(
    connection: &Connection,
    address: &SessionAddress,
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
        let current = address
            .source
            .provider()
            .storage()
            .current_fingerprint(&SessionRoot::new(address.roots.path()), &locator)?;
        if current != Some(expected) {
            return Err(AppError::SessionCacheSourceChanged(locator));
        }
    }
    Ok(())
}

/// `read`'s result over the copy the cache under `base`, opened with
/// `wait`, holds at `address` now, as [`SqliteCache::read_copy_at`] reads
/// it.
fn read_copy_with<T>(
    wait: ReaderWait,
    base: &CacheBase,
    address: &SessionAddress,
    read: impl FnOnce(&HeldCopy<'_>) -> Result<T>,
) -> CopyRead<T> {
    match wait.open(base)? {
        Some(cache) => cache.read_copy_at(address, read),
        None => Err(CopyMiss::NotThere),
    }
}

/// [`SqliteCache::read_copy_at`] in the cache under `base`, opened through
/// [`SqliteCache::open_waiting`].
pub(crate) fn read_copy_waiting<T>(
    base: &CacheBase,
    address: &SessionAddress,
    read: impl FnOnce(&HeldCopy<'_>) -> Result<T>,
) -> CopyRead<T> {
    read_copy_with(
        ReaderWait::Waiting(super::WAITING_BUSY_TIMEOUT),
        base,
        address,
        read,
    )
}

/// [`SqliteCache::read_copy_at`] in the cache under `base`. The read fails at
/// once with a busy error while another connection holds an exclusive lock,
/// as a rebuild does: callers wait through it with
/// [`super::read_when_unlocked`].
pub(crate) fn read_copy_without_waiting<T>(
    base: &CacheBase,
    address: &SessionAddress,
    read: impl FnOnce(&HeldCopy<'_>) -> Result<T>,
) -> CopyRead<T> {
    read_copy_with(
        ReaderWait::Waiting(std::time::Duration::ZERO),
        base,
        address,
        read,
    )
}

/// `read`'s result over the copy at `address`, inside `connection`'s read
/// snapshot. The copy is not there when it stores a file below a root this
/// address lacks: a directory beside the root that did not canonicalise when
/// the address was made. A later read that lists the session with this run's
/// roots reads the copy.
pub(super) fn read_copy_in<T>(
    connection: &Connection,
    address: &SessionAddress,
    read: impl FnOnce(&HeldCopy<'_>) -> Result<T>,
) -> CopyRead<T> {
    let stored = RowState::at(connection, address)?.listed()?;
    let places = Places::stored(connection, &address.roots)?;
    let held = HeldCopy {
        connection,
        session: stored.id,
        places: &places,
        generation: stored.generation,
        address,
    };
    read(&held).map_err(|error| match error {
        AppError::StoredRootUnknown { .. } => CopyMiss::NotThere,
        error => CopyMiss::ReadError(error),
    })
}

impl SqliteCache {
    /// `read`'s result over the copy this cache holds at `address` now,
    /// whichever generation it is, as it holds it, in one read snapshot:
    /// another write cannot replace the copy during the read.
    pub(crate) fn read_copy_at<T>(
        &self,
        address: &SessionAddress,
        read: impl FnOnce(&HeldCopy<'_>) -> Result<T>,
    ) -> CopyRead<T> {
        self.with_read_snapshot(|connection| Ok(read_copy_in(connection, address, read)))?
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
            let Some((address, session)) = listed_copy(connection, source, roots, listing, &key)?
            else {
                return Ok(None);
            };
            match validate_source(connection, &address, &places, session) {
                Ok(()) => Ok(Some(address.cache_key)),
                Err(error) if error.is_changed_source() => Ok(None),
                Err(error) => Err(error),
            }
        })
    }
}

/// The address of the listed copy `source` stores under the root of the row
/// `root`, whose own transcript's path below that root has `key`, with its
/// row's id. Its files are read through `roots`.
fn listed_copy(
    connection: &Connection,
    source: crate::history::Source,
    roots: SessionRoots,
    root: i64,
    key: &FileKey,
) -> Result<Option<(SessionAddress, i64)>> {
    let storage = source.provider().storage().cache();
    let stored: Option<(i64, String)> = connection
        .query_row(
            &format!(
                "SELECT s.id, s.cache_key FROM session_files f \
                 JOIN sessions s ON s.id = f.session \
                 WHERE f.kind = 'locator' AND f.key = ?1 AND f.root = ?2 AND s.root = ?2 \
                 AND s.storage = ?3 AND {} ORDER BY s.id LIMIT 1",
                super::copy_state::LISTED_COPY
            ),
            params![key.as_str(), root, storage.directory],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(stored.map(|(session, cache_key)| {
        (
            SessionAddress::new(storage, source, roots, cache_key),
            session,
        )
    }))
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
