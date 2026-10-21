//! The first build of the session cache database starts with a sample: each
//! provider's largest sessions and a spread of its others, written first.
//! Their size on disk per transcript byte projects the whole database's
//! peak size, and the build stops there when that would leave less than
//! [`FREE_SPACE_FLOOR`] free on the cache's drive.

use super::{CacheWriter, SqliteCache, free_space, is_busy_error, is_refused, refuse};
use crate::cli::DebugLevel;
use crate::error::{AppError, Result};
use crate::history::load_meter::{AgentMeter, LoadMeter};
use crate::history::provider::{SessionRoot, SessionStorage, SessionStub};
use std::path::{Path, PathBuf};

/// The free space a build leaves on the cache's drive: 15 GiB.
const FREE_SPACE_FLOOR: u64 = 15 * 1024 * 1024 * 1024;
/// Each provider's largest sessions in the sample.
const LARGEST_PER_PROVIDER: usize = 2;
/// Each provider's other sessions in the sample, spread evenly over its
/// sessions by size.
const SPREAD_PER_PROVIDER: usize = 8;
const SAMPLE_WRITE_HEADROOM: u64 = super::JOURNAL_SIZE_LIMIT;
// The transitional shard holds searchable text and its lowercase copy;
// SQLite adds a third projection. This allowance is an estimate, not a quota.
const SAMPLE_TRANSCRIPT_COPIES: u64 = 3;
// Large sessions can retain a different share of their transcript bytes
// than the rest of the corpus. Reserve an extra quarter beyond the sample.
const PROJECTION_HEADROOM_DIVISOR: u64 = 4;

/// One discovered session: its provider's storage, its root and its stub.
struct Candidate {
    storage: &'static dyn SessionStorage,
    root: SessionRoot,
    stub: SessionStub,
}

impl Candidate {
    fn transcript_bytes(&self) -> u64 {
        self.stub.fingerprint.size
    }
}

/// Sample measurements used to forecast database growth.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Projection {
    sampled_transcript_bytes: u64,
    sampled_database_bytes: u64,
    /// The write-ahead log's size when the sample was written: the log the
    /// build needs room for.
    sampled_log_bytes: u64,
    transcript_bytes: u64,
}

impl Projection {
    /// The database's size once every session is written, plus the room its
    /// write-ahead log needs.
    fn peak_bytes(self) -> u64 {
        let unsampled = self
            .transcript_bytes
            .saturating_sub(self.sampled_transcript_bytes);
        let per_transcript_byte = if self.sampled_transcript_bytes == 0 {
            0.0
        } else {
            self.sampled_database_bytes as f64 / self.sampled_transcript_bytes as f64
        };
        let log_room = self.sampled_log_bytes.max(super::JOURNAL_SIZE_LIMIT);
        let extrapolated = self
            .sampled_database_bytes
            .saturating_add((unsampled as f64 * per_transcript_byte) as u64)
            .saturating_add(log_room);
        with_headroom(extrapolated)
    }

    /// An error unless `free` bytes leave [`FREE_SPACE_FLOOR`] free once the
    /// database grows from its sampled size to its peak.
    fn admit(self, free: u64) -> Result<()> {
        admit_peak(self.peak_bytes(), self.sampled_database_bytes, free)
    }
}

fn with_headroom(bytes: u64) -> u64 {
    bytes.saturating_add(bytes.div_ceil(PROJECTION_HEADROOM_DIVISOR))
}

fn admit_peak(projected: u64, allocated: u64, free: u64) -> Result<()> {
    let growth = projected.saturating_sub(allocated);
    if free >= growth.saturating_add(FREE_SPACE_FLOOR) {
        return Ok(());
    }
    Err(AppError::SessionCacheBuildRefused {
        projected,
        free,
        floor: FREE_SPACE_FLOOR,
    })
}

pub(crate) fn admit_first_build(
    base: &Path,
    debug_level: Option<DebugLevel>,
    meter: Option<&LoadMeter>,
) -> Result<()> {
    if is_refused(base) {
        return Ok(());
    }
    let result = admit(base, debug_level, meter);
    if let Err(error) = &result
        && !matches!(error, AppError::SessionCacheRefreshLost)
        && !is_busy_error(error)
    {
        refuse(base);
    }
    result
}

