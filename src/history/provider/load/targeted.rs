//! Targeted reads: the sessions at given files, or the stubs a refresh
//! picks, read through the cache.

use super::cache_access::DatabaseReader;
use super::*;

/// The two ways the TUI reads sessions at given files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TargetedRead {
    /// On the drawing thread: does not acquire refresh ownership or write any
    /// cache. The next step of a session read outside the database is its
    /// [`NextStep::after_drawing_thread_read`].
    Interactive,
    /// On the TUI's background refresh thread: acquires ownership as a load
    /// does, and caches each session the database does not hold.
    Caching,
}

/// The outcomes of [`SessionLoader::read_stubs`], one per session read, with
/// the discovery and titles it read the sessions against.
pub(super) struct SessionReads {
    pub(super) outcomes: Vec<SessionOutcome>,
    /// The root's discovery repeated after acquiring refresh ownership;
    /// `None` when the reads used the caller's discovery.
    pub(super) rediscovered: Option<DiscoveredSessions>,
    pub(super) external_titles: HashMap<String, SessionTitle>,
}

impl SessionReads {
    /// `outcomes`, read against the caller's discovery and titles.
    fn against_caller(
        outcomes: Vec<SessionOutcome>,
        external_titles: &HashMap<String, SessionTitle>,
    ) -> Self {
        Self {
            outcomes,
            rediscovered: None,
            external_titles: external_titles.clone(),
        }
    }
}

/// The outcome of restoring every requested session from the database.
enum Probe {
    /// One settled outcome per requested session, in order.
    Read(Vec<SessionOutcome>),
    /// The database does not hold one of the sessions as it is now.
    Missed,
    /// The database stayed busy past the read's timeout.
    Busy,
}

/// A session [`SessionLoader::read_stubs_attempt`] read, before the next
/// step of a session read outside the database is decided.
struct AttemptRead {
    /// Settled.
    outcome: SessionOutcome,
    /// True when the session was read from its transcripts and not written
    /// to the database.
    is_outside_database: bool,
}

