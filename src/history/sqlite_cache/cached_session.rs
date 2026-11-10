//! A catalogue row's content location; connections stay local to each read.

use super::{
    CacheBase, ReaderWait, SqliteCache, StoredEntries, entries, files, is_storage_current,
    path_text,
};
use crate::error::Result;
use crate::history::TranscriptEntries;
use crate::history::provider::SessionCache;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct CachedSession {
    pub(super) base: CacheBase,
    pub(super) storage: SessionCache,
    pub(super) root: PathBuf,
    pub(super) cache_key: String,
    pub(super) generation: i64,
    pub(super) source: crate::history::Source,
    /// The wait of this session's reads: the wait of the reader that found
    /// it.
    pub(super) wait: ReaderWait,
    /// False for a copy read as it is, whether or not its transcripts
    /// changed since it was written.
    pub(super) checks_source: bool,
}

impl CachedSession {
    /// Other message bodies contain only the context needed to render this
    /// range's links, handles and header. Do not render outside `range`.
    pub(crate) fn agent_range(
        &self,
        path: &Path,
        range: crate::agent::refs::MessageRange,
    ) -> Result<Option<crate::agent::transcript::AgentTranscript>> {
        self.read(|connection, session| {
            entries::read_range(
                connection,
                session,
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

    /// True when the copy matches its transcripts as they are now; false when
    /// they changed since it was written, or the cache no longer holds it.
    pub(crate) fn matches_source(&self) -> Result<bool> {
        match self.read(|_, _| Ok(())) {
            Ok(found) => Ok(found.is_some()),
            Err(crate::error::AppError::SessionCacheSourceChanged(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// [`Self::matches_source`], and false as well when the copy's sub-agent
    /// transcripts are not `subagents`, the session's sub-agents now: a
    /// session that started a sub-agent since its copy was written.
    pub(crate) fn matches_transcripts(&self, subagents: &[PathBuf]) -> Result<bool> {
        let lists_subagents = self.read(|connection, session| {
            Ok(stored_subagents(connection, session)? == subagent_keys(subagents))
        });
        match lists_subagents {
            Ok(found) => Ok(found == Some(true)),
            Err(crate::error::AppError::SessionCacheSourceChanged(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn entries(&self) -> Result<Option<TranscriptEntries>> {
        self.read(|connection, session| entries::read(connection, session, None))
    }

    pub(crate) fn numbered_entries(
        &self,
        messages: Option<std::ops::RangeInclusive<u64>>,
    ) -> Result<Option<StoredEntries>> {
        self.read(|connection, session| entries::read_numbered(connection, session, messages))
    }

    pub(super) fn read<T>(
        &self,
        read: impl FnOnce(&Connection, i64) -> Result<T>,
    ) -> Result<Option<T>> {
        let Some(cache) = self.wait.open(&self.base)? else {
            return Ok(None);
        };
        cache.with_read_snapshot(|connection| {
            if !is_storage_current(connection, self.storage)? {
                return Ok(None);
            }
            let Some(session) = entries::session_id_of(
                connection,
                self.storage.directory,
                &path_text(&self.root),
                &self.cache_key,
            )?
            else {
                return Ok(None);
            };
            if session != self.generation {
                return Err(crate::error::AppError::SessionCacheNotReady {
                    session: self.cache_key.clone(),
                    reason: "catalogue generation changed".to_owned(),
                });
            }
            if self.checks_source {
                self.validate_source(connection, session)?;
            }
            let result = read(connection, session)?;
            if self.checks_source {
                self.validate_source(connection, session)?;
            }
            Ok(Some(result))
        })
    }

    fn validate_source(&self, connection: &Connection, session: i64) -> Result<()> {
        files::validate(connection, session)?;
        let logical: Option<(String, crate::history::fingerprint::CachedFingerprint)> = connection.query_row(
            "SELECT s.locator, s.file_size, s.mtime_secs, s.mtime_nsecs FROM sessions s \
             JOIN session_files f ON f.session = s.id WHERE s.id = ?1 AND f.kind = 'locator' AND f.file_size IS NULL",
            [session], |row| Ok((row.get(0)?, crate::history::fingerprint::CachedFingerprint { file_size: row.get::<_, i64>(1)? as u64, mtime_secs: row.get::<_, i64>(2)? as u64, mtime_nsecs: row.get(3)? })),
        ).optional()?;
        if let Some((locator, expected)) = logical {
            let current = self.source.provider().storage().current_fingerprint(
                &crate::history::provider::SessionRoot::new(&self.root),
                Path::new(&locator),
            )?;
            if current != Some(expected) {
                return Err(crate::error::AppError::SessionCacheSourceChanged(
                    locator.into(),
                ));
            }
        }
        Ok(())
    }
}

/// The listed session at `path` with `subagents` in the cache under `base`,
/// through [`SqliteCache::open_waiting`]; `None` when the cache does not
/// hold it.
pub(crate) fn find_waiting(
    base: &CacheBase,
    source: crate::history::Source,
    path: &Path,
    subagents: &[PathBuf],
) -> Result<Option<CachedSession>> {
    let Some(cache) = SqliteCache::open_waiting(base)? else {
        return Ok(None);
    };
    cache.find_session(source, path, subagents)
}

/// The copy of the listed session `source` stores at `path` in the cache
/// under `base`, whatever sub-agents it lists; `None` when the cache does
/// not hold it. The lookup and the copy's reads fail at once with a busy
/// error while another connection holds an exclusive lock, as a rebuild
/// does: callers wait through it with [`super::read_when_unlocked`].
pub(crate) fn find_copy_without_waiting(
    base: &CacheBase,
    source: crate::history::Source,
    path: &Path,
) -> Result<Option<CachedSession>> {
    let Some(cache) = SqliteCache::open_waiting_at_most(base, std::time::Duration::ZERO)? else {
        return Ok(None);
    };
    cache.find_copy(source, path)
}

/// [`find_copy_without_waiting`], whose reads return the copy as the cache
/// holds it, whether or not its transcripts changed since it was written.
/// The reads return the sub-agents the copy lists.
pub(crate) fn find_as_is_without_waiting(
    base: &CacheBase,
    source: crate::history::Source,
    path: &Path,
) -> Result<Option<CachedSession>> {
    Ok(find_copy_without_waiting(base, source, path)?.map(CachedSession::ignoring_source_changes))
}

impl SqliteCache {
    /// The listed session `source` stores at `path` with `subagents`;
    /// `None` when the database holds none.
    pub(crate) fn find_session(
        &self,
        source: crate::history::Source,
        path: &Path,
        subagents: &[PathBuf],
    ) -> Result<Option<CachedSession>> {
        self.with_read_snapshot(|connection| {
            let Some(copy) = self.listed_copy(connection, source, path)? else {
                return Ok(None);
            };
            let lists_subagents =
                stored_subagents(connection, copy.generation)? == subagent_keys(subagents);
            Ok(lists_subagents.then_some(copy))
        })
    }

    /// The listed session `source` stores at `path`, whatever sub-agents it
    /// lists; `None` when the database holds none.
    pub(crate) fn find_copy(
        &self,
        source: crate::history::Source,
        path: &Path,
    ) -> Result<Option<CachedSession>> {
        self.with_read_snapshot(|connection| self.listed_copy(connection, source, path))
    }

    /// The cache key of the listed session `source` stores under `root` at
    /// `path`, when its copy matches the transcripts as they are now; `None`
    /// when the database holds no listed session there, or an older copy.
    pub(crate) fn current_cache_key(
        &self,
        source: crate::history::Source,
        root: &Path,
        path: &Path,
    ) -> Result<Option<String>> {
        self.with_read_snapshot(|connection| {
            let Some(copy) = self
                .listed_copy(connection, source, path)?
                .filter(|copy| copy.root == root)
            else {
                return Ok(None);
            };
            match copy.validate_source(connection, copy.generation) {
                Ok(()) => Ok(Some(copy.cache_key)),
                Err(crate::error::AppError::SessionCacheSourceChanged(_)) => Ok(None),
                Err(error) => Err(error),
            }
        })
    }

    fn listed_copy(
        &self,
        connection: &Connection,
        source: crate::history::Source,
        path: &Path,
    ) -> Result<Option<CachedSession>> {
        let storage = source.provider().storage().cache();
        let stored: Option<(i64, String, String)> = connection.query_row(
            "SELECT id, root, cache_key FROM sessions WHERE storage = ?1 AND canonical_path = ?2 AND status = 'listed' ORDER BY id LIMIT 1",
            params![storage.directory, path_text(&files::key(path))],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        Ok(stored.map(|(generation, root, cache_key)| CachedSession {
            base: self.base().clone(),
            storage,
            root: root.into(),
            cache_key,
            generation,
            source,
            wait: self.wait,
            checks_source: true,
        }))
    }
}

/// The sub-agent transcripts the copy `session` lists, in order, by
/// [`files::key`].
fn stored_subagents(connection: &Connection, session: i64) -> Result<Vec<PathBuf>> {
    Ok(connection
        .prepare(
            "SELECT path FROM session_files WHERE session = ?1 AND kind = 'subagent' ORDER BY position",
        )?
        .query_map([session], |row| row.get::<_, String>(0).map(PathBuf::from))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn subagent_keys(subagents: &[PathBuf]) -> Vec<PathBuf> {
    subagents.iter().map(|path| files::key(path)).collect()
}
