//! The timings and counts `agent --perf-stats` reports on stderr, and the
//! stage an agent command is in for its progress record.

use crate::agent::refs::AgentConversationKey;
use crate::agent::transcript::AgentTranscript;
use crate::clock::{Clock, Moment};
use crate::history::load_meter::{AgentLoad, ItemCount, LoadMeter, LoadStep};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

const BYTES_PER_MB: u64 = 1024 * 1024;

/// A stage of an agent command, timed by a `perf stage=` record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    /// Loading every agent's sessions, for `sessions` and `search`.
    Load,
    /// Keying the sessions the load discovered.
    Discovery,
    /// Reading the transcripts of sessions without a row, to word their
    /// warnings.
    SkippedTranscripts,
    Ranking,
    TranscriptSearch,
    Semantic,
    HitMetadata,
    /// Discovering every agent's sessions to key them, for `read`, `outline`
    /// and `within`.
    Keys,
    /// Parsing the transcripts a `read`, `outline` or `within` reads.
    Transcript,
    /// Matching inside one transcript, for `within`.
    Search,
    Render,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Discovery => "discovery",
            Self::SkippedTranscripts => "skipped-transcripts",
            Self::Ranking => "ranking",
            Self::TranscriptSearch => "transcript-search",
            Self::Semantic => "semantic",
            Self::HitMetadata => "hit-metadata",
            Self::Keys => "keys",
            Self::Transcript => "transcript",
            Self::Search => "search",
            Self::Render => "render",
        }
    }
}

/// Runs `retrieve`, a match of one opened transcript, and returns its result
/// with the time it took when `reads` is collecting, zero otherwise. Add the
/// times of the matches a search uses with [`SearchReads::add_retrieval_time`].
pub fn time_retrieval<T>(
    reads: Option<&SearchReads>,
    retrieve: impl FnOnce() -> T,
) -> (T, Duration) {
    let Some(reads) = reads else {
        return (retrieve(), Duration::ZERO);
    };
    let started = reads.clock.now();
    let result = retrieve();
    (result, reads.clock.elapsed_since(started))
}

/// The stage a progress record names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressStage {
    Discovery,
    Cache,
    /// Parsing transcripts that are new or changed since the cache was
    /// written.
    Refresh,
    /// Parsing the transcript a command reads.
    Transcript,
    Search,
    Render,
}

impl ProgressStage {
    pub fn name(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Cache => "cache",
            Self::Refresh => "refresh",
            Self::Transcript => "transcript",
            Self::Search => "search",
            Self::Render => "render",
        }
    }
}

/// The stage running now, and its items when it counts them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandProgress {
    pub stage: ProgressStage,
    pub items: Option<ItemCount>,
}

impl CommandProgress {
    fn uncounted(stage: ProgressStage) -> Self {
        Self { stage, items: None }
    }
}

/// An agent command's record so far: the stage it is in, each stage's time,
/// the load's reads, each transcript parsed, and a global search's
/// transcript reads.
pub struct CommandPerf {
    clock: Arc<dyn Clock>,
    started: Moment,
    stages: Mutex<Vec<(Stage, Duration)>>,
    /// The stage that started last; `None` before the first.
    current: Mutex<Option<Stage>>,
    load: LoadMeter,
    transcripts: Mutex<Vec<TranscriptParse>>,
    /// `None` until a global search starts.
    search: OnceLock<SearchReads>,
}

/// One transcript parsed.
#[derive(Clone, Copy, Debug)]
struct TranscriptParse {
    time: Duration,
    bytes: u64,
    messages: usize,
}

impl CommandPerf {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            started: clock.now(),
            clock,
            stages: Mutex::default(),
            current: Mutex::default(),
            load: LoadMeter::default(),
            transcripts: Mutex::default(),
            search: OnceLock::new(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.clock.elapsed_since(self.started)
    }

    /// Runs `run` as `stage`, recording its wall time.
    pub fn time<T>(&self, stage: Stage, run: impl FnOnce() -> T) -> T {
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(stage);
        let started = self.clock.now();
        let result = run();
        self.stages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((stage, self.clock.elapsed_since(started)));
        result
    }

    /// The meter the command's load records what it read in.
    pub fn load_meter(&self) -> &LoadMeter {
        &self.load
    }

    /// Runs `parse`, a parse of `key`'s transcript, and records it when it
    /// succeeds. A failed parse is reported by the command's error or
    /// warning.
    pub fn time_transcript_parse<E>(
        &self,
        key: &AgentConversationKey,
        parse: impl FnOnce() -> Result<AgentTranscript, E>,
    ) -> Result<AgentTranscript, E> {
        let started = self.clock.now();
        let parsed = parse();
        if let Ok(transcript) = &parsed {
            self.transcripts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(TranscriptParse {
                    time: self.clock.elapsed_since(started),
                    bytes: key.transcript_bytes.unwrap_or(0),
                    messages: transcript.messages.len(),
                });
        }
        parsed
    }

