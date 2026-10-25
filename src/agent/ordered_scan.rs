//! Work over a ranked list on two thread pools, consumed in rank order.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::sync::mpsc;
use std::sync::{Condvar, Mutex, PoisonError};

/// Reads per core: more than the one search per core, because a read spends
/// part of its time waiting on the disk.
pub const READS_PER_CORE: usize = 2;

/// Bytes of items read or being read but not yet searched. One item more may
/// read past it at a time, so an item larger than the bound still reads. A
/// second item that does not fit waits for that place, and the items after it
/// that fit keep reading.
pub const READ_AHEAD_BYTES: u64 = 1 << 30;

/// What the consumer of [`scan_in_order`] wants after taking a result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanStep {
    Stop,
    /// Keep going. At most this many more results can be needed, so no item
    /// beyond them starts until a result shows more are needed.
    Continue {
        more_wanted: usize,
    },
}

/// How many reads and searches run at once, and how far reads run ahead.
#[derive(Clone, Copy, Debug)]
pub struct ScanPools {
    pub reads: usize,
    pub searches: usize,
    pub read_ahead_bytes: u64,
}

impl ScanPools {
    /// One search per core, [`READS_PER_CORE`] reads per core, and
    /// [`READ_AHEAD_BYTES`].
    pub fn for_this_machine() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
        Self {
            reads: cores.saturating_mul(READS_PER_CORE),
            searches: cores,
            read_ahead_bytes: READ_AHEAD_BYTES,
        }
    }
}

/// Reads `items` on one pool and searches each read item on another, starting
/// reads in item order, and hands each search's result to `consume` in item
/// order. Returns how many results `consume` took. `size` is an item's size in
/// bytes, for the read-ahead bound; `None` when it is unknown.
///
/// A read starts while it is within the larger of `first_wanted` (later, what
/// `consume` last asked for) and the read pool past the last result consumed,
/// and while the read-ahead bytes allow it. Past them, or at an unknown size,
/// a read starts only while no other item read past them is outstanding; until
/// then it waits, and later items that fit start ahead of it. Once `consume`
/// stops, no read or search starts; work already running finishes and its
/// results are dropped.
pub fn scan_in_order<T, D, R>(
    items: &[T],
    first_wanted: usize,
    size: impl Fn(&T) -> Option<u64> + Sync,
    read: impl Fn(&T) -> D + Sync,
    search: impl Fn(&T, D) -> R + Sync,
    consume: impl FnMut(R) -> ScanStep,
) -> usize
where
    T: Sync,
    D: Send,
    R: Send,
{
    scan_in_order_with(
        ScanPools::for_this_machine(),
        items,
        first_wanted,
        size,
        read,
        search,
        consume,
    )
}

/// The reads the pools may start and the read items waiting for a search.
struct Schedule<D> {
    /// The first item no read has tried yet.
    next_read: usize,
    /// Items a read tried while neither the read-ahead bytes nor the place
    /// past them had room. They start, earliest first, before untried items.
    waiting: BTreeSet<usize>,
    /// Items before this may start reading.
    limit: usize,
    stopped: bool,
    reading: usize,
    /// Bytes of the items read within the read-ahead bound, read or being
    /// read but not yet searched.
    unsearched_bytes: u64,
    /// The one item read past the read-ahead bound and not yet searched.
    past_bound: Option<usize>,
    read: BTreeMap<usize, D>,
}

impl<D> Schedule<D> {
    /// The earliest item a read may start now, its room reserved: a waiting
    /// item, then untried items, each one that does not fit moving to
    /// `waiting`. `None` while nothing before `limit` fits.
    fn claim_next(&mut self, sizes: &[Option<u64>], read_ahead_bytes: u64) -> Option<usize> {
        let waiting = self
            .waiting
            .iter()
            .copied()
            .take_while(|position| *position < self.limit)
            .collect::<Vec<_>>();
        for position in waiting {
            if self.reserve(position, sizes[position], read_ahead_bytes) {
                self.waiting.remove(&position);
                return Some(position);
            }
        }
        while self.next_read < sizes.len().min(self.limit) {
            let position = self.next_read;
            self.next_read += 1;
            if self.reserve(position, sizes[position], read_ahead_bytes) {
                return Some(position);
            }
            self.waiting.insert(position);
        }
        None
    }

