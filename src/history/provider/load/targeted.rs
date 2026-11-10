//! Targeted reads: the sessions at given files, or the stubs a refresh
//! picks, read through the cache.

use super::cache_access::DatabaseReader;
use super::*;
use crate::history::provider::walk::Canonicaliser;

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
    /// The sessions at `named`, files below `roots`, one outcome each in
    /// order, each answering for its file as named; `read_under_root` reads
    /// each root's picked stubs against the root's external titles.
    /// [`SessionRead::Missing`] for a file neither discovery nor
    /// [`SessionStorage::rediscover_unlisted_session`] finds. A file under a
    /// directory discovery could not list reads as its copy, through
    /// `reader`.
    pub(super) fn reread<E>(
        &self,
        roots: Vec<SessionRoot>,
        named: &[NamedFile<'_>],
        reader: DatabaseReader,
        read_under_root: impl Fn(
            &SessionRoot,
            &HashMap<String, SessionTitle>,
            Vec<SessionStub>,
        ) -> std::result::Result<SessionReads, E>,
    ) -> std::result::Result<Vec<SessionOutcome>, E> {
        let locators: Vec<&Path> = named.iter().map(|file| file.path).collect();
        let Ok(discovered) = self.discover_roots(roots) else {
            return Ok(all_failed_transiently(&locators));
        };
        let mut outcomes: Vec<Option<SessionOutcome>> = locators.iter().map(|_| None).collect();
        let mut picked: Vec<Vec<(usize, SessionStub)>> =
            discovered.iter().map(|_| Vec::new()).collect();
        let mut under_unlisted_directories: Vec<Vec<(usize, BelowKey)>> =
            discovered.iter().map(|_| Vec::new()).collect();
        let mut unlisted = Vec::new();
        for (index, locator) in named.iter().enumerate() {
            match find_listed(&discovered, locator) {
                Some(Listing::Listed(root_index, stub)) => picked[root_index].push((index, stub)),
                Some(Listing::UnreadableDirectory(root_index, key)) => {
                    under_unlisted_directories[root_index].push((index, key));
                }
                None => unlisted.push(index),
            }
        }
        for index in unlisted {
            let locator = locators[index];
            match self.find_unlisted(&discovered, &named[index]) {
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
            for (index, key) in under_unlisted_directories {
                let locator = locators[index];
                let copy = self.copy_under_unlisted_directory(
                    reader,
                    root,
                    &external_titles,
                    locator,
                    &key,
                );
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
        locator: &NamedFile,
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

    /// The session at `locator`, below a directory `root`'s discovery could
    /// not list, read from its copy in the database `reader` opens and
    /// settled, published at the copy's generation, while that copy matches
    /// the transcripts as they are now.
    /// `key` is the file's [`key_below`] `root`. Without a current copy or a
    /// readable database, a transient failure: the session may still be
    /// there, and a settled older copy would have the viewer request another
    /// refresh on every reload.
    fn copy_under_unlisted_directory(
        &self,
        reader: DatabaseReader,
        root: &SessionRoot,
        external_titles: &HashMap<String, SessionTitle>,
        locator: &Path,
        key: &BelowKey,
    ) -> SessionOutcome {
        if !key.is_below_root_itself() {
            return SessionOutcome::failed_transiently(locator);
        }
        let found = self.open_database_reader(reader).and_then(|database| {
            let Some(database) = database else {
                return Ok(None);
            };
            let cache_key =
                database
                    .reader()
                    .current_cache_key(self.storage.source(), root, &key.key)?;
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
        let place = self.record_place(root);
        match self.restored_copy(Some(&database), root, &place, external_titles, &stub) {
            Some(read) => SessionOutcome::settled(&stub, read),
            None => SessionOutcome::not_cached(&stub),
        }
    }

    /// The stub of the session at `locator` under `root`, found through
    /// [`SessionStorage::rediscover_unlisted_session`]; `None` when the
    /// provider finds no session there, or names another file.
    fn rediscover_same_file(
        &self,
        root: &SessionRoot,
        locator: &NamedFile,
    ) -> Result<Option<SessionStub>> {
        Ok(self
            .storage
            .rediscover_unlisted_session(root, locator.path, self.environment.lister())?
            .filter(|stub| locator.is(root, &stub.locator)))
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
        let place = self.record_place(root);
        let reads = stubs
            .iter()
            .map(|stub| self.read_copy(database.as_ref(), root, &place, external_titles, stub))
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
        place: &RecordPlace<'_>,
        external_titles: &HashMap<String, SessionTitle>,
        stub: &SessionStub,
    ) -> AttemptRead {
        let current = database.and_then(|database| {
            self.read_from_database(database.reader(), root, place, external_titles, stub)
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
                outcome: self.cached_copy(database, root, place, external_titles, stub),
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
        // Both discoveries spell their files below `root` as it is spelled.
        let requested: HashSet<BelowKey> = stubs
            .iter()
            .filter_map(|old| key_below(root, &old.locator))
            .collect();
        let mut picked: Vec<SessionStub> = found
            .stubs
            .iter()
            .filter(|fresh| {
                key_below(root, &fresh.locator).is_some_and(|key| requested.contains(&key))
            })
            .cloned()
            .collect();
        let found_again: HashSet<BelowKey> = picked
            .iter()
            .filter_map(|fresh| key_below(root, &fresh.locator))
            .collect();
        let unlisted: Vec<&SessionStub> = stubs
            .iter()
            .filter(|old| {
                key_below(root, &old.locator).is_none_or(|key| !found_again.contains(&key))
            })
            .collect();
        let mut failed = Vec::new();
        if unlisted.is_empty() {
            return (picked, failed);
        }
        self.read_under_writer_lock(owner, || {
            for old in unlisted {
                if key_below(root, &old.locator)
                    .is_some_and(|key| is_beneath_unreadable_directory(root, found, &key))
                {
                    failed.push(SessionOutcome::failed_transiently(&old.locator));
                    continue;
                }
                let named = NamedFile::new(&old.locator, self.environment.canonicaliser());
                match self.rediscover_same_file(root, &named) {
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
        let place = self.record_place(root);
        let probed = (|| {
            let Some(database) = self.open_database_reader(reader)? else {
                return Ok(None);
            };
            stubs
                .iter()
                .map(|stub| {
                    let read = self.read_from_database(
                        database.reader(),
                        root,
                        &place,
                        external_titles,
                        stub,
                    )?;
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
        place: &RecordPlace<'_>,
        external_titles: &HashMap<String, SessionTitle>,
        stub: &SessionStub,
    ) -> Result<Option<SessionRead>> {
        if !cache.holds(self.storage.cache(), root, stub)? {
            return Ok(None);
        }
        let cached = cache.catalogue_session(
            self.storage.cache(),
            self.storage.source(),
            root,
            &stub.cache_key,
            self.show_last,
        )?;
        Ok(cached.map(|cached| {
            self.session_read(
                place,
                stub,
                restore_from_catalogue(&cached, external_titles),
            )
        }))
    }

    /// The copy of `stub`'s session `database` holds, as it is, whether or
    /// not the transcript changed since, its row unpublished;
    /// [`SessionRead::NotCached`] when the session is missing from it, since
    /// nothing may parse the transcript.
    fn cached_copy(
        &self,
        database: Option<&CacheDatabase>,
        root: &SessionRoot,
        place: &RecordPlace<'_>,
        external_titles: &HashMap<String, SessionTitle>,
        stub: &SessionStub,
    ) -> SessionOutcome {
        match self.restored_copy(database, root, place, external_titles, stub) {
            Some(read) => SessionOutcome::settled(stub, read_as_unpublished(read, stub)),
            None => SessionOutcome::not_cached(stub),
        }
    }

    /// The read of the copy of `stub`'s session `database` holds, restored
    /// from its catalogue row as it is; `None` when the session is missing
    /// from it or its catalogue cannot be read.
    fn restored_copy(
        &self,
        database: Option<&CacheDatabase>,
        root: &SessionRoot,
        place: &RecordPlace<'_>,
        external_titles: &HashMap<String, SessionTitle>,
        stub: &SessionStub,
    ) -> Option<SessionRead> {
        let copy = database.and_then(|database| {
            database
                .reader()
                .catalogue_session(
                    self.storage.cache(),
                    self.storage.source(),
                    root,
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
        copy.map(|copy| {
            self.session_read(place, stub, restore_from_catalogue(&copy, external_titles))
        })
    }

    /// `stubs` read through the cache as `access` selects: each from the
    /// database when it holds the session as it is now, the others parsed
    /// from their transcripts, in writer-locked groups while the refresh
    /// owner is live, and written to the database. A whole parse whose copy
    /// does not hold its entries keeps them on its outcome, as does the
    /// parse of a session without a row; a resumed read whose write fails
    /// reads as the stored copy. A read of the cache as it
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
        let place = self.record_place(root);
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
                    self.read_from_database(database.reader(), root, &place, external_titles, stub)
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
                outcome: self.cached_copy(database.as_ref(), root, &place, external_titles, stub),
                is_current: false,
            }));
            return reads;
        }
        let writes_database = matches!(database, Some(CacheDatabase::Writer(_)));
        let to_parse: Vec<(&SessionStub, Option<Conversation>)> = to_parse
            .into_iter()
            .map(|stub| {
                let stored = match &database {
                    Some(database @ CacheDatabase::Writer(_)) => {
                        self.stored_listed_row(database, root, stub)
                    }
                    _ => None,
                };
                (stub, stored)
            })
            .collect();
        let unparsed = self.parse_in_groups(
            access,
            &to_parse,
            |permit, (stub, stored)| {
                let resumed = stored
                    .as_ref()
                    .and_then(|stored| self.read_resumed(permit, root, stub, stored, &|| {}));
                if let Some(resumed) = resumed {
                    return SessionParse::Resumed(Box::new(resumed));
                }
                let stamps = writes_database.then(|| StubStamps::current(stub));
                let (raw, transcripts) = self.parse(permit, root, stub, &|| {});
                let record = match stamps {
                    Some(stamps) => {
                        cache_record(&place, stub, stamps, &raw, transcripts, &|error| {
                            self.report_unbuilt_record(stub, error)
                        })
                    }
                    None => ParseRecord::without_database(&raw, transcripts),
                };
                SessionParse::Whole(Box::new(raw), record)
            },
            |group, parsed| {
                for (&(stub, _), parsed) in group.iter().zip(parsed) {
                    let write = |database: &mut Option<CacheDatabase>, record: &SessionRecord| {
                        match database {
                            Some(CacheDatabase::Writer(writer)) if access.owner().is_some() => {
                                writer
                                    .write(record)
                                    .inspect_err(|error| {
                                        debug::warn(
                                            self.debug_level,
                                            &format!(
                                                "Failed to write the session cache database: \
                                                 {error}"
                                            ),
                                        );
                                    })
                                    .ok()
                            }
                            _ => None,
                        }
                    };
                    reads.push(match parsed {
                        SessionParse::Whole(raw, record) => {
                            let (generation, unwritten_entries) = match record {
                                ParseRecord::Record(record) => {
                                    let generation = write(&mut database, &record);
                                    let is_written =
                                        generation.is_some() && record.writes_its_entries();
                                    let unwritten =
                                        (!is_written).then(|| record.into_parsed_entries());
                                    (generation, unwritten.flatten())
                                }
                                ParseRecord::NotRecorded => (None, None),
                                ParseRecord::Unbuilt(entries) => (None, Some(entries)),
                            };
                            let mut raw = match (*raw, generation) {
                                (RawRead::Parsed(row), Some(generation)) => {
                                    RawRead::Parsed(published_at(row, generation))
                                }
                                (raw, _) => raw,
                            };
                            let unwritten = match unwritten_entries {
                                Some(entries) => UnwrittenParse::Entries(entries),
                                None => raw.take_unlisted_parse(),
                            };
                            AttemptRead {
                                outcome: SessionOutcome::settled_with_unwritten(
                                    stub,
                                    self.session_read(&place, stub, raw),
                                    unwritten,
                                ),
                                is_current: generation.is_some(),
                            }
                        }
                        SessionParse::Resumed(resumed) => {
                            match write(&mut database, &resumed.record) {
                                Some(generation) => {
                                    let row = published_at(resumed.row, generation);
                                    AttemptRead {
                                        outcome: SessionOutcome::settled(
                                            stub,
                                            self.session_read(&place, stub, RawRead::Parsed(row)),
                                        ),
                                        is_current: true,
                                    }
                                }
                                None => AttemptRead {
                                    outcome: self.cached_copy(
                                        database.as_ref(),
                                        root,
                                        &place,
                                        external_titles,
                                        stub,
                                    ),
                                    is_current: false,
                                },
                            }
                        }
                    });
                }
            },
        );
        if let Some(CacheDatabase::Writer(writer)) = &database
            && let Err(error) = writer.checkpoint_after_writes()
        {
            debug::warn(
                self.debug_level,
                &format!("Failed to checkpoint the session cache database: {error}"),
            );
        }
        reads.extend(unparsed.iter().map(|(stub, _)| AttemptRead {
            outcome: self.cached_copy(database.as_ref(), root, &place, external_titles, stub),
            is_current: false,
        }));
        reads
    }

    /// The listed row `database` holds for `stub`'s session, whether or not
    /// its transcripts changed since; `None` when it holds none, or the
    /// catalogue cannot be read, which reads the session whole.
    fn stored_listed_row(
        &self,
        database: &CacheDatabase,
        root: &SessionRoot,
        stub: &SessionStub,
    ) -> Option<Conversation> {
        let cached = database
            .reader()
            .catalogue_session(
                self.storage.cache(),
                self.storage.source(),
                root,
                &stub.cache_key,
                self.show_last,
            )
            .ok()
            .flatten();
        match cached {
            Some(CatalogueSession::Listed(row)) => Some(*row),
            _ => None,
        }
    }
}

/// A file's place in the first root's discovery that settles it.
enum Listing {
    /// Listed by the root at this index, as this stub.
    Listed(usize, SessionStub),
    /// Beneath a directory the discovery of the root at this index could not
    /// list, with the file's [`key_below`] that root.
    UnreadableDirectory(usize, BelowKey),
}

/// `locator` in `discovered`; `None` when every root's discovery leaves it
/// out. Every root is matched against the file as named before any is
/// matched against a canonical path, so a file as spelled never has its
/// directory canonicalised because an earlier root didn't hold it.
fn find_listed(
    discovered: &[(SessionRoot, DiscoveredSessions)],
    locator: &NamedFile,
) -> Option<Listing> {
    Spelling::IN_ORDER.into_iter().find_map(|spelling| {
        discovered
            .iter()
            .enumerate()
            .find_map(|(root_index, (root, found))| {
                let key = locator.key_below_as(root, spelling)?;
                if is_beneath_unreadable_directory(root, found, &key) {
                    return Some(Listing::UnreadableDirectory(root_index, key));
                }
                found
                    .stubs
                    .iter()
                    .find(|stub| key_below(root, &stub.locator).as_ref() == Some(&key))
                    .map(|stub| Listing::Listed(root_index, stub.clone()))
            })
    })
}

/// The ways a file a targeted read names is matched with a root, from the
/// cheapest to the one that canonicalises its directory.
#[derive(Clone, Copy)]
enum Spelling {
    /// Below the root or a directory beside it, as spelled.
    AsNamed,
    /// Below the canonical path of the root or a directory beside it, each
    /// canonicalised once per root.
    BelowCanonicalRoot,
    /// With its own directory canonicalised, at most once per file.
    ThroughCanonicalDirectory,
}

impl Spelling {
    const IN_ORDER: [Self; 3] = [
        Self::AsNamed,
        Self::BelowCanonicalRoot,
        Self::ThroughCanonicalDirectory,
    ];
}

/// A file a targeted read names, matched with the files a root's discovery
/// spells by its [`key_below`] the root, through each [`Spelling`] in order.
pub(super) struct NamedFile<'a> {
    path: &'a Path,
    canonicaliser: &'a dyn Canonicaliser,
    /// `path` with its directory canonical; `None` when the directory does
    /// not canonicalise. Set from the start for a canonical path.
    canonical: std::cell::OnceCell<Option<PathBuf>>,
}

impl<'a> NamedFile<'a> {
    pub(super) fn new(path: &'a Path, canonicaliser: &'a dyn Canonicaliser) -> Self {
        Self {
            path,
            canonicaliser,
            canonical: std::cell::OnceCell::new(),
        }
    }

    /// The file at `path`, a canonical path: its directory is not
    /// canonicalised again.
    pub(super) fn canonical(path: &'a Path, canonicaliser: &'a dyn Canonicaliser) -> Self {
        Self {
            path,
            canonicaliser,
            canonical: std::cell::OnceCell::from(Some(path.to_path_buf())),
        }
    }

    /// The file's [`key_below`] `root` through the first [`Spelling`] that
    /// places it; `None` outside it. A bare file name names a file in the
    /// current directory.
    fn key_below(&self, root: &SessionRoot) -> Option<BelowKey> {
        Spelling::IN_ORDER
            .into_iter()
            .find_map(|spelling| self.key_below_as(root, spelling))
    }

    /// The file's [`key_below`] `root` through `spelling`.
    fn key_below_as(&self, root: &SessionRoot, spelling: Spelling) -> Option<BelowKey> {
        let (position, below) = match spelling {
            Spelling::AsNamed => root.position_of(self.path),
            Spelling::BelowCanonicalRoot => {
                root.canonical_position_of(self.path, self.canonicaliser)
            }
            Spelling::ThroughCanonicalDirectory => {
                let canonical = self
                    .canonical
                    .get_or_init(|| {
                        let directory = match self.path.parent()? {
                            directory if directory.as_os_str().is_empty() => Path::new("."),
                            directory => directory,
                        };
                        let canonical = self.canonicaliser.canonicalise(directory).ok()?;
                        Some(canonical.join(self.path.file_name()?))
                    })
                    .as_deref()?;
                root.canonical_position_of(canonical, self.canonicaliser)
            }
        }?;
        Some(BelowKey {
            position,
            key: below.key(),
        })
    }

    /// True when `discovered`, a file `root`'s discovery spells, is this
    /// file.
    fn is(&self, root: &SessionRoot, discovered: &Path) -> bool {
        self.key_below(root)
            .is_some_and(|key| key_below(root, discovered) == Some(key))
    }
}

/// The key two spellings of a file below a root or a directory beside it
/// share: the directory's position and the key of the path below it, as
/// [`SessionRoot::position_of`] places it.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct BelowKey {
    position: usize,
    key: String,
}

impl BelowKey {
    /// True for a file below the root, not a directory beside it: true for
    /// every listed session's own transcript. A directory beside the root
    /// holds other files of listed sessions, such as Codex's archived
    /// rollouts from before an edit, and the rollouts of archived threads,
    /// which discovery doesn't list.
    fn is_below_root_itself(&self) -> bool {
        self.position == 0
    }
}

/// The [`BelowKey`] of `path`, a file below `root` or a directory beside
/// it; `None` for a path below neither.
fn key_below(root: &SessionRoot, path: &Path) -> Option<BelowKey> {
    root.position_of(path).map(|(position, below)| BelowKey {
        position,
        key: below.key(),
    })
}

/// True when the file whose [`key_below`] `root` is `key` lies beneath a
/// directory `found`, `root`'s discovery, could not list.
fn is_beneath_unreadable_directory(
    root: &SessionRoot,
    found: &DiscoveredSessions,
    key: &BelowKey,
) -> bool {
    found.unreadable_directories.iter().any(|directory| {
        key_below(root, &directory.path).is_some_and(|directory| {
            directory.position == key.position
                && Path::new(&key.key).starts_with(Path::new(&directory.key))
        })
    })
}