    /// The transcript reads of the command's global search, recorded from
    /// the first call on.
    pub fn search_reads(&self) -> &SearchReads {
        self.search
            .get_or_init(|| SearchReads::new(Arc::clone(&self.clock)))
    }

    pub fn progress(&self) -> CommandProgress {
        let current = *self.current.lock().unwrap_or_else(PoisonError::into_inner);
        match current {
            None | Some(Stage::Load | Stage::Keys) => self.load_progress(),
            Some(Stage::Discovery) => CommandProgress::uncounted(ProgressStage::Discovery),
            Some(Stage::SkippedTranscripts | Stage::Transcript) => {
                CommandProgress::uncounted(ProgressStage::Transcript)
            }
            Some(
                Stage::Ranking
                | Stage::TranscriptSearch
                | Stage::Semantic
                | Stage::HitMetadata
                | Stage::Search,
            ) => CommandProgress::uncounted(ProgressStage::Search),
            Some(Stage::Render) => CommandProgress::uncounted(ProgressStage::Render),
        }
    }

    fn load_progress(&self) -> CommandProgress {
        let progress = self.load.progress();
        CommandProgress {
            stage: match progress.step {
                LoadStep::Discovery => ProgressStage::Discovery,
                LoadStep::Cache => ProgressStage::Cache,
                LoadStep::Refresh => ProgressStage::Refresh,
            },
            items: progress.items,
        }
    }

    /// One `perf` record per stage in the order they ran; each agent's
    /// discovery, cache and refresh, agents in the order they loaded; each
    /// transcript parsed; a global search's reads and retrieval; the
    /// `peak_memory` in bytes when the platform reports it; then the total.
    pub fn records(&self, peak_memory: Option<u64>) -> String {
        let mut records = String::new();
        for (stage, time) in self
            .stages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                records,
                "perf stage={} ms={}",
                stage.name(),
                time.as_millis()
            );
        }
        for agent in self.load.agents() {
            write_agent_load(&mut records, &agent);
        }
        for parse in self
            .transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                records,
                "perf transcript ms={} bytes={} messages={}",
                parse.time.as_millis(),
                parse.bytes,
                parse.messages
            );
        }
        if let Some(search) = self.search.get() {
            search.write_records(&mut records);
        }
        if let Some(bytes) = peak_memory {
            let _ = writeln!(records, "perf memory peak-mb={}", bytes / BYTES_PER_MB);
        }
        let _ = writeln!(records, "perf total ms={}", self.elapsed().as_millis());
        records
    }
}

fn write_agent_load(records: &mut String, agent: &AgentLoad) {
    let source = agent.source.label();
    let AgentLoad {
        discovery,
        cache,
        refresh,
        ..
    } = agent;
    let _ = writeln!(
        records,
        "perf discovery source={source} roots={} sessions={} ms={}",
        discovery.roots,
        discovery.sessions,
        discovery.time.as_millis()
    );
    let _ = writeln!(
        records,
        "perf cache source={source} reads={} sessions={} ms={}",
        cache.reads,
        cache.sessions,
        cache.time.as_millis()
    );
    let _ = writeln!(
        records,
        "perf refresh source={source} parsed={} bytes={} ms={}",
        refresh.transcripts,
        refresh.bytes,
        refresh.time.as_millis()
    );
}

/// A global search's transcript reads, and the time its threads spent
/// matching them.
pub struct SearchReads {
    clock: Arc<dyn Clock>,
    sessions_opened: AtomicUsize,
    transcript_bytes: AtomicU64,
    read_nanos: AtomicU64,
    retrieval: Mutex<Duration>,
}