fn check_capacity(base: &Path, free: Option<u64>, transcript_bytes: u64) -> Result<()> {
    let free = free.ok_or_else(|| AppError::SessionCacheCapacityUnknown(base.to_path_buf()))?;
    let writes = transcript_bytes
        .saturating_mul(SAMPLE_TRANSCRIPT_COPIES)
        .saturating_add(SAMPLE_WRITE_HEADROOM);
    admit_peak(with_headroom(writes), 0, free)
}

fn admit(base: &Path, debug_level: Option<DebugLevel>, meter: Option<&LoadMeter>) -> Result<()> {
    let storages: Vec<&'static dyn SessionStorage> = crate::history::provider::providers()
        .iter()
        .map(|provider| provider.storage())
        .collect();
    admit_storages(
        base,
        &storages,
        &|| free_space::available_bytes(base),
        debug_level,
        meter,
    )
}

/// [`admit`] the sessions of `storages`, with `available` reporting the free
/// space on the cache's drive.
fn admit_storages(
    base: &Path,
    storages: &[&'static dyn SessionStorage],
    available: &dyn Fn() -> Option<u64>,
    debug_level: Option<DebugLevel>,
    meter: Option<&LoadMeter>,
) -> Result<()> {
    let check_space = |bytes| check_capacity(base, available(), bytes);
    check_space(0)?;
    if SqliteCache::open_waiting(base)?.is_some() {
        return Ok(());
    }
    let ownership = super::refresh::RefreshOwnership::acquire(base, || {
        meter.map(LoadMeter::start_refresh_wait)
    })?;
    let owner = ownership.token();
    let mut cache = SqliteCache::open_for_refresh(owner)?;
    if cache.is_admitted()? {
        return Ok(());
    }
    cache.remove_every_session()?;
    let candidates = owner.write_batch(|| discover_every_storage(storages))??;
    let sample = sample_of(&candidates, storages);
    for candidate in &sample {
        cache.admit_storage(candidate.storage.cache())?;
    }
    write_sample(&mut cache, &sample, owner, debug_level, meter, &check_space)?;
    let log_bytes = file_size(&log_path(base));
    cache.checkpoint()?;
    let projection = Projection {
        sampled_transcript_bytes: sample
            .iter()
            .map(|candidate| candidate.transcript_bytes())
            .sum(),
        sampled_database_bytes: file_size(&super::database_path(base)),
        sampled_log_bytes: log_bytes,
        transcript_bytes: candidates.iter().map(Candidate::transcript_bytes).sum(),
    };
    let free = available();
    cache.record_projection(&projection_meta(projection, free))?;
    projection
        .admit(free.ok_or_else(|| AppError::SessionCacheCapacityUnknown(base.to_path_buf()))?)?;
    cache.mark_admitted()
}

/// Every session `storages` discover. The sessions under a directory
/// discovery cannot list are left out of the forecast's transcript total;
/// the build caches the readable ones, and loads keep the cached sessions
/// such a discovery misses.
fn discover_every_storage(storages: &[&'static dyn SessionStorage]) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    for &storage in storages {
        for root in storage.roots()? {
            let found = storage.discover(&root)?;
            candidates.extend(found.stubs.into_iter().map(|stub| Candidate {
                storage,
                root: root.clone(),
                stub,
            }));
        }
    }
    Ok(candidates)
}

/// Each storage's [`LARGEST_PER_PROVIDER`] largest sessions, and
/// [`SPREAD_PER_PROVIDER`] of its others spread evenly over them by size.
/// Only sessions with a fingerprint, which the database can hold.
fn sample_of<'a>(
    candidates: &'a [Candidate],
    storages: &[&'static dyn SessionStorage],
) -> Vec<&'a Candidate> {
    let mut sample = Vec::new();
    for storage in storages {
        let source = storage.source();
        let mut sessions: Vec<&Candidate> = candidates
            .iter()
            .filter(|candidate| {
                candidate.storage.source() == source && candidate.stub.fingerprint.stamp().is_some()
            })
            .collect();
        sessions.sort_by_key(|candidate| std::cmp::Reverse(candidate.transcript_bytes()));
        let others = sessions.split_off(LARGEST_PER_PROVIDER.min(sessions.len()));
        sample.extend(sessions);
        sample.extend(evenly_spread(&others, SPREAD_PER_PROVIDER));
    }
    sample
}

