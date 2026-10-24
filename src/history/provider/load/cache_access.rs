//! A read's access to the session cache, and the groups sessions parse in.

use super::*;

/// A read's access to the session cache.
#[derive(Clone, Copy)]
pub(super) enum CacheAccess<'a> {
    /// Read the database, and parse and write for this owner while it is
    /// live.
    Refresh(&'a LiveOwner<'a>),
    /// Read the database as it is through a waiting reader; parse nothing.
    AsIs,
}

impl<'a> CacheAccess<'a> {
    /// The owner whose writes this read makes; `None` without ownership,
    /// and from the moment the owner is no longer live.
    pub(super) fn owner(self) -> Option<&'a RefreshToken> {
        match self {
            Self::Refresh(owner) => owner.is_live().then(|| owner.token()),
            Self::AsIs => None,
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
    /// a failed write batch does, and the caller reads the database as it
    /// is.
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
        let Some(cache) = reader.open(self.cache_base())? else {
            return Ok(None);
        };
        if !cache.storage_is_current(self.storage.cache())? {
            return Ok(None);
        }
        Ok(Some(CacheDatabase::Reader(cache)))
    }

    /// The database `access` reads and writes: for an owner no longer live,
    /// the database as it is.
    pub(super) fn open_database_for(&self, access: CacheAccess) -> Result<Option<CacheDatabase>> {
        if let CacheAccess::Refresh(owner) = access
            && let Some(writer) = self.open_database_writer(owner.token())
        {
            return Ok(Some(writer));
        }
        self.open_database_reader(DatabaseReader::WithoutWaiting)
    }

    /// Parse `items` with `parse` in parallel, in groups of at most
    /// [`READ_BATCH_LENGTH`], and hand each group's results to `after_group`
    /// once the group's batch ends. Returns the items left unparsed.
    ///
    /// Each group asks `access`'s owner for a permit as it starts, and
    /// parses inside one writer-locked batch. Once the owner is no longer
    /// live, or a batch cannot start, the group and the rest stay unparsed.
    /// A read of the cache as it is parses nothing: its caller hands it no
    /// items.
    ///
    /// `after_group` runs outside the writer lock: a record sent to the
    /// bounded database queue under the lock could fill it and block the
    /// consumer, which needs that lock to drain it.
    pub(super) fn parse_in_groups<'i, T: Sync, Parsed: Send>(
        &self,
        access: CacheAccess<'_>,
        items: &'i [T],
        parse: impl Fn(ParsePermit<'_>, &T) -> Parsed + Sync,
        mut after_group: impl FnMut(&[T], Vec<Parsed>),
    ) -> &'i [T] {
        let CacheAccess::Refresh(owner) = access else {
            assert!(
                items.is_empty(),
                "a read of the session cache as it is gets no sessions to parse"
            );
            return items;
        };
        // The parses run on the thread pool, beyond the reach of the check
        // in `admit`.
        if !items.is_empty() {
            sqlite_cache::refuse_the_drawing_thread("parse transcripts");
        }
        let mut rest = items;
        while !rest.is_empty() {
            let Some(permit) = owner.parse_permit() else {
                return rest;
            };
            let (group, remaining) = rest.split_at(READ_BATCH_LENGTH.min(rest.len()));
            let Some((parsed, coordinated)) = owner
                .token()
                .batch_returning(|| group.par_iter().map(|item| parse(permit, item)).collect())
            else {
                return rest;
            };
            self.report_read_coordination(coordinated);
            after_group(group, parsed);
            rest = remaining;
        }
        rest
    }
}

/// The most sessions a writer-locked batch parses.
pub(super) const READ_BATCH_LENGTH: usize = 4;