/// `reads` as outcomes. The next step of each session read outside the
/// database is `next_outside`'s, which runs once, and only when there is
/// one.
fn with_next_steps(
    reads: Vec<AttemptRead>,
    next_outside: impl FnOnce() -> NextStep,
) -> Vec<SessionOutcome> {
    let next = reads
        .iter()
        .any(|read| read.is_outside_database)
        .then(next_outside);
    reads
        .into_iter()
        .map(|read| match next {
            Some(next) if read.is_outside_database => read.outcome.with_next_step(next),
            _ => read.outcome,
        })
        .collect()
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
    /// The sessions at `locators`, one outcome each in order, each answering
    /// for its locator, read through the cache as `targeted_read` selects.
    /// [`SessionRead::Missing`] for a file neither discovery nor
    /// [`SessionStorage::rediscover_unlisted_session`] finds.
    pub(super) fn reread(
        &self,
        locators: &[&Path],
        targeted_read: TargetedRead,
    ) -> Vec<SessionOutcome> {
        let Ok(discovered) = self.discover_every_root() else {
            return locators
                .iter()
                .map(|locator| SessionOutcome::failed_transiently(locator))
                .collect();
        };
        let mut outcomes: Vec<Option<SessionOutcome>> = locators.iter().map(|_| None).collect();
        let mut picked: Vec<Vec<(usize, SessionStub)>> =
            discovered.iter().map(|_| Vec::new()).collect();
        let mut unlisted = Vec::new();
        for (index, locator) in locators.iter().enumerate() {
            match find_listed(&discovered, locator) {
                Some(Listing::Listed(root_index, stub)) => picked[root_index].push((index, stub)),
                Some(Listing::UnreadableDirectory) => {
                    outcomes[index] = Some(SessionOutcome::failed_transiently(locator));
                }
                None => unlisted.push(index),
            }
        }
        for index in unlisted {
            let locator = locators[index];
            match self.find_unlisted(&discovered, locator) {
                Ok(Some((root_index, stub))) => picked[root_index].push((index, stub)),
                Ok(None) => outcomes[index] = Some(SessionOutcome::missing(locator)),
                Err(error) => {
                    debug::warn(
                        self.debug_level,
                        &format!(
                            "Failed to find session {} again: {error}",
                            locator.display()
                        ),
                    );
                    outcomes[index] = Some(SessionOutcome::failed_transiently(locator));
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
            let mut by_locator: HashMap<PathBuf, SessionOutcome> = reads_under_root
                .outcomes
                .into_iter()
                .map(|outcome| (outcome.locator().to_path_buf(), outcome))
                .collect();
            for (index, stub) in picked {
                outcomes[index] = by_locator
                    .remove(&stub.locator)
                    .map(|outcome| outcome.answering(locators[index]));
            }
        }
        outcomes
            .into_iter()
            .zip(locators)
            .map(|(outcome, locator)| outcome.unwrap_or_else(|| SessionOutcome::missing(locator)))
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
    /// for [`read_resolved_session`].
    pub(super) fn read_one(&self, root: &SessionRoot, stub: SessionStub) -> SessionOutcome {
        let external_titles = self.storage.external_titles(root);
        let locator = stub.locator.clone();
        self.read_stubs_interactively(root, &external_titles, vec![stub])
            .outcomes
            .pop()
            .unwrap_or_else(|| SessionOutcome::failed_transiently(&locator))
    }

    /// `stubs`, sessions under `root`, read as the TUI's drawing thread
    /// reads them ([`ReadOnlyReason::DrawingThread`]): restored from the
    /// database through the interactive reader when it holds each as it is
    /// now, otherwise without writing any cache. The next step of a session
    /// read outside the database is its [`NextStep::after_drawing_thread_read`].
    fn read_stubs_interactively(
        &self,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stubs: Vec<SessionStub>,
    ) -> SessionReads {
        let reason =
            match self.probe_database(root, external_titles, &stubs, DatabaseReader::Interactive) {
                Probe::Read(outcomes) => {
                    return SessionReads::against_caller(outcomes, external_titles);
                }
                Probe::Busy => ReadOnlyReason::DatabaseBusy,
                Probe::Missed => ReadOnlyReason::DrawingThread,
            };
        let reads = self.read_stubs_attempt(root, external_titles, &stubs, reason.access());
        let outcomes = with_next_steps(reads, || {
            NextStep::after_drawing_thread_read(self.cache_base(), reason.fallback())
        });
        SessionReads::against_caller(outcomes, external_titles)
    }

    /// `stubs`, sessions under `root`, read through the cache on a thread
    /// that may wait: loads, refreshes and the TUI's background refresh.
    ///
    /// When the session cache database holds every stub as it is now, each
    /// restores from it without refresh ownership. Otherwise the read
    /// acquires ownership, discovers the root again, and reads the stubs
    /// `after_rediscovery` picks from that discovery. Those that the database
    /// does not hold are read from their transcripts and written to the
    /// database. A [`ReadOnlyReason`] reads without writing any cache: one
    /// found before reading, or a failed write batch that revokes write
    /// permission on the way. The next step of a session read outside the
    /// database is its [`NextStep::after_read_without_writing`] as the read
    /// ends.
    pub(super) fn read_stubs(
        &self,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stubs: Vec<SessionStub>,
        after_rediscovery: StubsAfterRediscovery<'_>,
    ) -> SessionReads {
        match self.probe_database(root, external_titles, &stubs, DatabaseReader::Waiting) {
            Probe::Read(outcomes) => {
                return SessionReads::against_caller(outcomes, external_titles);
            }
            Probe::Busy => {
                return self.read_requested_stubs_read_only(
                    root,
                    external_titles,
                    &stubs,
                    ReadOnlyReason::DatabaseBusy,
                );
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
                return self.read_requested_stubs_read_only(root, external_titles, &stubs, reason);
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
                outcomes: failed,
                rediscovered: Some(found),
                external_titles: external_titles.clone(),
            };
        }
        let titles = self.read_under_writer_lock(owner, || self.storage.external_titles(root));
        let access = match &ownership {
            Ok(ownership) => CacheAccess::after_acquisition(ownership.token()),
            Err(reason) => reason.access(),
        };
        let reads = self.read_stubs_attempt(root, &titles, &picked, access);
        let fallback = match &ownership {
            Ok(_) => Fallback::NotServed,
            Err(reason) => reason.fallback(),
        };
        // Release first: this process's revoked ownership would otherwise
        // read as another process's refresh.
        drop(ownership);
        let mut outcomes = with_next_steps(reads, || {
            NextStep::after_read_without_writing(self.cache_base(), fallback)
        });
        outcomes.extend(failed);
        SessionReads {
            outcomes,
            rediscovered: Some(found),
            external_titles: titles,
        }
    }

    /// `stubs`, the sessions asked for, read without writing any cache for
    /// `reason`. The next step of a session read outside the database is its
    /// [`NextStep::after_read_without_writing`].
    fn read_requested_stubs_read_only(
        &self,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stubs: &[SessionStub],
        reason: ReadOnlyReason,
    ) -> SessionReads {
        let reads = self.read_stubs_attempt(root, external_titles, stubs, reason.access());
        let outcomes = with_next_steps(reads, || {
            NextStep::after_read_without_writing(self.cache_base(), reason.fallback())
        });
        SessionReads::against_caller(outcomes, external_titles)
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
    ) -> (Vec<SessionStub>, Vec<SessionOutcome>) {
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
                    failed.push(SessionOutcome::failed_transiently(&old.locator));
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
                        failed.push(SessionOutcome::failed_transiently(&old.locator));
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
                    Ok(read.map(|read| SessionOutcome::settled(stub, read)))
                })
                .collect::<Result<Option<Vec<_>>>>()
        })();
        match probed {
            Ok(Some(outcomes)) => Probe::Read(outcomes),
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
            CatalogueSession::Listed(row) => SessionRead::Listed(Box::new(
                self.resolve_preview_and_project(restored_row(&row, stub, external_titles)),
            )),
            CatalogueSession::Empty => SessionRead::Empty,
            CatalogueSession::Unreadable => SessionRead::Failed(ReadError::Permanent),
        }))
    }

    /// `stubs` read through the cache as `access` selects: each from the
    /// database when it holds the session as it is now, the others parsed
    /// from their transcripts, in writer-locked groups under refresh
    /// ownership, and written to the database. A parsed session the
    /// database did not take is read outside it.
    fn read_stubs_attempt(
        &self,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stubs: &[SessionStub],
        access: CacheAccess<'_>,
    ) -> Vec<AttemptRead> {
        let mut reads = Vec::with_capacity(stubs.len());
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
        let mut to_parse = Vec::new();
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
                Some(read) => reads.push(AttemptRead {
                    outcome: SessionOutcome::settled(stub, read),
                    is_outside_database: false,
                }),
                None => to_parse.push(stub),
            }
        }
        let writes_database = matches!(database, Some(CacheDatabase::Writer(_)));
        self.parse_in_groups(
            access,
            &to_parse,
            |stub| {
                let raw = self.parse(root, stub, &|| {});
                let record = writes_database
                    .then(|| cache_record(self.storage, root, stub, &raw))
                    .flatten();
                (raw, record)
            },
            |group, parsed| {
                for (&stub, (raw, record)) in group.iter().zip(parsed) {
                    let is_written = access.owner().is_some()
                        && match (&mut database, record) {
                            (Some(CacheDatabase::Writer(writer)), Some(record)) => writer
                                .write(&record)
                                .inspect_err(|error| {
                                    debug::warn(
                                        self.debug_level,
                                        &format!(
                                            "Failed to write the session cache database: {error}"
                                        ),
                                    );
                                })
                                .is_ok(),
                            _ => false,
                        };
                    reads.push(AttemptRead {
                        outcome: SessionOutcome::settled(stub, self.session_read(stub, raw)),
                        is_outside_database: !is_written,
                    });
                }
            },
        );
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
