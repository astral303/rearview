//! A load's reads from each agent, and the step it is on: `agent
//! --perf-stats` reports the first, the agent progress record the second.

use super::Source;
use super::cache::ShardRead;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// A step of one agent's load. Each root goes through discovery, then its
/// cache, then the refresh.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LoadStep {
    /// Walking the agent's roots and building a stub per session.
    #[default]
    Discovery,
    /// Reading and decoding a root's cache shards.
    Cache,
    /// Parsing the transcripts that are new or changed since the cache was
    /// written.
    Refresh,
}

/// A step's done and total items.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ItemCount {
    pub done: usize,
    pub total: usize,
}

/// The step a load is on, and its items when the step counts them: a root's
/// cache shards, or the transcripts the refresh parses.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StepProgress {
    pub step: LoadStep,
    /// `None` for discovery, which does not count its work.
    pub items: Option<ItemCount>,
}

/// One agent's load totals.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentLoad {
    pub source: Source,
    pub discovery: DiscoveryTotals,
    pub cache: CacheTotals,
    pub refresh: RefreshTotals,
}

impl AgentLoad {
    fn of(source: Source) -> Self {
        Self {
            source,
            discovery: DiscoveryTotals::default(),
            cache: CacheTotals::default(),
            refresh: RefreshTotals::default(),
        }
    }
}

/// The agent's roots walked and the sessions found under them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiscoveryTotals {
    pub roots: usize,
    pub sessions: usize,
    pub time: Duration,
}

/// The cache shard files read. Shards are read in parallel, so the times
/// are summed over the threads that read them and can exceed the load's
/// wall time.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheTotals {
    pub files: usize,
    pub bytes: u64,
    pub read_time: Duration,
    pub decode_time: Duration,
    pub entries: usize,
}

/// The transcripts parsed because they were new or changed, a session's own
/// and each sub-agent's, with the bytes of their sessions. Sessions are
/// parsed in parallel, so the time is summed over the threads that parsed
/// them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RefreshTotals {
    pub transcripts: usize,
    pub bytes: u64,
    pub time: Duration,
}

/// Every agent's load totals, and the step the load is on now. Shared by
/// the threads that load, and read by the thread that reports progress.
#[derive(Debug, Default)]
pub struct LoadMeter {
    agents: Mutex<Vec<AgentLoad>>,
    progress: Mutex<StepProgress>,
    /// The count of this load's threads waiting for another rearview's
    /// refresh of the session cache.
    refresh_waiters: AtomicUsize,
}

/// One thread's wait for another rearview's refresh, counted in its
/// [`LoadMeter`] until dropped.
pub(crate) struct RefreshWait<'a>(Option<&'a AtomicUsize>);

impl Drop for RefreshWait<'_> {
    fn drop(&mut self) {
        if let Some(waiters) = self.0 {
            waiters.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl LoadMeter {
    /// True while one of this load's threads waits for another rearview's
    /// refresh to release the session cache.
    pub(crate) fn is_waiting_for_refresh(&self) -> bool {
        self.refresh_waiters.load(Ordering::Relaxed) != 0
    }

    /// Count a wait for another rearview's refresh until the returned guard
    /// drops.
    pub(crate) fn start_refresh_wait(&self) -> RefreshWait<'_> {
        self.refresh_waiters.fetch_add(1, Ordering::Relaxed);
        RefreshWait(Some(&self.refresh_waiters))
    }

    /// The meter for `source`'s load.
    pub fn agent(&self, source: Source) -> AgentMeter<'_> {
        AgentMeter(Some((self, source)))
    }

    /// Each agent's load totals, in the order the agents started loading.
    pub fn agents(&self) -> Vec<AgentLoad> {
        self.agents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn progress(&self) -> StepProgress {
        *self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update_agent(&self, source: Source, update: impl FnOnce(&mut AgentLoad)) {
        let mut agents = self.agents.lock().unwrap_or_else(PoisonError::into_inner);
        let index = match agents.iter().position(|agent| agent.source == source) {
            Some(index) => index,
            None => {
                agents.push(AgentLoad::of(source));
                agents.len() - 1
            }
        };
        update(&mut agents[index]);
    }

    fn start_step(&self, step: LoadStep, total: Option<usize>) {
        *self.progress.lock().unwrap_or_else(PoisonError::into_inner) = StepProgress {
            step,
            items: total.map(|total| ItemCount { done: 0, total }),
        };
    }

    fn count_item_done(&self) {
        let mut progress = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(items) = &mut progress.items {
            items.done += 1;
        }
    }
}

/// Records one agent's load in a [`LoadMeter`], or records nothing for a load
/// nobody meters.
#[derive(Clone, Copy, Debug)]
pub struct AgentMeter<'a>(Option<(&'a LoadMeter, Source)>);

impl<'a> AgentMeter<'a> {
    pub const UNMETERED: Self = Self(None);