/// `count` of `items`, evenly spaced from the first to the last; all of them
/// when there are no more than `count`.
fn evenly_spread<T: Copy>(items: &[T], count: usize) -> Vec<T> {
    if items.len() <= count {
        return items.to_vec();
    }
    (0..count)
        .map(|index| items[index * (items.len() - 1) / (count - 1).max(1)])
        .collect()
}

/// Reads each sampled session as a load reads a changed one, and writes what
/// it read to `cache`.
fn write_sample(
    cache: &mut CacheWriter,
    sample: &[&Candidate],
    owner: &super::refresh::RefreshToken,
    debug_level: Option<DebugLevel>,
    meter: Option<&LoadMeter>,
    check_capacity: &dyn Fn(u64) -> Result<()>,
) -> Result<()> {
    let progress = sample.first().map_or(AgentMeter::UNMETERED, |candidate| {
        AgentMeter::of(meter, candidate.storage.source())
    });
    progress.refreshing(
        sample
            .iter()
            .map(|candidate| 1 + candidate.stub.subagents.len())
            .sum(),
    );
    for candidate in sample {
        let check_space = || check_capacity(candidate.transcript_bytes());
        let record = crate::history::provider::sample_record(
            candidate.storage,
            &candidate.root,
            &candidate.stub,
            owner,
            debug_level,
            AgentMeter::of(meter, candidate.storage.source()),
            &check_space,
        )?
        .ok_or_else(|| AppError::SessionCacheNotReady {
            session: candidate.stub.cache_key.clone(),
            reason: "the disk sample could not read this session".to_owned(),
        })?;
        let error = match &record.entries {
            Some(Err(error)) => Some(error.as_str()),
            _ if record.catalogue.status == super::SessionStatus::Unreadable => {
                Some("the disk sample could not read this session")
            }
            _ => None,
        };
        if let Some(error) = error {
            return Err(AppError::SessionCacheNotReady {
                session: candidate.stub.cache_key.clone(),
                reason: error.to_owned(),
            });
        }
        cache.write_checked(&record, &check_space)?;
    }
    Ok(())
}

fn projection_meta(projection: Projection, free: Option<u64>) -> Vec<(&'static str, String)> {
    let mut meta = vec![
        (
            "sample_transcript_bytes",
            projection.sampled_transcript_bytes.to_string(),
        ),
        (
            "sample_database_bytes",
            projection.sampled_database_bytes.to_string(),
        ),
        ("sample_log_bytes", projection.sampled_log_bytes.to_string()),
        ("transcript_bytes", projection.transcript_bytes.to_string()),
        ("projected_peak_bytes", projection.peak_bytes().to_string()),
        (
            "projection_headroom_percent",
            (100 / PROJECTION_HEADROOM_DIVISOR).to_string(),
        ),
    ];
    if let Some(free) = free {
        meta.push(("free_bytes_at_sample", free.to_string()));
    }
    meta
}

