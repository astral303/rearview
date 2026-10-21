//! The timings and counts `agent search --perf-stats` reports on stderr.

use crate::agent::refs::AgentConversationKey;
use crate::clock::{Clock, Moment};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Runs `run`, recording it as `stage` when `perf` is collecting.
pub fn time_stage<T>(perf: Option<&SearchPerf>, stage: &'static str, run: impl FnOnce() -> T) -> T {
    match perf {
        Some(perf) => perf.time(stage, run),
        None => run(),
    }
}

/// Runs `retrieve`, a match of one opened transcript, and returns its result
/// with the time it took when `perf` is collecting, zero otherwise. Add the
/// times of the matches a search uses with [`SearchPerf::add_retrieval_time`].
pub fn time_retrieval<T>(perf: Option<&SearchPerf>, retrieve: impl FnOnce() -> T) -> (T, Duration) {
    let Some(perf) = perf else {
        return (retrieve(), Duration::ZERO);
    };
    let started = perf.clock.now();
    let result = retrieve();
    (result, perf.clock.elapsed_since(started))
}

pub struct SearchPerf {
    clock: Arc<dyn Clock>,
    started: Moment,
    stages: Mutex<Vec<(&'static str, Duration)>>,
    sessions_opened: AtomicUsize,
    transcript_bytes: AtomicU64,
    read_nanos: AtomicU64,
    retrieval: Mutex<Duration>,
}

impl SearchPerf {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            started: clock.now(),
            clock,
            stages: Mutex::new(Vec::new()),
            sessions_opened: AtomicUsize::new(0),
            transcript_bytes: AtomicU64::new(0),
            read_nanos: AtomicU64::new(0),
            retrieval: Mutex::new(Duration::ZERO),
        }
    }

    /// Runs `stage`, recording its wall time.
    pub fn time<T>(&self, stage: &'static str, run: impl FnOnce() -> T) -> T {
        let started = self.clock.now();
        let result = run();
        self.stages
            .lock()
            .unwrap()
            .push((stage, self.clock.elapsed_since(started)));
        result
    }

    /// Runs `read`, a read of `key`'s transcript, recording its time and the
    /// transcript's size.
    pub fn time_read<T>(&self, key: &AgentConversationKey, read: impl FnOnce() -> T) -> T {
        let started = self.clock.now();
        let result = read();
        let elapsed = self.clock.elapsed_since(started);
        self.sessions_opened.fetch_add(1, Ordering::Relaxed);
        self.transcript_bytes
            .fetch_add(key.transcript_bytes.unwrap_or(0), Ordering::Relaxed);
        self.read_nanos.fetch_add(
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        result
    }

    /// Adds time the search threads spent matching opened transcripts.
    pub fn add_retrieval_time(&self, time: Duration) {
        *self.retrieval.lock().unwrap() += time;
    }

    /// One `perf` record per stage in the order they ran, then the transcript
    /// reads and retrieval, each summed across the threads that ran them.
    pub fn records(&self) -> String {
        let mut records = String::new();
        for (stage, time) in self.stages.lock().unwrap().iter() {
            records.push_str(&format!("perf stage={stage} ms={}\n", time.as_millis()));
        }
        records.push_str(&format!(
            "perf reads sessions={} transcript-bytes={} thread-ms={}\n",
            self.sessions_opened.load(Ordering::Relaxed),
            self.transcript_bytes.load(Ordering::Relaxed),
            Duration::from_nanos(self.read_nanos.load(Ordering::Relaxed)).as_millis()
        ));
        records.push_str(&format!(
            "perf retrieval thread-ms={}\n",
            self.retrieval.lock().unwrap().as_millis()
        ));
        records.push_str(&format!(
            "perf total ms={}\n",
            self.clock.elapsed_since(self.started).as_millis()
        ));
        records
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;

    fn perf() -> SearchPerf {
        SearchPerf::new(Arc::new(TestClock::default()))
    }

    #[test]
    fn records_name_each_stage_then_reads_retrieval_and_total() {
        let perf = perf();
        perf.time("load", || ());
        perf.time("ranking", || ());

        let records = perf.records();

        let names = records
            .lines()
            .map(|line| line.split(' ').nth(1).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["stage=load", "stage=ranking", "reads", "retrieval", "total"]
        );
    }

    #[test]
    fn reads_count_their_sessions_and_transcript_bytes() {
        let mut first = AgentConversationKey::new("project", "one.jsonl", "one.jsonl".into());
        first.transcript_bytes = Some(5);
        let mut second = AgentConversationKey::new("project", "two.jsonl", "two.jsonl".into());
        second.transcript_bytes = Some(3);
        let perf = perf();

        perf.time_read(&first, || ());
        perf.time_read(&second, || ());

        assert!(
            perf.records()
                .contains("perf reads sessions=2 transcript-bytes=8 ")
        );
    }

    #[test]
    fn records_report_the_time_each_stage_and_read_took_and_the_total() {
        let clock = Arc::new(TestClock::default());
        let perf = SearchPerf::new(clock.clone());
        let key = AgentConversationKey::new("project", "one.jsonl", "one.jsonl".into());

        perf.time("load", || clock.sleep(Duration::from_millis(40)));
        perf.time_read(&key, || clock.sleep(Duration::from_millis(25)));
        clock.advance(Duration::from_millis(5));

        let records = perf.records();
        assert!(records.contains("perf stage=load ms=40\n"), "{records}");
        assert!(records.contains(" thread-ms=25\n"), "{records}");
        assert!(records.contains("perf total ms=70\n"), "{records}");
    }
}
