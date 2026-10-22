//! A read's access to the session cache, and the groups sessions parse in.

use super::*;

/// A read's access to the session cache.
#[derive(Clone, Copy)]
pub(super) enum CacheAccess<'a> {
    /// Read the database, and write it for this owner while its write
    /// permission stands.
    Refresh(&'a RefreshToken),
    /// Read the database as it is through a waiting reader; parse nothing.
    AsIs,
    /// Read no cache and write nothing: every session is read from its
    /// transcripts.
    Uncached,
}

impl<'a> CacheAccess<'a> {
    /// The owner whose writes this read makes; `None` without ownership,
    /// and from the moment a failed write batch revokes the owner's write
    /// permission.
    pub(super) fn owner(self) -> Option<&'a RefreshToken> {
        match self {
            Self::Refresh(owner) => publishing(Some(owner)),
            Self::AsIs | Self::Uncached => None,
        }
    }
}

/// Deletion reconciliation: the cached sessions a load removes when its
/// discovery does not find them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Deletions {
    /// Discovery listed every directory: a session it does not find was
    /// deleted.
    Reconcile,
    /// Discovery could not list a directory. Keep the database row of a
    /// session it does not find: the session may still be there.
    Keep,
}

impl Deletions {
    pub(super) fn of(found: &DiscoveredSessions) -> Self {
        if found.unreadable_directories.is_empty() {
            Self::Reconcile
        } else {
            Self::Keep
        }
    }
}

/// `owner` while its write permission stands; `None` once revoked.
pub(super) fn publishing(owner: Option<&RefreshToken>) -> Option<&RefreshToken> {
    owner.filter(|owner| owner.can_publish())
}

/// The reader a read opens the database with when it does not write it.
#[derive(Clone, Copy)]
pub(super) enum DatabaseReader {
    /// A reader whose statements fail at once while another connection holds
    /// an exclusive lock, as a rebuild does: a probe, and a read of the cache
    /// as it is, which waits through
    /// [`OperationScope::read_when_unlocked`].
    WithoutWaiting,
    /// [`InteractiveCache::open`]: the TUI's drawing thread.
    Interactive,
}

impl DatabaseReader {
    fn open(self, base: &CacheBase) -> Result<Option<SqliteCache>> {
        match self {
            Self::WithoutWaiting => {
                SqliteCache::open_waiting_at_most(base, std::time::Duration::ZERO)
            }
            Self::Interactive => InteractiveCache::at(base.clone()).open(),
        }
    }
}