impl SearchReads {
    fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            sessions_opened: AtomicUsize::new(0),
            transcript_bytes: AtomicU64::new(0),
            read_nanos: AtomicU64::new(0),
            retrieval: Mutex::new(Duration::ZERO),
        }
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
        *self
            .retrieval
            .lock()
            .unwrap_or_else(PoisonError::into_inner) += time;
    }

    /// The reads and the retrieval, each summed across the threads that ran
    /// them.
    fn write_records(&self, records: &mut String) {
        let _ = writeln!(
            records,
            "perf reads sessions={} transcript-bytes={} thread-ms={}",
            self.sessions_opened.load(Ordering::Relaxed),
            self.transcript_bytes.load(Ordering::Relaxed),
            Duration::from_nanos(self.read_nanos.load(Ordering::Relaxed)).as_millis()
        );
        let _ = writeln!(
            records,
            "perf retrieval thread-ms={}",
            self.retrieval
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_millis()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use crate::history::Source;

    fn perf() -> CommandPerf {
        CommandPerf::new(Arc::new(TestClock::default()))
    }

    /// Each record's name: `stage=NAME` for a stage, the second word
    /// otherwise.
    fn record_names(records: &str) -> Vec<&str> {
        records
            .lines()
            .map(|line| line.split(' ').nth(1).unwrap())
            .collect()
    }

    fn key_of(bytes: u64) -> AgentConversationKey {
        let mut key = AgentConversationKey::new("project", "one.jsonl", "one.jsonl".into());
        key.transcript_bytes = Some(bytes);
        key
    }

    fn transcript_of(messages: usize) -> AgentTranscript {
        use crate::agent::test_support::{text_message, transcript};
        use crate::agent::transcript::AgentMessageRole;
        transcript(
            (1..=messages)
                .map(|ordinal| text_message(ordinal, AgentMessageRole::User, "a question"))
                .collect(),
            "one.jsonl",
        )
    }

    #[test]
    fn records_name_each_stage_each_agents_load_each_transcript_then_memory_and_total() {
        let perf = perf();
        perf.time(Stage::Keys, || {
            perf.load_meter()
                .agent(Source::Codex)
                .discovered(1, 2, Duration::ZERO);
        });
        perf.time(Stage::Transcript, || {
            perf.time_transcript_parse(&key_of(10), || Ok::<_, ()>(transcript_of(1)))
        })
        .unwrap();
        perf.time(Stage::Render, || ());

        let records = perf.records(Some(3 * BYTES_PER_MB));

        assert_eq!(
            record_names(&records),
            [
                "stage=keys",
                "stage=transcript",
                "stage=render",
                "discovery",
                "cache",
                "refresh",
                "transcript",
                "memory",
                "total",
            ],
            "{records}"
        );
        assert!(
            records.contains("perf discovery source=codex roots=1 sessions=2 ms=0\n"),
            "{records}"
        );
        assert!(
            records.contains("perf transcript ms=0 bytes=10 messages=1\n"),
            "{records}"
        );
        assert!(records.contains("perf memory peak-mb=3\n"), "{records}");
    }

    #[test]
    fn a_search_adds_its_reads_and_retrieval_before_memory_and_total() {
        let perf = perf();
        perf.time(Stage::Load, || ());
        perf.time(Stage::TranscriptSearch, || {
            perf.search_reads().time_read(&key_of(5), || ());
            perf.search_reads().time_read(&key_of(3), || ());
        });

        let records = perf.records(None);

        assert_eq!(
            record_names(&records),
            [
                "stage=load",
                "stage=transcript-search",
                "reads",
                "retrieval",
                "total",
            ],
            "{records}"
        );
        assert!(
            records.contains("perf reads sessions=2 transcript-bytes=8 "),
            "{records}"
        );
    }

    #[test]
    fn records_report_the_time_each_stage_and_read_took_and_the_total() {
        let clock = Arc::new(TestClock::default());
        let perf = CommandPerf::new(clock.clone());

        perf.time(Stage::Load, || clock.sleep(Duration::from_millis(40)));
        perf.search_reads()
            .time_read(&key_of(1), || clock.sleep(Duration::from_millis(25)));
        clock.advance(Duration::from_millis(5));

        let records = perf.records(None);
        assert!(records.contains("perf stage=load ms=40\n"), "{records}");
        assert!(records.contains(" thread-ms=25\n"), "{records}");
        assert!(records.contains("perf total ms=70\n"), "{records}");
    }

    #[test]
    fn a_failed_transcript_parse_isnt_recorded() {
        let perf = perf();

        let parsed = perf.time_transcript_parse(&key_of(10), || Err::<AgentTranscript, _>("bad"));

        assert!(parsed.is_err());
        assert!(!perf.records(None).contains("perf transcript"));
    }

    #[test]
    fn progress_names_the_load_step_while_loading_and_the_stage_after() {
        let perf = perf();
        let codex = perf.load_meter().agent(Source::Codex);

        assert_eq!(perf.progress().stage, ProgressStage::Discovery);
        perf.time(Stage::Load, || {
            codex.refreshing(4);
            codex.transcript_read();
            assert_eq!(
                perf.progress(),
                CommandProgress {
                    stage: ProgressStage::Refresh,
                    items: Some(ItemCount { done: 1, total: 4 }),
                }
            );
        });
        perf.time(Stage::TranscriptSearch, || {
            assert_eq!(
                perf.progress(),
                CommandProgress::uncounted(ProgressStage::Search)
            );
        });
    }
}
