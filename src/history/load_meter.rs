//! A load's reads from each agent, and the step it is on: `agent
//! --perf-stats` reports the first, the agent progress record the second.

use super::Source;
use super::sqlite_cache::{OtherRearview, WaitStatus};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// A step of one agent's load. Each root goes through discovery, then its
/// cache, then the refresh.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LoadStep {
    /// Walking the agent's roots and building a stub per session.
    #[default]
    Discovery,
    /// Reading a root's sessions from the session cache database.
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

/// The step a load is on, and its items when the step counts them: the
/// transcripts the refresh parses.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StepProgress {
    pub step: LoadStep,
    /// `None` for discovery and the cache read, which do not count their
    /// work.
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

/// The session cache database's reads of a root's sessions, and the
/// sessions they held.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheTotals {
    pub reads: usize,
    pub sessions: usize,
    pub time: Duration,
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
    /// The load's wait for another rearview's hold on the session cache.
    wait: Arc<WaitStatus>,
}

impl LoadMeter {
    /// The other rearview the load waits for now; `None` while it does not
    /// wait.
    pub(crate) fn waiting_for(&self) -> Option<OtherRearview> {
        self.wait.current()
    }

    /// True while the load waits for another rearview's hold on the session
    /// cache.
    pub(crate) fn is_waiting_for_refresh(&self) -> bool {
        self.waiting_for().is_some()
    }

    /// The status the load's operation publishes its wait on.
    pub(crate) fn wait_status(&self) -> &Arc<WaitStatus> {
        &self.wait
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

    /// Starts reading a root's sessions from the session cache database.
    pub fn reading_cache(self) {
        if let Some((meter, _)) = self.0 {
            meter.start_step(LoadStep::Cache, None);
        }
    }

    /// Adds a read of a root's sessions from the database, holding
    /// `sessions`, in `time`.
    pub fn cache_read(self, sessions: usize, time: Duration) {
        self.update(|agent| {
            agent.cache.reads += 1;
            agent.cache.sessions += sessions;
            agent.cache.time += time;
        });
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
    fn each_agent_gets_one_record_in_order_it_started_loading() {
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
    fn each_cache_read_adds_its_sessions_and_time_to_cache_totals() {
        let meter = LoadMeter::default();
        let codex = meter.agent(Source::Codex);
        codex.reading_cache();

        codex.cache_read(4, MS);
        codex.cache_read(0, 2 * MS);

        assert_eq!(
            meter.agents()[0].cache,
            CacheTotals {
                reads: 2,
                sessions: 4,
                time: 3 * MS,
            }
        );
        assert_eq!(
            meter.progress(),
            StepProgress {
                step: LoadStep::Cache,
                items: None,
            }
        );
    }

    #[test]
    fn refresh_counts_transcripts_read_of_those_it_parses() {
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
