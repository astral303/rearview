//! Targeted reads: the sessions at given files, or the stubs a refresh
//! picks, read through the cache.

use super::cache_access::{DatabaseReader, publishing};
use super::*;

/// The two ways the TUI reads sessions at given files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TargetedRead {
    /// On the drawing thread: does not acquire refresh ownership or write any
    /// cache. A session read outside the database carries its
    /// [`InteractiveCache::follow_up`].
    Interactive,
    /// On the TUI's background refresh thread: acquires ownership as a load
    /// does, and caches each session the database does not hold.
    Caching,
}

/// The reads of [`SessionLoader::read_stubs`], with the discovery and titles
/// it read the sessions against.
pub(super) struct SessionReads {
    pub(super) read: Vec<(PathBuf, SessionRead)>,
    /// The root's discovery repeated after acquiring refresh ownership;
    /// `None` when the reads used the caller's discovery.
    pub(super) rediscovered: Option<DiscoveredSessions>,
    pub(super) external_titles: HashMap<String, SessionTitle>,
    /// The sessions read outside the database.
    outside_database: HashSet<PathBuf>,
    /// The follow-up of the sessions in `outside_database`.
    follow_up: Option<CacheFollowUp>,
}

impl SessionReads {
    /// Reads made against the caller's discovery and titles, with nothing
    /// to follow up.
    fn against_caller(
        read: Vec<(PathBuf, SessionRead)>,
        external_titles: &HashMap<String, SessionTitle>,
    ) -> Self {
        Self {
            read,
            rediscovered: None,
            external_titles: external_titles.clone(),
            outside_database: HashSet::new(),
            follow_up: None,
        }
    }

    /// These reads, with `follow_up`'s follow-up for the sessions read
    /// outside the database. `follow_up` runs only when there are some.
    fn followed_up(mut self, follow_up: impl FnOnce() -> Option<CacheFollowUp>) -> Self {
        if !self.outside_database.is_empty() {
            self.follow_up = follow_up();
        }
        self
    }

    /// The follow-up of the session at `locator`; `None` when the database
    /// served it, or nothing caches it.
    pub(super) fn follow_up_of(&self, locator: &Path) -> Option<CacheFollowUp> {
        self.follow_up
            .filter(|_| self.outside_database.contains(locator))
    }
}

/// The outcome of restoring every requested session from the database.
enum Probe {
    /// One read per requested session, in order.
    Read(Vec<(PathBuf, SessionRead)>),
    /// The database does not hold one of the sessions as it is now.
    Missed,
    /// The database stayed busy past the read's timeout.
    Busy,
}

/// The reads of [`SessionLoader::read_stubs_attempt`], one per stub, and the
/// sessions among them read outside the database.
#[derive(Default)]
struct AttemptReads {
    read: Vec<(PathBuf, SessionRead)>,
    outside_database: HashSet<PathBuf>,
}

/// The stubs [`SessionLoader::read_stubs`] reads, picked from the discovery
/// it repeats after acquiring refresh ownership.
#[derive(Clone, Copy)]
pub(super) enum StubsAfterRediscovery<'a> {
    /// The files the requested stubs name, however discovery spells their
    /// paths. A requested stub that discovery leaves out is found through
    /// [`SessionStorage::rediscover_unlisted_session`].
    SameFiles,
    /// Every stub the predicate picks.
    Picked(&'a dyn Fn(&SessionStub) -> bool),
}

