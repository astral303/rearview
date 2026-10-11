//! The timings and counts `agent search --perf-stats` reports on stderr.

use crate::agent::refs::AgentConversationKey;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Runs `run`, recording it as `stage` when `perf` is collecting.
pub fn time_stage<T>(perf: Option<&SearchPerf>, stage: &'static str, run: impl FnOnce() -> T) -> T {
    match perf {
        Some(perf) => perf.time(stage, run),
        None => run(),
    }
}

pub struct SearchPerf {
    started: Instant,
    stages: Mutex<Vec<(&'static str, Duration)>>,
    sessions_opened: AtomicUsize,
    transcript_bytes: AtomicU64,
    read_nanos: AtomicU64,
    retrieval: Mutex<Duration>,
}

impl SearchPerf {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            stages: Mutex::new(Vec::new()),
            sessions_opened: AtomicUsize::new(0),
            transcript_bytes: AtomicU64::new(0),
            read_nanos: AtomicU64::new(0),
            retrieval: Mutex::new(Duration::ZERO),
        }
    }

    /// Runs `stage`, recording its wall time.
    pub fn time<T>(&self, stage: &'static str, run: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let result = run();
        self.stages.lock().unwrap().push((stage, started.elapsed()));
        result
    }

    /// Runs `read`, a read of `key`'s transcript, recording its time and the
    /// transcript's size.
    pub fn time_read<T>(&self, key: &AgentConversationKey, read: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let result = read();
        let elapsed = started.elapsed();
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
            self.started.elapsed().as_millis()
        ));
        records
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_name_each_stage_then_the_reads_retrieval_and_total() {
        let perf = SearchPerf::new();
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
        let perf = SearchPerf::new();

        perf.time_read(&first, || ());
        perf.time_read(&second, || ());

        assert!(
            perf.records()
                .contains("perf reads sessions=2 transcript-bytes=8 ")
        );
    }
}