    /// Reserves room for reading `position`, of `size`: within the read-ahead
    /// bytes, or as the one item past them. False while neither has room.
    fn reserve(&mut self, position: usize, size: Option<u64>, read_ahead_bytes: u64) -> bool {
        match size {
            Some(size) if self.unsearched_bytes.saturating_add(size) <= read_ahead_bytes => {
                self.unsearched_bytes += size;
            }
            _ if self.past_bound.is_none() => self.past_bound = Some(position),
            _ => return false,
        }
        self.reading += 1;
        true
    }

    /// True once every item has started reading.
    fn is_every_read_started(&self, item_count: usize) -> bool {
        self.next_read >= item_count && self.waiting.is_empty()
    }

    /// Releases what `position`, of `size`, held of the read-ahead bound once
    /// it is searched.
    fn finish_search(&mut self, position: usize, size: Option<u64>) {
        if self.past_bound == Some(position) {
            self.past_bound = None;
        } else {
            self.unsearched_bytes -= size.unwrap_or(0);
        }
    }
}

fn scan_in_order_with<T, D, R>(
    pools: ScanPools,
    items: &[T],
    first_wanted: usize,
    size: impl Fn(&T) -> Option<u64> + Sync,
    read: impl Fn(&T) -> D + Sync,
    search: impl Fn(&T, D) -> R + Sync,
    mut consume: impl FnMut(R) -> ScanStep,
) -> usize
where
    T: Sync,
    D: Send,
    R: Send,
{
    if items.len() <= 1 || (pools.reads <= 1 && pools.searches <= 1) {
        let mut consumed = 0;
        for item in items {
            consumed += 1;
            if consume(search(item, read(item))) == ScanStep::Stop {
                break;
            }
        }
        return consumed;
    }

    let sizes = items.iter().map(&size).collect::<Vec<_>>();
    let window = |more_wanted: usize| more_wanted.max(pools.reads).max(1);
    let schedule = Mutex::new(Schedule {
        next_read: 0,
        waiting: BTreeSet::new(),
        limit: window(first_wanted),
        stopped: false,
        reading: 0,
        unsearched_bytes: 0,
        past_bound: None,
        read: BTreeMap::new(),
    });
    let changed = Condvar::new();
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        let (schedule, changed, sizes) = (&schedule, &changed, &sizes);
        for _ in 0..pools.reads.min(items.len()) {
            let read = &read;
            scope.spawn(move || {
                let _stop = StopOnUnwind { schedule, changed };
                while let Some(position) = claim_read(schedule, changed, sizes, pools) {
                    let item = read(&items[position]);
                    let mut schedule = schedule.lock().unwrap();
                    schedule.reading -= 1;
                    schedule.read.insert(position, item);
                    changed.notify_all();
                }
            });
        }
        for _ in 0..pools.searches.min(items.len()) {
            let (sender, search) = (sender.clone(), &search);
            scope.spawn(move || {
                let _stop = StopOnUnwind { schedule, changed };
                while let Some((position, item)) = take_read(schedule, changed, items.len()) {
                    let result = search(&items[position], item);
                    let mut locked = schedule.lock().unwrap();
                    locked.finish_search(position, sizes[position]);
                    changed.notify_all();
                    drop(locked);
                    if sender.send((position, result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);

        let mut finished = BTreeMap::new();
        let mut consumed = 0;
        let mut stopped = false;
        for (position, result) in receiver {
            if stopped {
                continue;
            }
            finished.insert(position, result);
            while let Some(result) = finished.remove(&consumed) {
                consumed += 1;
                let step = consume(result);
                let mut schedule = schedule.lock().unwrap();
                match step {
                    ScanStep::Stop => {
                        schedule.stopped = true;
                        stopped = true;
                    }
                    ScanStep::Continue { more_wanted } => {
                        schedule.limit = consumed.saturating_add(window(more_wanted));
                    }
                }
                changed.notify_all();
                if stopped {
                    finished.clear();
                    break;
                }
            }
        }
        consumed
    })
}

/// The next item a read may start, waiting while the window or the read-ahead
/// bound allow none; `None` once every item has started or the scan stops.
fn claim_read<D>(
    schedule: &Mutex<Schedule<D>>,
    changed: &Condvar,
    sizes: &[Option<u64>],
    pools: ScanPools,
) -> Option<usize> {
    let mut schedule = schedule.lock().unwrap();
    loop {
        if schedule.stopped || schedule.is_every_read_started(sizes.len()) {
            return None;
        }
        if let Some(position) = schedule.claim_next(sizes, pools.read_ahead_bytes) {
            return Some(position);
        }
        schedule = changed.wait(schedule).unwrap();
    }
}

/// The earliest read item waiting for a search; `None` once the scan stops,
/// or every item has been read and taken.
fn take_read<D>(
    schedule: &Mutex<Schedule<D>>,
    changed: &Condvar,
    item_count: usize,
) -> Option<(usize, D)> {
    let mut schedule = schedule.lock().unwrap();
    loop {
        if schedule.stopped {
            return None;
        }
        if let Some(entry) = schedule.read.pop_first() {
            return Some(entry);
        }
        if schedule.reading == 0 && schedule.is_every_read_started(item_count) {
            return None;
        }
        schedule = changed.wait(schedule).unwrap();
    }
}

/// Stops the scan when a pool thread panics, so the other threads stop
/// waiting and the panic reaches the caller.
struct StopOnUnwind<'a, D> {
    schedule: &'a Mutex<Schedule<D>>,
    changed: &'a Condvar,
}

impl<D> Drop for StopOnUnwind<'_, D> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.schedule
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .stopped = true;
            self.changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn keep_going() -> ScanStep {
        ScanStep::Continue {
            more_wanted: usize::MAX,
        }
    }

    fn pools(reads: usize, searches: usize) -> ScanPools {
        ScanPools {
            reads,
            searches,
            read_ahead_bytes: u64::MAX,
        }
    }

    /// Keeps `count` at least as high as `current`.
    fn raise_to(count: &AtomicUsize, current: usize) {
        count.fetch_max(current, Ordering::Relaxed);
    }

    #[test]
    fn results_are_consumed_in_item_order_when_later_items_finish_first() {
        let items = (0..8).collect::<Vec<u64>>();
        let mut consumed = Vec::new();

        let count = scan_in_order_with(
            pools(4, 4),
            &items,
            usize::MAX,
            |_: &u64| Some(1),
            |item: &u64| {
                std::thread::sleep(Duration::from_millis((8 - item) * 5));
                *item
            },
            |_: &u64, item: u64| item,
            |item| {
                consumed.push(item);
                keep_going()
            },
        );

        assert_eq!(count, 8);
        assert_eq!(consumed, items);
    }

    #[test]
    fn stop_doesnt_start_remaining_items() {
        let items = (0..100).collect::<Vec<usize>>();
        let started = AtomicUsize::new(0);
        let mut consumed = Vec::new();

        let count = scan_in_order_with(
            pools(4, 4),
            &items,
            3,
            |_: &usize| Some(1),
            |item: &usize| {
                started.fetch_add(1, Ordering::Relaxed);
                *item
            },
            |_: &usize, item: usize| item,
            |item| {
                consumed.push(item);
                if item == 2 {
                    ScanStep::Stop
                } else {
                    ScanStep::Continue { more_wanted: 3 }
                }
            },
        );

        assert_eq!(count, 3);
        assert_eq!(consumed, [0, 1, 2]);
        // The last window before the stop, the read pool past the second
        // result, let items up to the sixth start.
        assert!(started.load(Ordering::Relaxed) <= 6);
    }

    #[test]
    fn items_beyond_wanted_results_and_read_pool_dont_start() {
        let items = (0..20).collect::<Vec<usize>>();
        let started = AtomicUsize::new(0);

        let count = scan_in_order_with(
            pools(1, 8),
            &items,
            3,
            |_: &usize| Some(1),
            |item: &usize| {
                started.fetch_add(1, Ordering::Relaxed);
                *item
            },
            |_: &usize, item: usize| item,
            |item| {
                let more_wanted = 2 - item;
                if more_wanted == 0 {
                    ScanStep::Stop
                } else {
                    ScanStep::Continue { more_wanted }
                }
            },
        );

        assert_eq!(count, 3);
        assert_eq!(started.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn reads_run_ahead_of_wanted_results_up_to_read_pool() {
        let items = (0..20).collect::<Vec<usize>>();
        let started = AtomicUsize::new(0);

        scan_in_order_with(
            pools(4, 1),
            &items,
            1,
            |_: &usize| Some(1),
            |item: &usize| {
                started.fetch_add(1, Ordering::Relaxed);
                *item
            },
            |_: &usize, item: usize| {
                // Hold the first search until the read pool has run ahead.
                let deadline = Instant::now() + Duration::from_secs(5);
                while item == 0 && started.load(Ordering::Relaxed) < 4 {
                    assert!(Instant::now() < deadline, "the reads didn't run ahead");
                    std::thread::sleep(Duration::from_millis(1));
                }
                item
            },
            |_| ScanStep::Stop,
        );

        assert_eq!(started.load(Ordering::Relaxed), 4);
    }

    /// Scans `sizes` on four reads and four searches with a read-ahead bound
    /// of 10 bytes, returning how many items were consumed and the most that
    /// were outstanding at once, from the start of a read to the end of its
    /// search.
    fn most_outstanding_with_sizes(sizes: &[Option<u64>]) -> (usize, usize) {
        let items = (0..sizes.len()).collect::<Vec<usize>>();
        let outstanding = AtomicUsize::new(0);
        let most_outstanding = AtomicUsize::new(0);

        let count = scan_in_order_with(
            ScanPools {
                reads: 4,
                searches: 4,
                read_ahead_bytes: 10,
            },
            &items,
            usize::MAX,
            |item: &usize| sizes[*item],
            |item: &usize| {
                let now = outstanding.fetch_add(1, Ordering::Relaxed) + 1;
                raise_to(&most_outstanding, now);
                std::thread::sleep(Duration::from_millis(2));
                *item
            },
            |_: &usize, item: usize| {
                outstanding.fetch_sub(1, Ordering::Relaxed);
                item
            },
            |_| keep_going(),
        );

        (count, most_outstanding.load(Ordering::Relaxed))
    }

    #[test]
    fn outstanding_items_stay_within_read_ahead_bytes_and_one_item_past_them() {
        let (count, most_outstanding) = most_outstanding_with_sizes(&[Some(4); 30]);

        assert_eq!(count, 30);
        // Two items of 4 bytes fit in the bound of 10; one more reads past it.
        assert!(most_outstanding <= 3);
    }

    #[test]
    fn two_items_past_read_ahead_bytes_are_never_outstanding_at_once() {
        let (count, most_outstanding) = most_outstanding_with_sizes(&[Some(100); 6]);

        assert_eq!(count, 6);
        assert_eq!(most_outstanding, 1);
    }

    #[test]
    fn items_of_unknown_size_read_past_bound_one_at_time() {
        let (count, most_outstanding) = most_outstanding_with_sizes(&[None; 6]);

        assert_eq!(count, 6);
        assert_eq!(most_outstanding, 1);
    }

    #[test]
    fn item_past_read_ahead_bytes_doesnt_hold_back_smaller_reads() {
        let items = (0..4).collect::<Vec<usize>>();
        let started = AtomicUsize::new(0);

        let count = scan_in_order_with(
            ScanPools {
                reads: 4,
                searches: 4,
                read_ahead_bytes: 10,
            },
            &items,
            usize::MAX,
            |item: &usize| if *item == 0 { Some(100) } else { Some(1) },
            |item: &usize| {
                started.fetch_add(1, Ordering::Relaxed);
                // Hold the large first read until every other read has started.
                let deadline = Instant::now() + Duration::from_secs(5);
                while *item == 0 && started.load(Ordering::Relaxed) < 4 {
                    assert!(
                        Instant::now() < deadline,
                        "the large read held back the rest"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                *item
            },
            |_: &usize, item: usize| item,
            |_| keep_going(),
        );

        assert_eq!(count, 4);
    }

    #[test]
    fn smaller_reads_start_ahead_of_second_item_past_read_ahead_bytes() {
        let sizes = [100, 100, 1, 1, 1];
        let items = (0..sizes.len()).collect::<Vec<usize>>();
        let started = Mutex::new(Vec::new());
        let mut consumed = Vec::new();

        scan_in_order_with(
            ScanPools {
                reads: 4,
                searches: 4,
                read_ahead_bytes: 10,
            },
            &items,
            usize::MAX,
            |item: &usize| Some(sizes[*item]),
            |item: &usize| {
                started.lock().unwrap().push(*item);
                // Hold the first large read, which takes the place past the
                // bound, until the small reads after the second have started.
                let deadline = Instant::now() + Duration::from_secs(5);
                while *item == 0 && started.lock().unwrap().len() < 4 {
                    assert!(
                        Instant::now() < deadline,
                        "the second large read held back the rest"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                if *item == 0 {
                    assert!(!started.lock().unwrap().contains(&1));
                }
                *item
            },
            |_: &usize, item: usize| item,
            |item| {
                consumed.push(item);
                keep_going()
            },
        );

        assert_eq!(consumed, items);
    }

    #[test]
    fn searches_stay_within_search_pool() {
        let items = (0..40).collect::<Vec<usize>>();
        let searching = AtomicUsize::new(0);
        let most_searching = AtomicUsize::new(0);

        scan_in_order_with(
            pools(8, 2),
            &items,
            usize::MAX,
            |_: &usize| Some(1),
            |item: &usize| *item,
            |_: &usize, item: usize| {
                let now = searching.fetch_add(1, Ordering::Relaxed) + 1;
                raise_to(&most_searching, now);
                std::thread::sleep(Duration::from_millis(1));
                searching.fetch_sub(1, Ordering::Relaxed);
                item
            },
            |_| keep_going(),
        );

        assert!(most_searching.load(Ordering::Relaxed) <= 2);
    }

    #[test]
    fn panicking_search_reaches_caller() {
        let items = (0..20).collect::<Vec<usize>>();

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scan_in_order_with(
                pools(4, 2),
                &items,
                usize::MAX,
                |_: &usize| Some(1),
                |item: &usize| *item,
                |_: &usize, item: usize| {
                    assert_ne!(item, 3, "search failed");
                    item
                },
                |_| keep_going(),
            )
        }));

        assert!(outcome.is_err());
    }

    #[test]
    fn one_thread_consumes_in_order_and_stops_at_stop() {
        let items = [10, 20, 30];
        let mut consumed = Vec::new();

        let count = scan_in_order_with(
            pools(1, 1),
            &items,
            usize::MAX,
            |_: &i32| Some(1),
            |item: &i32| *item,
            |_: &i32, item: i32| item,
            |item| {
                consumed.push(item);
                if item == 20 {
                    ScanStep::Stop
                } else {
                    keep_going()
                }
            },
        );

        assert_eq!(count, 2);
        assert_eq!(consumed, [10, 20]);
    }
}
