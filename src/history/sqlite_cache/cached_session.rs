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
    /// The clock of the rearview that found this session.
    pub(crate) fn clock(&self) -> &dyn crate::clock::Clock {
        self.base.clock().as_ref()
    }

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

    /// True when the copy matches its transcripts as they are now; false when
    /// they changed since it was written, or the cache no longer holds it.
    pub(crate) fn matches_source(&self) -> Result<bool> {
        match self.read(|_, _| Ok(())) {
            Ok(found) => Ok(found.is_some()),
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

/// [`find_waiting`], whose reads return the copy as the cache holds it,
/// whether or not its transcripts changed since it was written.
pub(crate) fn find_as_is(
    base: &CacheBase,
    source: crate::history::Source,
    path: &Path,
    subagents: &[PathBuf],
) -> Result<Option<CachedSession>> {
    Ok(find_waiting(base, source, path, subagents)?.map(CachedSession::ignoring_source_changes))
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
            let stored: Option<(i64, String, String)> = connection.query_row(
                "SELECT id, root, cache_key FROM sessions WHERE storage = ?1 AND canonical_path = ?2 AND status = 'listed' ORDER BY id LIMIT 1",
                params![source.provider().storage().cache().directory, path_text(&files::key(path))],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional()?;
            let Some((generation, root, cache_key)) = stored else { return Ok(None) };
            let stored = connection.prepare("SELECT path FROM session_files WHERE session = ?1 AND kind = 'subagent' ORDER BY position")?
                .query_map([generation], |row| row.get::<_, String>(0).map(PathBuf::from))?.collect::<rusqlite::Result<Vec<_>>>()?;
            if stored != subagents.iter().map(|path| files::key(path)).collect::<Vec<_>>() { return Ok(None); }
            Ok(Some(CachedSession { base: self.base().clone(), storage: source.provider().storage().cache(), root: root.into(), cache_key, generation, source, wait: self.wait, checks_source: true }))
        })
    }
}