impl SessionLoader<'_> {
    /// `root` discovered again under the writer lock for `owner`'s refresh;
    /// `None` when that discovery fails, and the caller keeps the discovery
    /// from before. A write batch that fails here revokes `owner`'s write
    /// permission, and the discovery it made is still returned.
    pub(super) fn rediscover_owned(
        &self,
        root: &SessionRoot,
        owner: &RefreshToken,
    ) -> Option<DiscoveredSessions> {
        let (found, coordinated) =
            owner.read(|| self.storage.discover(root, self.environment.lister()));
        self.report_read_coordination(coordinated);
        found
            .inspect_err(|error| {
                debug::warn(
                    self.debug_level,
                    &format!(
                        "Failed to discover {} sessions again under refresh ownership: {error}",
                        self.storage.source().display_label()
                    ),
                );
            })
            .ok()
    }

    /// Run `read` holding the writer lock for `owner`'s refresh, so elapsed
    /// time cannot hand ownership to a waiter during the read. Return the
    /// read's result even when coordination fails, rather than reading
    /// twice. Without an owner whose write permission stands, run `read`
    /// unlocked.
    pub(super) fn read_under_writer_lock<T>(
        &self,
        owner: Option<&RefreshToken>,
        read: impl FnOnce() -> T,
    ) -> T {
        let Some(owner) = publishing(owner) else {
            return read();
        };
        let (result, coordinated) = owner.read(read);
        self.report_read_coordination(coordinated);
        result
    }

    pub(super) fn report_read_coordination(&self, coordinated: Result<()>) {
        if let Err(error) = coordinated {
            debug::warn(
                self.debug_level,
                &format!(
                    "Failed to hold the session cache writer lock; continuing without cache writes: {error}"
                ),
            );
        }
    }

    /// The admitted database for `owner`'s refresh, when this release's
    /// layout holds it; `None` once `owner`'s write permission is revoked.
    /// A write that fails while opening it revokes the write permission, as
    /// a failed write batch does, and the read goes on without writing.
    pub(super) fn open_database_writer(&self, owner: &RefreshToken) -> Option<CacheDatabase> {
        let owner = publishing(Some(owner))?;
        let opened = SqliteCache::open_for_refresh(owner).and_then(|writer| {
            writer.admit_storage(self.storage.cache())?;
            Ok(writer)
        });
        match opened {
            Ok(writer) => Some(CacheDatabase::Writer(writer)),
            Err(error) => {
                owner.revoke();
                debug::warn(
                    self.debug_level,
                    &format!("Failed to open the session cache for writing: {error}"),
                );
                None
            }
        }
    }

    /// The database as `reader` opens it, when it holds this storage's
    /// sessions under this release's schema.
    pub(super) fn open_database_reader(
        &self,
        reader: DatabaseReader,
    ) -> Result<Option<CacheDatabase>> {
        let Some(base) = self.cache_base() else {
            return Ok(None);
        };
        let Some(cache) = reader.open(base)? else {
            return Ok(None);
        };
        if !cache.storage_is_current(self.storage.cache())? {
            return Ok(None);
        }
        Ok(Some(CacheDatabase::Reader(cache)))
    }

    /// The database `access` reads and writes.
    pub(super) fn open_database_for(&self, access: CacheAccess) -> Result<Option<CacheDatabase>> {
        match access {
            CacheAccess::Refresh(owner) => Ok(self.open_database_writer(owner)),
            CacheAccess::AsIs => self.open_database_reader(DatabaseReader::WithoutWaiting),
            CacheAccess::Uncached => Ok(None),
        }
    }

    /// Parse `items` with `parse`, in parallel, and hand each group's results
    /// to `after_group` once the group's batch commits.
    ///
    /// While `access` has an owner whose write permission stands, each group
    /// holds at most [`READ_BATCH_LENGTH`] items and parses inside one
    /// writer-locked batch. The size is chosen as each group starts: after a
    /// revocation, the rest parse in one group without the lock. Without an
    /// owner, every item parses in one group without the lock.
    ///
    /// `after_group` runs outside the writer lock: a record sent to the
    /// bounded database queue under the lock could fill it and block the
    /// consumer, which needs that lock to drain it.
    pub(super) fn parse_in_groups<T: Sync, Parsed: Send>(
        &self,
        access: CacheAccess<'_>,
        items: &[T],
        parse: impl Fn(&T) -> Parsed + Sync,
        mut after_group: impl FnMut(&[T], Vec<Parsed>),
    ) {
        // The parses run on the thread pool, beyond the reach of the check
        // in `parse`.
        if !items.is_empty() && self.cache_base().is_some() {
            sqlite_cache::refuse_the_drawing_thread("parse transcripts beside the session cache");
        }
        let mut rest = items;
        while !rest.is_empty() {
            let group_length = match access.owner() {
                Some(_) => READ_BATCH_LENGTH.min(rest.len()),
                None => rest.len(),
            };
            let (group, remaining) = rest.split_at(group_length);
            rest = remaining;
            let parsed = self
                .read_under_writer_lock(access.owner(), || group.par_iter().map(&parse).collect());
            after_group(group, parsed);
        }
    }
}

/// The most sessions a writer-locked batch parses.
pub(super) const READ_BATCH_LENGTH: usize = 4;