fn log_path(base: &Path) -> PathBuf {
    let mut log = super::database_path(base).into_os_string();
    log.push("-wal");
    PathBuf::from(log)
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |metadata| metadata.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn an_unreadable_sample_doesnt_supply_a_valid_size_measurement() {
        use crate::history::provider::{Fingerprint, SessionRoot, SessionStub};
        use std::time::{Duration, UNIX_EPOCH};
        let base = tempfile::tempdir().unwrap();
        let mut cache = SqliteCache::open(base.path()).unwrap();
        let owner = cache.token().clone();
        let candidate = Candidate {
            storage: crate::history::Source::Claude.provider().storage(),
            root: SessionRoot::new(base.path()),
            stub: SessionStub {
                locator: base.path().join("missing.jsonl"),
                subagents: Vec::new(),
                cache_key: "missing".to_owned(),
                fingerprint: Fingerprint {
                    size: 1000,
                    modified: Some(UNIX_EPOCH + Duration::from_secs(1)),
                },
            },
        };
        assert!(matches!(
            write_sample(&mut cache, &[&candidate], &owner, None, None, &|_| Ok(())),
            Err(AppError::SessionCacheNotReady { .. })
        ));
        assert!(!cache.is_admitted().unwrap());
    }

    #[test]
    fn unknown_capacity_and_a_sample_without_headroom_are_refused() {
        let base = Path::new("cache");
        assert!(matches!(
            check_capacity(base, None, 0),
            Err(AppError::SessionCacheCapacityUnknown(_))
        ));
        assert!(matches!(
            check_capacity(base, Some(FREE_SPACE_FLOOR + 1024 * 1024), 0),
            Err(AppError::SessionCacheBuildRefused { .. })
        ));
        assert!(
            check_capacity(base, Some(FREE_SPACE_FLOOR + SAMPLE_WRITE_HEADROOM * 2), 0).is_ok()
        );
    }

    #[test]
    fn a_large_sample_requires_its_own_write_allowance_before_parsing() {
        let base = Path::new("cache");
        let free = FREE_SPACE_FLOOR + with_headroom(SAMPLE_WRITE_HEADROOM);
        assert!(check_capacity(base, Some(free), 0).is_ok());
        assert!(matches!(
            check_capacity(base, Some(free), SAMPLE_WRITE_HEADROOM),
            Err(AppError::SessionCacheBuildRefused { .. })
        ));
        let needed = with_headroom(SAMPLE_WRITE_HEADROOM * (SAMPLE_TRANSCRIPT_COPIES + 1));
        assert!(
            check_capacity(base, Some(FREE_SPACE_FLOOR + needed), SAMPLE_WRITE_HEADROOM).is_ok()
        );
    }

    #[test]
    fn reopening_a_sample_row_doesnt_complete_disk_admission() {
        use std::cell::Cell;
        let base = tempfile::tempdir().unwrap();
        let mut cache = SqliteCache::open(base.path()).unwrap();
        let session = crate::history::provider::contract_tests::every_record_sessions().remove(0);
        let root = super::super::tests::root_of(&session);
        let storage = super::super::tests::storage_of(session.row.source);
        let record = super::super::SessionRecord::listed(
            storage,
            session.row.source,
            &root,
            &super::super::tests::stub_of(&session, 1),
            &session.row,
        )
        .unwrap();
        let checks = Cell::new(0);
        let exhausted = || {
            checks.set(checks.get() + 1);
            if checks.get() >= 3 {
                check_capacity(base.path(), Some(FREE_SPACE_FLOOR), 0)
            } else {
                Ok(())
            }
        };
        assert!(cache.write_checked(&record, &exhausted).is_err());
        assert!(cache.write(&record).is_err());
        assert!(!cache.is_admitted().unwrap());
        drop(cache);

        let cache = SqliteCache::open(base.path()).unwrap();
        assert!(!cache.is_admitted().unwrap());
        assert!(
            cache
                .current_fingerprints(storage, &root.path)
                .unwrap()
                .is_empty()
        );
        assert_eq!(cache.stored_keys(storage, &root.path).unwrap().len(), 1);
        cache.remove_every_session().unwrap();
        assert!(cache.stored_keys(storage, &root.path).unwrap().is_empty());
    }

    #[test]
    fn a_first_build_with_an_unreadable_directory_caches_every_providers_readable_sessions() {
        use crate::history::provider::contract_tests::{kept_storage_under, registered_fixtures};
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let storages: Vec<&'static dyn SessionStorage> = registered_fixtures()
            .into_iter()
            .take(2)
            .enumerate()
            .map(|(index, fixture)| {
                fixture.write_session(home.path(), fixture.ids().session);
                let unreadable = (index == 0).then(|| home.path().join("unreadable"));
                kept_storage_under(fixture, home.path(), unreadable)
            })
            .collect();

        admit_storages(base.path(), &storages, &|| Some(u64::MAX / 4), None, None).unwrap();

        assert!(!is_refused(base.path()));
        let cache = SqliteCache::open_waiting(base.path())
            .unwrap()
            .expect("the first build was not admitted");
        for storage in storages {
            let root = &storage.roots().unwrap()[0];
            assert_eq!(
                cache
                    .stored_keys(storage.cache(), &root.path)
                    .unwrap()
                    .len(),
                1,
                "{:?}'s readable session was not cached",
                storage.source()
            );
        }
    }

    #[test]
    fn an_admission_error_refuses_subsequent_writes_in_the_process() {
        let base = tempfile::tempdir().unwrap();
        std::fs::write(super::super::database_path(base.path()), "invalid database").unwrap();
        assert!(admit_first_build(base.path(), None, None).is_err());
        assert!(is_refused(base.path()));
    }

    #[test]
    fn resetting_an_interrupted_sample_doesnt_delete_an_admitted_build() {
        let base = tempfile::tempdir().unwrap();
        let mut cache = super::super::tests::admitted_cache(base.path());
        let (storage, root) = super::super::tests::write_test_entries(
            &mut cache,
            crate::history::TranscriptEntries {
                entries: Vec::new(),
                malformed_lines: Vec::new(),
            },
        );
        cache.remove_every_session().unwrap();
        assert_eq!(cache.stored_keys(storage, &root.path).unwrap().len(), 1);
    }

    fn projection(sampled_transcript_bytes: u64, sampled_database_bytes: u64) -> Projection {
        Projection {
            sampled_transcript_bytes,
            sampled_database_bytes,
            sampled_log_bytes: 0,
            transcript_bytes: 10 * GIB,
        }
    }

    #[test]
    fn the_peak_adds_headroom_to_the_sample_extrapolation_and_log_allowance() {
        let half = projection(GIB, GIB / 2);

        assert_eq!(half.peak_bytes(), 6 * GIB + GIB / 4 + 80 * 1024 * 1024);
    }

    #[test]
    fn a_build_that_only_fits_without_headroom_is_refused() {
        const MIB: u64 = 1024 * 1024;
        let sample = Projection {
            sampled_transcript_bytes: GIB,
            sampled_database_bytes: 100 * MIB,
            sampled_log_bytes: 64 * MIB,
            transcript_bytes: 2 * GIB,
        };
        let free = FREE_SPACE_FLOOR + 164 * MIB;
        assert!(
            matches!(sample.admit(free), Err(AppError::SessionCacheBuildRefused {
            projected, floor: FREE_SPACE_FLOOR, ..
        }) if projected == 330 * MIB)
        );
    }

    #[test]
    fn a_projection_beyond_the_integer_range_doesnt_wrap_to_a_small_build() {
        assert_eq!(projection(GIB, u64::MAX).peak_bytes(), u64::MAX);
    }

    #[test]
    fn a_build_that_leaves_the_floor_free_is_admitted() {
        let half = projection(GIB, GIB / 2);
        let growth = half.peak_bytes() - GIB / 2;

        assert!(half.admit(growth + FREE_SPACE_FLOOR).is_ok());
    }

    #[test]
    fn a_build_that_leaves_less_than_the_floor_names_its_peak_and_the_free_space() {
        let half = projection(GIB, GIB / 2);
        let free = half.peak_bytes() - GIB / 2 + FREE_SPACE_FLOOR - 1;

        let refused = half.admit(free).unwrap_err();

        assert!(
            matches!(
                refused,
                AppError::SessionCacheBuildRefused { projected, free: reported, floor }
                    if projected == half.peak_bytes() && reported == free && floor == FREE_SPACE_FLOOR
            ),
            "{refused:?}"
        );
    }

    #[test]
    fn the_spread_takes_the_first_the_last_and_evenly_spaced_items_between() {
        let items: Vec<usize> = (0..10).collect();

        assert_eq!(evenly_spread(&items, 4), [0, 3, 6, 9]);
        assert_eq!(evenly_spread(&items[..3], 4), [0, 1, 2]);
    }
}