impl SessionLoader<'_> {
    /// The sessions at `locators`, one result each in order, read through
    /// the cache as `targeted_read` selects. `None` for a file neither
    /// discovery nor [`SessionStorage::rediscover_unlisted_session`] finds.
    pub(super) fn reread(
        &self,
        locators: &[&Path],
        targeted_read: TargetedRead,
    ) -> Vec<InteractiveRead<Option<SessionRead>>> {
        let transient =
            || InteractiveRead::settled(Some(SessionRead::Failed(ReadError::Transient)));
        let Ok(discovered) = self.discover_every_root() else {
            return locators.iter().map(|_| transient()).collect();
        };
        let mut reads: Vec<Option<InteractiveRead<Option<SessionRead>>>> =
            locators.iter().map(|_| None).collect();
        let mut picked: Vec<Vec<(usize, SessionStub)>> =
            discovered.iter().map(|_| Vec::new()).collect();
        let mut unlisted = Vec::new();
        for (index, locator) in locators.iter().enumerate() {
            match find_listed(&discovered, locator) {
                Some(Listing::Listed(root_index, stub)) => picked[root_index].push((index, stub)),
                Some(Listing::UnreadableDirectory) => reads[index] = Some(transient()),
                None => unlisted.push(index),
            }
        }
        for index in unlisted {
            match self.find_unlisted(&discovered, locators[index]) {
                Ok(Some((root_index, stub))) => picked[root_index].push((index, stub)),
                Ok(None) => reads[index] = Some(InteractiveRead::settled(None)),
                Err(error) => {
                    debug::warn(
                        self.debug_level,
                        &format!(
                            "Failed to find session {} again: {error}",
                            locators[index].display()
                        ),
                    );
                    reads[index] = Some(transient());
                }
            }
        }
        for ((root, _), picked) in discovered.iter().zip(picked) {
            if picked.is_empty() {
                continue;
            }
            let external_titles = self.storage.external_titles(root);
            let stubs = picked.iter().map(|(_, stub)| stub.clone()).collect();
            let reads_under_root = match targeted_read {
                TargetedRead::Interactive => {
                    self.read_stubs_interactively(root, &external_titles, stubs)
                }
                TargetedRead::Caching => self.read_stubs(
                    root,
                    &external_titles,
                    stubs,
                    StubsAfterRediscovery::SameFiles,
                ),
            };
            let follow_ups: Vec<_> = picked
                .iter()
                .map(|(_, stub)| reads_under_root.follow_up_of(&stub.locator))
                .collect();
            let mut read: HashMap<PathBuf, SessionRead> =
                reads_under_root.read.into_iter().collect();
            for ((index, stub), follow_up) in picked.into_iter().zip(follow_ups) {
                reads[index] = Some(InteractiveRead {
                    value: read.remove(&stub.locator),
                    follow_up,
                });
            }
        }
        reads
            .into_iter()
            .map(|read| read.unwrap_or_else(|| InteractiveRead::settled(None)))
            .collect()
    }

    /// The session at `locator` that every root's discovery in `discovered`
    /// leaves out, such as a sub-agent opened by its ID, with the index of
    /// the first root whose provider finds it; `None` when no root's does.
    /// Fails when a lookup fails and no root finds the session.
    fn find_unlisted(
        &self,
        discovered: &[(SessionRoot, DiscoveredSessions)],
        locator: &Path,
    ) -> Result<Option<(usize, SessionStub)>> {
        let mut failure = None;
        for (root_index, (root, _)) in discovered.iter().enumerate() {
            match self.rediscover_same_file(root, locator) {
                Ok(Some(stub)) => return Ok(Some((root_index, stub))),
                Ok(None) => {}
                Err(error) => failure = Some(error),
            }
        }
        failure.map_or(Ok(None), Err)
    }

    /// The stub of the session at `locator` under `root`, found through
    /// [`SessionStorage::rediscover_unlisted_session`]; `None` when the
    /// provider finds no session there, or names another file.
    fn rediscover_same_file(
        &self,
        root: &SessionRoot,
        locator: &Path,
    ) -> Result<Option<SessionStub>> {
        Ok(self
            .storage
            .rediscover_unlisted_session(root, locator, self.environment.lister())?
            .filter(|stub| same_file(&stub.locator, locator)))
    }

    /// One session under `root`, read as the TUI's drawing thread reads it
    /// for [`load_session_by_id`].
    pub(super) fn read_one(
        &self,
        root: &SessionRoot,
        stub: SessionStub,
    ) -> InteractiveRead<SessionRead> {
        let external_titles = self.storage.external_titles(root);
        let mut reads = self.read_stubs_interactively(root, &external_titles, vec![stub]);
        let Some((locator, read)) = reads.read.pop() else {
            return InteractiveRead::settled(SessionRead::Failed(ReadError::Transient));
        };
        InteractiveRead {
            follow_up: reads.follow_up_of(&locator),
            value: read,
        }
    }

    /// `stubs`, sessions under `root`, read as the TUI's drawing thread
    /// reads them ([`ReadOnlyReason::DrawingThread`]): restored from the
    /// database through the interactive reader when it holds each as it is
    /// now, otherwise without writing any cache. The sessions read outside
    /// the database carry [`InteractiveCache::follow_up`].
    fn read_stubs_interactively(
        &self,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stubs: Vec<SessionStub>,
    ) -> SessionReads {
        let reason =
            match self.probe_database(root, external_titles, &stubs, DatabaseReader::Interactive) {
                Probe::Read(read) => return SessionReads::against_caller(read, external_titles),
                Probe::Busy => ReadOnlyReason::DatabaseBusy,
                Probe::Missed => ReadOnlyReason::DrawingThread,
            };
        self.read_requested_stubs_read_only(root, external_titles, &stubs, reason)
            .followed_up(|| self.interactive_cache().follow_up(reason.fallback()))
    }

    /// `stubs`, sessions under `root`, read through the cache on a thread
    /// that may wait: loads, refreshes and the TUI's background refresh.
    ///
    /// When the session cache database holds every stub as it is now, each
    /// restores from it without refresh ownership. Otherwise the read
    /// acquires ownership, discovers the root again, and reads the stubs
    /// `after_rediscovery` picks from that discovery. Those that the database
    /// does not hold are read from their transcripts and written to the
    /// database and their shard. A [`ReadOnlyReason`] reads without writing
    /// any cache: one found before reading, or a failed write batch that
    /// revokes write permission on the way. The sessions such a read takes
    /// from outside the database wait for the refresh running when it ends,
    /// if any.
    pub(super) fn read_stubs(
        &self,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stubs: Vec<SessionStub>,
        after_rediscovery: StubsAfterRediscovery<'_>,
    ) -> SessionReads {
        let await_refresh = |reason: ReadOnlyReason| {
            self.interactive_cache()
                .refresh_to_await(reason.fallback())
                .map(CacheFollowUp::AwaitRefresh)
        };
        match self.probe_database(root, external_titles, &stubs, DatabaseReader::Waiting) {
            Probe::Read(read) => return SessionReads::against_caller(read, external_titles),
            Probe::Busy => {
                let reason = ReadOnlyReason::DatabaseBusy;
                return self
                    .read_requested_stubs_read_only(root, external_titles, &stubs, reason)
                    .followed_up(|| await_refresh(reason));
            }
            Probe::Missed => {}
        }
        self.meter
            .refreshing(stubs.iter().map(transcript_count).sum());
        let (found, ownership) = match self.own_refresh(root, LockWait::starting_now(self.clock()))
        {
            Ok(OwnedRefresh { ownership, found }) => (found, Ok(ownership)),
            Err(ReadOnly {
                reason,
                rediscovered: Some(found),
            }) => (found, Err(reason)),
            Err(ReadOnly {
                reason,
                rediscovered: None,
            }) => {
                return self
                    .read_requested_stubs_read_only(root, external_titles, &stubs, reason)
                    .followed_up(|| await_refresh(reason));
            }
        };
        let owner = ownership.as_ref().ok().map(RefreshOwnership::token);
        let (picked, failed) = match after_rediscovery {
            StubsAfterRediscovery::Picked(select) => (
                found
                    .stubs
                    .iter()
                    .filter(|stub| select(stub))
                    .cloned()
                    .collect(),
                Vec::new(),
            ),
            StubsAfterRediscovery::SameFiles => self.find_same_files(root, &stubs, &found, owner),
        };
        if picked.is_empty() {
            return SessionReads {
                read: failed,
                rediscovered: Some(found),
                ..SessionReads::against_caller(Vec::new(), external_titles)
            };
        }
        if let Some(owner) = publishing(owner) {
            self.report_cache_update(
                owner.write_batch(|| self.cache.migrate_legacy(&root.path, self.clock())),
            );
        }
        let titles = self.read_under_writer_lock(owner, || self.storage.external_titles(root));
        let access = match &ownership {
            Ok(ownership) => CacheAccess::after_acquisition(ownership.token()),
            Err(reason) => reason.access(),
        };
        let AttemptReads {
            mut read,
            outside_database,
        } = self.read_stubs_attempt(root, &titles, &picked, access);
        read.extend(failed);
        let (kept_write_permission, reason) = match &ownership {
            Ok(ownership) => (
                ownership.token().can_publish(),
                ReadOnlyReason::WritePermissionRevoked,
            ),
            Err(reason) => (false, *reason),
        };
        // Release first: this process's revoked ownership would otherwise
        // read as another process's refresh.
        drop(ownership);
        let reads = SessionReads {
            read,
            rediscovered: Some(found),
            external_titles: titles,
            outside_database,
            follow_up: None,
        };
        if kept_write_permission {
            return reads;
        }
        reads.followed_up(|| await_refresh(reason))
    }

    /// `stubs`, the sessions asked for, read without writing any cache for
    /// `reason`.
    fn read_requested_stubs_read_only(
        &self,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stubs: &[SessionStub],
        reason: ReadOnlyReason,
    ) -> SessionReads {
        let AttemptReads {
            read,
            outside_database,
        } = self.read_stubs_attempt(root, external_titles, stubs, reason.access());
        SessionReads {
            outside_database,
            ..SessionReads::against_caller(read, external_titles)
        }
    }

    /// The TUI's cache for this loader's base, which decides what caches a
    /// session read outside the database.
    fn interactive_cache(&self) -> InteractiveCache {
        InteractiveCache::of(self.cache_base().cloned())
    }

    /// The stubs in `found`, a discovery repeated after acquiring refresh
    /// ownership, for the files `stubs` name. A requested stub that `found`
    /// leaves out is found through
    /// [`SessionStorage::rediscover_unlisted_session`]; it reads as a
    /// transient failure when that lookup fails or `found` could not list its
    /// directory.
    fn find_same_files(
        &self,
        root: &SessionRoot,
        stubs: &[SessionStub],
        found: &DiscoveredSessions,
        owner: Option<&RefreshToken>,
    ) -> (Vec<SessionStub>, Vec<(PathBuf, SessionRead)>) {
        let mut picked: Vec<SessionStub> = found
            .stubs
            .iter()
            .filter(|fresh| {
                stubs
                    .iter()
                    .any(|old| same_file(&old.locator, &fresh.locator))
            })
            .cloned()
            .collect();
        let unlisted: Vec<&SessionStub> = stubs
            .iter()
            .filter(|old| {
                !picked
                    .iter()
                    .any(|fresh| same_file(&fresh.locator, &old.locator))
            })
            .collect();
        let mut failed = Vec::new();
        if unlisted.is_empty() {
            return (picked, failed);
        }
        self.read_under_writer_lock(owner, || {
            for old in unlisted {
                if found
                    .unreadable_directories
                    .iter()
                    .any(|directory| old.locator.starts_with(&directory.path))
                {
                    failed.push((
                        old.locator.clone(),
                        SessionRead::Failed(ReadError::Transient),
                    ));
                    continue;
                }
                match self.rediscover_same_file(root, &old.locator) {
                    Ok(Some(fresh)) => picked.push(fresh),
                    Ok(None) => {}
                    Err(error) => {
                        debug::warn(
                            self.debug_level,
                            &format!(
                                "Failed to find session {} again: {error}",
                                old.locator.display()
                            ),
                        );
                        failed.push((
                            old.locator.clone(),
                            SessionRead::Failed(ReadError::Transient),
                        ));
                    }
                }
            }
        });
        (picked, failed)
    }

    /// `stubs` restored from the database, all of them or none, without
    /// refresh ownership, through `reader`.
    fn probe_database(
        &self,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stubs: &[SessionStub],
        reader: DatabaseReader,
    ) -> Probe {
        if stubs.is_empty() {
            return Probe::Read(Vec::new());
        }
        let probed = (|| {
            let Some(database) = self.open_database_reader(reader)? else {
                return Ok(None);
            };
            stubs
                .iter()
                .map(|stub| {
                    let read =
                        self.read_from_database(database.reader(), root, external_titles, stub)?;
                    Ok(read.map(|read| (stub.locator.clone(), read)))
                })
                .collect::<Result<Option<Vec<_>>>>()
        })();
        match probed {
            Ok(Some(read)) => Probe::Read(read),
            Ok(None) => Probe::Missed,
            Err(error) if sqlite_cache::is_busy_error(&error) => Probe::Busy,
            Err(error) => {
                debug::warn(
                    self.debug_level,
                    &format!("Failed to read the session cache database: {error}"),
                );
                Probe::Missed
            }
        }
    }

    /// `stub`'s session restored from `cache`; `None` when the database does
    /// not hold the session as it is now.
    fn read_from_database(
        &self,
        cache: &SqliteCache,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stub: &SessionStub,
    ) -> Result<Option<SessionRead>> {
        if !cache.holds(self.storage.cache(), &root.path, stub)? {
            return Ok(None);
        }
        let cached = cache.catalogue_session(
            self.storage.cache(),
            self.storage.source(),
            &root.path,
            &stub.cache_key,
            self.show_last,
        )?;
        Ok(cached.map(|cached| match cached {
            CatalogueSession::Listed(mut row) => {
                if let Some(title) = external_titles.get(&row.session_id) {
                    apply_external_title(&mut row, title);
                }
                SessionRead::Listed(Box::new(self.resolve_preview_and_project(*row)))
            }
            CatalogueSession::Empty => SessionRead::Empty,
            CatalogueSession::Unreadable => SessionRead::Failed(ReadError::Permanent),
        }))
    }

    /// `stubs` read through the cache as `access` selects: each from the
    /// database when it holds the session as it is now, the others shard by
    /// shard. Only the shards holding one of them are read, and under
    /// refresh ownership only those holding one that missed the cache are
    /// rewritten, merging their entries into the shard as it is when
    /// written. A session the database does not hold is restored from its
    /// shard entry without the writer lock when no database write needs its
    /// record; otherwise it is parsed from its transcripts, in writer-locked
    /// groups under refresh ownership, and written to the database.
    fn read_stubs_attempt(
        &self,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stubs: &[SessionStub],
        access: CacheAccess<'_>,
    ) -> AttemptReads {
        let mut reads = AttemptReads::default();
        if stubs.is_empty() {
            return reads;
        }
        let mut database = self
            .open_database_for(access)
            .inspect_err(|error| {
                debug::warn(
                    self.debug_level,
                    &format!("Failed to open the session cache database: {error}"),
                )
            })
            .ok()
            .flatten();
        let mut by_shard = BTreeMap::<usize, Vec<SessionStub>>::new();
        for stub in stubs {
            let cached = database
                .as_ref()
                .map(|database| {
                    self.read_from_database(database.reader(), root, external_titles, stub)
                })
                .transpose()
                .inspect_err(|error| {
                    debug::warn(
                        self.debug_level,
                        &format!("Failed to read the session cache catalogue: {error}"),
                    )
                })
                .ok()
                .flatten()
                .flatten();
            match cached {
                Some(read) => reads.read.push((stub.locator.clone(), read)),
                None => by_shard
                    .entry(shard_index(&stub.cache_key))
                    .or_default()
                    .push(stub.clone()),
            }
        }
        // Parse a session a writable database misses instead of restoring it
        // from its shard: the database write needs the record a parse makes.
        let writes_database = matches!(database, Some(CacheDatabase::Writer(_)));
        for (shard, stubs) in by_shard {
            let mut entries =
                CacheEntries::read_from(self.read_under_writer_lock(access.owner(), || {
                    self.cache.read_existing_shard(&root.path, shard)
                }));
            let (restorable, to_parse): (Vec<_>, Vec<_>) = stubs
                .iter()
                .partition(|stub| !writes_database && cached_entry(&entries.read, stub).is_some());
            let restored: Vec<SessionOutcome> = restorable
                .par_iter()
                .map(|stub| {
                    let entry = cached_entry(&entries.read, stub);
                    self.restore_or_parse(root, entry, external_titles, stub, &|| {})
                })
                .collect();
            for (stub, outcome) in restorable.into_iter().zip(restored) {
                reads.outside_database.insert(stub.locator.clone());
                let read = self.record(stub, outcome, &mut entries);
                reads.read.push((stub.locator.clone(), read));
            }
            self.parse_in_groups(
                access,
                &to_parse,
                |stub| {
                    let outcome = self.restore_or_parse(root, None, external_titles, stub, &|| {});
                    let record = writes_database
                        .then(|| cache_record(self.storage, root, stub, &outcome))
                        .flatten();
                    (outcome, record)
                },
                |group, outcomes| {
                    let mut missed = false;
                    for (&stub, (outcome, record)) in group.iter().zip(outcomes) {
                        missed |= cached_entry(&entries.read, stub).is_none();
                        if access.owner().is_some()
                            && let (Some(CacheDatabase::Writer(writer)), Some(record)) =
                                (&mut database, record)
                            && let Err(error) = writer.write(&record)
                        {
                            debug::warn(
                                self.debug_level,
                                &format!("Failed to write the session cache database: {error}"),
                            );
                        }
                        reads.outside_database.insert(stub.locator.clone());
                        reads.read.push((
                            stub.locator.clone(),
                            self.record(stub, outcome, &mut entries),
                        ));
                    }
                    if missed && let Some(owner) = access.owner() {
                        self.report_cache_update(owner.write_batch(|| {
                            self.cache.merge_into_shard(
                                &root.path,
                                shard,
                                std::mem::take(&mut entries.current),
                                self.clock(),
                            )
                        }));
                    }
                },
            );
        }
        reads
    }
}

/// A file's place in the first root's discovery that settles it.
enum Listing {
    /// Listed by the root at this index, as this stub.
    Listed(usize, SessionStub),
    /// Beneath a directory that root's discovery could not list.
    UnreadableDirectory,
}

/// `locator` in `discovered`; `None` when every root's discovery leaves it
/// out.
fn find_listed(
    discovered: &[(SessionRoot, DiscoveredSessions)],
    locator: &Path,
) -> Option<Listing> {
    discovered
        .iter()
        .enumerate()
        .find_map(|(root_index, (_, found))| {
            if found
                .unreadable_directories
                .iter()
                .any(|directory| locator.starts_with(&directory.path))
            {
                return Some(Listing::UnreadableDirectory);
            }
            found
                .stubs
                .iter()
                .find(|stub| same_file(&stub.locator, locator))
                .map(|stub| Listing::Listed(root_index, stub.clone()))
        })
}
