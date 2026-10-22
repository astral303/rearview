//! A read's access to the session cache: refresh ownership, every reason a
//! load or read does not write any cache, and the groups sessions parse in.

use super::*;

/// A read's access to the session cache.
#[derive(Clone, Copy)]
pub(super) enum CacheAccess<'a> {
    /// Read the database and shards, and write both for this owner while its
    /// write permission stands.
    Refresh(&'a RefreshToken),
    /// Read the database through this reader, and the shards; write neither.
    ReadOnly(DatabaseReader),
    /// Read the shards only; write nothing.
    ShardsOnly,
}

impl<'a> CacheAccess<'a> {
    /// The owner whose writes this read makes; `None` without ownership,
    /// and from the moment a failed write batch revokes the owner's write
    /// permission ([`ReadOnlyReason::WritePermissionRevoked`]).
    pub(super) fn owner(self) -> Option<&'a RefreshToken> {
        match self {
            Self::Refresh(owner) => publishing(Some(owner)),
            Self::ReadOnly(_) | Self::ShardsOnly => None,
        }
    }

    /// The access of a read that acquired ownership as `owner`, whose write
    /// permission any write batch since can have revoked.
    pub(super) fn after_acquisition(owner: &'a RefreshToken) -> Self {
        match publishing(Some(owner)) {
            Some(owner) => Self::Refresh(owner),
            None => ReadOnlyReason::WritePermissionRevoked.access(),
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
    /// Discovery could not list a directory. Keep the database row and shard
    /// entry of a session it does not find: the session may still be there.
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
    /// [`SqliteCache::open_waiting`]: loads, agent commands and the TUI's
    /// background refresh thread.
    Waiting,
    /// [`SqliteCache::open_waiting_at_most`]: a full load's probe, which
    /// spends the time left of the [`LockWait`] it shares with acquiring
    /// ownership.
    WaitingAtMost(Duration),
    /// [`InteractiveCache::open`]: the TUI's drawing thread.
    Interactive,
}

impl DatabaseReader {
    fn open(self, base: &CacheBase) -> Result<Option<SqliteCache>> {
        match self {
            Self::Waiting => SqliteCache::open_waiting(base),
            Self::WaitingAtMost(timeout) => SqliteCache::open_waiting_at_most(base, timeout),
            Self::Interactive => InteractiveCache::at(base.clone()).open(),
        }
    }
}

/// Every reason a load or read does not write any cache, and what it reads.
/// Every read reads each session the caches do not serve from its
/// transcripts. A full load reads the shards only, whatever the reason. A
/// targeted read reads what [`Self::access`] picks, as each reason names.
///
/// The database probe decides [`Self::DatabaseBusy`], and the caller
/// [`Self::DrawingThread`]. [`SessionLoader::own_refresh`] decides the
/// outcomes of acquiring ownership, and [`SessionLoader::rediscover_owned`]
/// those of discovering the root again under it. A discovery that cannot
/// list a directory is no reason: the read writes its sessions and keeps the
/// cached sessions that discovery did not find ([`Deletions::Keep`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReadOnlyReason {
    /// The database stayed busy past the reader's timeout, as during a
    /// refresh's rebuild. A targeted read reads the shards.
    DatabaseBusy,
    /// The read runs on the TUI's drawing thread, which does not acquire
    /// refresh ownership. It reads the database through the interactive
    /// reader, and the shards.
    DrawingThread,
    /// No cache directory: there is no cache to read.
    NoCacheDirectory,
    /// This process refused the base's first build. A targeted read reads
    /// the shards: the database is not opened.
    RefusedBase,
    /// Acquiring ownership failed: the writer lock stayed busy for the
    /// limit, or the database failed. A targeted read reads the database
    /// through the waiting reader, and the shards.
    AcquisitionFailed,
    /// Discovery repeated after acquiring ownership failed. The read uses the
    /// discovery from before, and reads as [`Self::AcquisitionFailed`] does.
    RediscoveryFailed,
    /// A write batch failed after acquiring ownership, revoking write
    /// permission: discovering the root again, migrating the legacy cache,
    /// reading the titles or the database's catalogue, finding a session
    /// discovery leaves out, parsing a group of sessions, or writing a shard.
    /// Sessions read before keep their writes; the rest read as
    /// [`Self::AcquisitionFailed`] does.
    WritePermissionRevoked,
}