    /// Count a wait for another rearview's refresh in the meter until the
    /// returned guard drops; an unmetered load counts nothing.
    pub(crate) fn start_refresh_wait(self) -> RefreshWait<'a> {
        self.0
            .map_or(RefreshWait(None), |(meter, _)| meter.start_refresh_wait())
    }

    /// `source`'s meter in `meter`, or [`Self::UNMETERED`] without one.
    pub fn of(meter: Option<&'a LoadMeter>, source: Source) -> Self {
        meter.map_or(Self::UNMETERED, |meter| meter.agent(source))
    }

    pub fn discovering(self) {
        if let Some((meter, _)) = self.0 {
            meter.start_step(LoadStep::Discovery, None);
        }
    }

    /// Adds `roots` walked, finding `sessions`, in `time`.
    pub fn discovered(self, roots: usize, sessions: usize, time: Duration) {
        self.update(|agent| {
            agent.discovery.roots += roots;
            agent.discovery.sessions += sessions;
            agent.discovery.time += time;
        });
    }

    /// Starts reading a root's cache, `shards` of it.
    pub fn reading_cache(self, shards: usize) {
        if let Some((meter, _)) = self.0 {
            meter.start_step(LoadStep::Cache, Some(shards));
        }
    }

    /// One shard of the root's cache is done: `read` is its file's read, or
    /// `None` for a shard without a file this release can read.
    pub fn shard_done(self, read: Option<ShardRead>) {
        if let Some(read) = read {
            self.update(|agent| {
                agent.cache.files += 1;
                agent.cache.bytes += read.bytes;
                agent.cache.read_time += read.read_time;
                agent.cache.decode_time += read.decode_time;
                agent.cache.entries += read.entries;
            });
        }
        if let Some((meter, _)) = self.0 {
            meter.count_item_done();
        }
    }

    /// Starts parsing the `transcripts` of a root that missed its cache.
    pub fn refreshing(self, transcripts: usize) {
        if let Some((meter, _)) = self.0 {
            meter.start_step(LoadStep::Refresh, Some(transcripts));
        }
    }

    /// One transcript the refresh parses has been read.
    pub fn transcript_read(self) {
        if let Some((meter, _)) = self.0 {
            meter.count_item_done();
        }
    }

    /// Adds a session parsed in `time`: `transcripts` of it, `bytes` in all.
    pub fn session_parsed(self, transcripts: usize, bytes: u64, time: Duration) {
        self.update(|agent| {
            agent.refresh.transcripts += transcripts;
            agent.refresh.bytes += bytes;
            agent.refresh.time += time;
        });
    }

    fn update(self, update: impl FnOnce(&mut AgentLoad)) {
        if let Some((meter, source)) = self.0 {
            meter.update_agent(source, update);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn each_agent_gets_one_record_in_the_order_it_started_loading() {
        let meter = LoadMeter::default();

        meter.agent(Source::Codex).discovered(1, 3, MS);
        meter.agent(Source::Claude).discovered(2, 5, MS);
        meter.agent(Source::Codex).discovered(1, 4, MS);

        let agents = meter.agents();
        assert_eq!(
            agents
                .iter()
                .map(|agent| (
                    agent.source,
                    agent.discovery.roots,
                    agent.discovery.sessions
                ))
                .collect::<Vec<_>>(),
            [(Source::Codex, 2, 7), (Source::Claude, 2, 5)]
        );
        assert_eq!(agents[0].discovery.time, 2 * MS);
    }

    #[test]
    fn a_shard_read_adds_to_the_cache_totals_and_an_absent_shard_only_counts_as_done() {
        let meter = LoadMeter::default();
        let codex = meter.agent(Source::Codex);
        codex.reading_cache(16);

        codex.shard_done(Some(ShardRead {
            bytes: 100,
            read_time: MS,
            decode_time: 2 * MS,
            entries: 4,
        }));
        codex.shard_done(None);

        let cache = meter.agents()[0].cache;
        assert_eq!(
            (cache.files, cache.bytes, cache.entries),
            (1, 100, 4),
            "{cache:?}"
        );
        assert_eq!((cache.read_time, cache.decode_time), (MS, 2 * MS));
        assert_eq!(
            meter.progress(),
            StepProgress {
                step: LoadStep::Cache,
                items: Some(ItemCount { done: 2, total: 16 }),
            }
        );
    }

    #[test]
    fn the_refresh_counts_transcripts_read_of_those_it_parses() {
        let meter = LoadMeter::default();
        let codex = meter.agent(Source::Codex);
        codex.refreshing(3);

        codex.transcript_read();
        codex.transcript_read();
        codex.session_parsed(2, 500, MS);

        assert_eq!(
            meter.progress().items,
            Some(ItemCount { done: 2, total: 3 })
        );
        assert_eq!(
            meter.agents()[0].refresh,
            RefreshTotals {
                transcripts: 2,
                bytes: 500,
                time: MS,
            }
        );
    }

    #[test]
    fn discovery_doesnt_count_items() {
        let meter = LoadMeter::default();
        let codex = meter.agent(Source::Codex);
        codex.refreshing(3);

        codex.discovering();

        assert_eq!(meter.progress(), StepProgress::default());
    }
}
