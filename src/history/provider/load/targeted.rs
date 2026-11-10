//! Targeted reads: the sessions at given files, or the stubs a refresh
//! picks, read through the cache.

use super::cache_access::DatabaseReader;
use super::*;

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

/// A session one read returned, before the next step of a session the
/// database does not hold as read is decided.
struct AttemptRead {
    /// Settled.
    outcome: SessionOutcome,
    /// True when the database holds the session as the read returned it, as
    /// its transcripts are now. False for a session parsed and not written,
    /// an older copy, and a session missing from the database.
    is_current: bool,
}

/// `reads` as outcomes. Each read that is not current takes the step
/// `next_step` returns. `next_step` runs once, and only when such a read
/// exists.
fn with_next_steps(
    reads: Vec<AttemptRead>,
    next_step: impl FnOnce() -> NextStep,
) -> Vec<SessionOutcome> {
    let next = reads.iter().any(|read| !read.is_current).then(next_step);
    reads
        .into_iter()
        .map(|read| match next {
            Some(next) if !read.is_current => read.outcome.with_next_step(next),
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
    /// for its locator; `read_under_root` reads each root's picked stubs
    /// against the root's external titles. [`SessionRead::Missing`] for a
    /// file neither discovery nor
    /// [`SessionStorage::rediscover_unlisted_session`] finds. A file under a
    /// directory discovery could not list reads as its copy, through
    /// `reader`.
    pub(super) fn reread<E>(
        &self,
        locators: &[&Path],
        reader: DatabaseReader,
        read_under_root: impl Fn(
            &SessionRoot,
            &HashMap<String, SessionTitle>,
            Vec<SessionStub>,
        ) -> std::result::Result<SessionReads, E>,
    ) -> std::result::Result<Vec<SessionOutcome>, E> {
        let Ok(discovered) = self.discover_every_root() else {
            return Ok(locators
                .iter()
                .map(|locator| SessionOutcome::failed_transiently(locator))
                .collect());
        };
        let mut outcomes: Vec<Option<SessionOutcome>> = locators.iter().map(|_| None).collect();
        let mut picked: Vec<Vec<(usize, SessionStub)>> =
            discovered.iter().map(|_| Vec::new()).collect();
        let mut under_unlisted_directories: Vec<Vec<usize>> =
            discovered.iter().map(|_| Vec::new()).collect();
        let mut unlisted = Vec::new();
        for (index, locator) in locators.iter().enumerate() {
            match find_listed(&discovered, locator) {
                Some(Listing::Listed(root_index, stub)) => picked[root_index].push((index, stub)),
                Some(Listing::UnreadableDirectory(root_index)) => {
                    under_unlisted_directories[root_index].push(index);
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
        for (((root, _), picked), under_unlisted_directories) in discovered
            .iter()
            .zip(picked)
            .zip(under_unlisted_directories)
        {
            if picked.is_empty() && under_unlisted_directories.is_empty() {
                continue;
            }
            let external_titles = self.storage.external_titles(root);
            for index in under_unlisted_directories {
                let locator = locators[index];
                let copy =
                    self.copy_under_unlisted_directory(reader, root, &external_titles, locator);
                outcomes[index] = Some(copy.answering(locator));
            }
            if picked.is_empty() {
                continue;
            }
            let stubs = picked.iter().map(|(_, stub)| stub.clone()).collect();
            let reads_under_root = read_under_root(root, &external_titles, stubs)?;
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
        Ok(outcomes
            .into_iter()
            .zip(locators)
            .map(|(outcome, locator)| outcome.unwrap_or_else(|| SessionOutcome::missing(locator)))
            .collect())
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

    /// The copy the database opened through `reader` holds of the session
    /// at `locator`, under a directory `root`'s discovery could not list,
    /// settled while it matches the transcripts as they are now. Without a
    /// current copy, or a database that can be read, a transient failure:
    /// the session may still be there, and a settled older copy would have
    /// the viewer request another refresh on every reload.
    fn copy_under_unlisted_directory(
        &self,
        reader: DatabaseReader,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        locator: &Path,
    ) -> SessionOutcome {
        let found = self.open_database_reader(reader).and_then(|database| {
            let Some(database) = database else {
                return Ok(None);
            };
            let cache_key =
                database
                    .reader()
                    .current_cache_key(self.storage.source(), &root.path, locator)?;
            Ok(cache_key.map(|cache_key| (database, cache_key)))
        });
        let (database, cache_key) = match found {
            Ok(Some(found)) => found,
            Ok(None) => return SessionOutcome::failed_transiently(locator),
            Err(error) => {
                debug::warn(
                    self.debug_level,
                    &format!("Failed to read the session cache catalogue: {error}"),
                );
                return SessionOutcome::failed_transiently(locator);
            }
        };
        let stub = SessionStub {
            locator: locator.to_path_buf(),
            subagents: Vec::new(),
            cache_key,
            fingerprint: Fingerprint::UNKNOWN,
        };
        self.cached_copy(Some(&database), root, external_titles, &stub)
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
    /// reads them: from the session cache alone, through the interactive
    /// reader. A copy the database holds as the transcripts are now is
    /// settled. An older copy, or [`SessionRead::NotCached`] for a session
    /// missing from the database, takes its
    /// [`NextStep::after_read_without_writing`], or its
    /// [`NextStep::after_failed_interactive_read`] when the database could
    /// not be opened.
    pub(super) fn read_stubs_interactively(
        &self,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stubs: Vec<SessionStub>,
    ) -> SessionReads {
        let (database, open_error) = match self.open_database_reader(DatabaseReader::Interactive) {
            Ok(database) => (database, None),
            Err(error) => {
                debug::warn(
                    self.debug_level,
                    &format!("Failed to open the session cache database: {error}"),
                );
                (None, Some(error))
            }
        };
        let reads = stubs
            .iter()
            .map(|stub| self.read_copy(database.as_ref(), root, external_titles, stub))
            .collect();
        let outcomes = with_next_steps(reads, || match &open_error {
            Some(error) => NextStep::after_failed_interactive_read(self.cache_base(), error),
            None => NextStep::after_read_without_writing(self.cache_base()),
        });
        SessionReads::against_caller(outcomes, external_titles)
    }

    /// `stub`'s session as `database` holds it: current with its
    /// transcripts, or the copy as it is.
    fn read_copy(
        &self,
        database: Option<&CacheDatabase>,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stub: &SessionStub,
    ) -> AttemptRead {
        let current = database.and_then(|database| {
            self.read_from_database(database.reader(), root, external_titles, stub)
                .inspect_err(|error| {
                    debug::warn(
                        self.debug_level,
                        &format!("Failed to read the session cache catalogue: {error}"),
                    )
                })
                .ok()
                .flatten()
        });
        match current {
            Some(read) => AttemptRead {
                outcome: SessionOutcome::settled(stub, read),
                is_current: true,
            },
            None => AttemptRead {
                outcome: self.cached_copy(database, root, external_titles, stub),
                is_current: false,
            },
        }
    }

    /// `stubs`, sessions under `root`, read through the cache on a thread
    /// that may wait: refreshes and the TUI's background refresh.
    ///
    /// When the session cache database holds every stub as it is now, each
    /// restores from it without refresh ownership. Otherwise the read takes
    /// `operation`'s refresh ownership, discovers the root again, and reads
    /// the stubs `after_rediscovery` picks from that discovery; without
    /// ownership it reads the cache as it is. Those that the database does
    /// not hold are read from their transcripts and written to the database;
    /// once the owner is no longer live, the rest read as their cached
    /// copies, without parsing. Each session the database does not hold as
    /// read gets its [`NextStep::after_read_without_writing`] as the read
    /// ends. Fails on a database error other than a busy lock.
    pub(super) fn read_stubs(
        &self,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stubs: Vec<SessionStub>,
        after_rediscovery: StubsAfterRediscovery<'_>,
        operation: &OperationScope,
    ) -> Result<SessionReads> {
        if let Probe::Read(outcomes) = self.probe_database(
            root,
            external_titles,
            &stubs,
            DatabaseReader::WithoutWaiting,
        ) {
            return Ok(SessionReads::against_caller(outcomes, external_titles));
        }
        self.meter
            .refreshing(stubs.iter().map(transcript_count).sum());
        let owner = match operation.acquire_access()? {
            OperationAccess::Owner(owner) => owner,
            OperationAccess::AsIs => {
                let reads =
                    self.read_stubs_attempt(root, external_titles, &stubs, CacheAccess::AsIs);
                let outcomes = with_next_steps(reads, || {
                    NextStep::after_read_without_writing(self.cache_base())
                });
                return Ok(SessionReads::against_caller(outcomes, external_titles));
            }
        };
        let Some(found) = self.rediscover_owned(root, owner.token()) else {
            let reads = self.read_stubs_attempt(
                root,
                external_titles,
                &stubs,
                CacheAccess::Refresh(&owner),
            );
            let outcomes = with_next_steps(reads, || {
                NextStep::after_read_without_writing(self.cache_base())
            });
            return Ok(SessionReads::against_caller(outcomes, external_titles));
        };
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
            StubsAfterRediscovery::SameFiles => {
                self.find_same_files(root, &stubs, &found, Some(owner.token()))
            }
        };
        if picked.is_empty() {
            return Ok(SessionReads {
                outcomes: failed,
                rediscovered: Some(found),
                external_titles: external_titles.clone(),
            });
        }
        let titles =
            self.read_under_writer_lock(Some(owner.token()), || self.storage.external_titles(root));
        let reads = self.read_stubs_attempt(root, &titles, &picked, CacheAccess::Refresh(&owner));
        let mut outcomes = with_next_steps(reads, || {
            NextStep::after_read_without_writing(self.cache_base())
        });
        outcomes.extend(failed);
        Ok(SessionReads {
            outcomes,
            rediscovered: Some(found),
            external_titles: titles,
        })
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
        Ok(cached.map(|cached| {
            self.session_read(stub, restore_from_catalogue(&cached, stub, external_titles))
        }))
    }

    /// The copy of `stub`'s session `database` holds, as it is, whether or
    /// not the transcript changed since; [`SessionRead::NotCached`] when the
    /// session is missing from it, since nothing may parse the transcript.
    fn cached_copy(
        &self,
        database: Option<&CacheDatabase>,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        stub: &SessionStub,
    ) -> SessionOutcome {
        let copy = database.and_then(|database| {
            database
                .reader()
                .catalogue_session(
                    self.storage.cache(),
                    self.storage.source(),
                    &root.path,
                    &stub.cache_key,
                    self.show_last,
                )
                .inspect_err(|error| {
                    debug::warn(
                        self.debug_level,
                        &format!("Failed to read the session cache catalogue: {error}"),
                    )
                })
                .ok()
                .flatten()
        });
        match copy {
            Some(copy) => SessionOutcome::settled(
                stub,
                self.session_read(stub, restore_from_catalogue(&copy, stub, external_titles)),
            ),
            None => SessionOutcome::not_cached(stub),
        }
    }

    /// `stubs` read through the cache as `access` selects: each from the
    /// database when it holds the session as it is now, the others parsed
    /// from their transcripts, in writer-locked groups while the refresh
    /// owner is live, and written to the database. A parsed session the
    /// database did not take is read outside it. A read of the cache as it
    /// is, and every session left once the owner is no longer live, parses
    /// nothing: see [`Self::cached_copy`].
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
                    is_current: true,
                }),
                None => to_parse.push(stub),
            }
        }
        if let CacheAccess::AsIs = access {
            reads.extend(to_parse.into_iter().map(|stub| AttemptRead {
                outcome: self.cached_copy(database.as_ref(), root, external_titles, stub),
                is_current: false,
            }));
            return reads;
        }
        let writes_database = matches!(database, Some(CacheDatabase::Writer(_)));
        let unparsed = self.parse_in_groups(
            access,
            &to_parse,
            |permit, stub| {
                let raw = self.parse(permit, root, stub, &|| {});
                let record = writes_database
                    .then(|| cache_record(permit, self.storage, root, stub, &raw))
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
                        is_current: is_written,
                    });
                }
            },
        );
        reads.extend(unparsed.iter().map(|stub| AttemptRead {
            outcome: self.cached_copy(database.as_ref(), root, external_titles, stub),
            is_current: false,
        }));
        reads
    }
}

/// A file's place in the first root's discovery that settles it.
enum Listing {
    /// Listed by the root at this index, as this stub.
    Listed(usize, SessionStub),
    /// Beneath a directory the discovery of the root at this index could not
    /// list.
    UnreadableDirectory(usize),
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
                return Some(Listing::UnreadableDirectory(root_index));
            }
            found
                .stubs
                .iter()
                .find(|stub| same_file(&stub.locator, locator))
                .map(|stub| Listing::Listed(root_index, stub.clone()))
        })
}