impl ReadOnlyReason {
    /// A targeted read's cache access for this reason.
    pub(super) fn access(self) -> CacheAccess<'static> {
        match self {
            Self::DatabaseBusy => CacheAccess::ShardsOnly,
            Self::DrawingThread => CacheAccess::ReadOnly(DatabaseReader::Interactive),
            Self::NoCacheDirectory
            | Self::RefusedBase
            | Self::AcquisitionFailed
            | Self::RediscoveryFailed
            | Self::WritePermissionRevoked => CacheAccess::ReadOnly(DatabaseReader::Waiting),
        }
    }

    /// The way a read for this reason fell back from the database.
    pub(super) fn fallback(self) -> Fallback {
        match self {
            Self::DatabaseBusy => Fallback::DatabaseBusy,
            Self::DrawingThread
            | Self::NoCacheDirectory
            | Self::RefusedBase
            | Self::AcquisitionFailed
            | Self::RediscoveryFailed
            | Self::WritePermissionRevoked => Fallback::NotServed,
        }
    }
}

/// A load or read that does not write any cache: why, and the discovery repeated
/// after acquiring refresh ownership when one completed.
pub(super) struct ReadOnly {
    pub(super) reason: ReadOnlyReason,
    pub(super) rediscovered: Option<DiscoveredSessions>,
}

impl ReadOnly {
    pub(super) fn new(reason: ReadOnlyReason) -> Self {
        Self {
            reason,
            rediscovered: None,
        }
    }
}

/// Refresh ownership, and the root's discovery repeated under it.
pub(super) struct OwnedRefresh {
    pub(super) ownership: RefreshOwnership,
    pub(super) found: DiscoveredSessions,
}

impl SessionLoader<'_> {
    /// Refresh ownership of this cache's base, waiting while another refresh
    /// keeps committing and continuing `lock_wait`, and `root` discovered
    /// again under it; otherwise why the read does not write any cache.
    pub(super) fn own_refresh(
        &self,
        root: &SessionRoot,
        lock_wait: LockWait,
    ) -> std::result::Result<OwnedRefresh, ReadOnly> {
        let Some(base) = self.cache_base() else {
            return Err(ReadOnly::new(ReadOnlyReason::NoCacheDirectory));
        };
        if base.is_refused() {
            return Err(ReadOnly::new(ReadOnlyReason::RefusedBase));
        }
        match RefreshOwnership::acquire_within(base, lock_wait, || self.meter.start_refresh_wait())
        {
            Ok(ownership) => self.rediscover_owned(root, ownership),
            Err(error) => {
                debug::warn(
                    self.debug_level,
                    &format!("Failed to acquire session cache refresh ownership: {error}"),
                );
                Err(ReadOnly::new(ReadOnlyReason::AcquisitionFailed))
            }
        }
    }

    /// `root` discovered again under the writer lock for `ownership`'s
    /// refresh; otherwise why the read does not write any cache. Every
    /// outcome but success drops `ownership`.
    pub(super) fn rediscover_owned(
        &self,
        root: &SessionRoot,
        ownership: RefreshOwnership,
    ) -> std::result::Result<OwnedRefresh, ReadOnly> {
        let (found, coordinated) = ownership
            .token()
            .read(|| self.storage.discover(root, self.environment.lister()));
        let found = match found {
            Ok(found) => found,
            Err(error) => {
                debug::warn(
                    self.debug_level,
                    &format!(
                        "Failed to discover {} sessions again after acquiring refresh ownership: {error}",
                        self.storage.source().display_label()
                    ),
                );
                return Err(ReadOnly::new(ReadOnlyReason::RediscoveryFailed));
            }
        };
        if coordinated.is_ok() {
            return Ok(OwnedRefresh { ownership, found });
        }
        self.report_read_coordination(coordinated);
        Err(ReadOnly {
            reason: ReadOnlyReason::WritePermissionRevoked,
            rediscovered: Some(found),
        })
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

    fn report_read_coordination(&self, coordinated: Result<()>) {
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
    pub(super) fn open_database_writer(
        &self,
        owner: &RefreshToken,
    ) -> Result<Option<CacheDatabase>> {
        let Some(owner) = publishing(Some(owner)) else {
            return Ok(None);
        };
        if self.cache_base().is_none_or(CacheBase::is_refused) {
            return Ok(None);
        }
        let writer = SqliteCache::open_for_refresh(owner)?;
        if !writer.is_admitted()? {
            return Ok(None);
        }
        writer.admit_storage(self.storage.cache())?;
        Ok(Some(CacheDatabase::Writer(writer)))
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
            CacheAccess::Refresh(owner) => self.open_database_writer(owner),
            CacheAccess::ReadOnly(reader) => self.open_database_reader(reader),
            CacheAccess::ShardsOnly => Ok(None),
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
