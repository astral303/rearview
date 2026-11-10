//! The `protocol agent-progress` record a running agent command writes to
//! stderr. A caller's shell tool can return early with whatever was printed,
//! and an agent reads empty output as a failure; the record shows the command
//! is still working.

use crate::agent::perf::CommandPerf;
use std::fmt::Write as _;
use std::io::Write;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

/// When progress records fall due: the first once the command has run for
/// `first`, then one every `every`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProgressSchedule {
    pub first: Duration,
    pub every: Duration,
}

impl ProgressSchedule {
    /// The first record well inside the 10 seconds after which Codex's shell
    /// tool returns what was printed so far.
    pub const DEFAULT: Self = Self {
        first: Duration::from_secs(2),
        every: Duration::from_secs(5),
    };
}

/// The progress records of one command, each built when it falls due from
/// the stage `perf` names then.
pub struct ProgressRecords<'a> {
    command: &'static str,
    perf: &'a CommandPerf,
    schedule: ProgressSchedule,
    next_due: Duration,
}

impl<'a> ProgressRecords<'a> {
    pub fn new(command: &'static str, perf: &'a CommandPerf, schedule: ProgressSchedule) -> Self {
        Self {
            command,
            perf,
            schedule,
            next_due: schedule.first,
        }
    }

    /// The wait from `elapsed` until the next record falls due; zero once due.
    fn wait_after(&self, elapsed: Duration) -> Duration {
        self.next_due.saturating_sub(elapsed)
    }

    /// The record due when the command has run for `elapsed`; `None` before
    /// the next one falls due.
    fn due_at(&mut self, elapsed: Duration) -> Option<String> {
        if elapsed < self.next_due {
            return None;
        }
        self.next_due = elapsed + self.schedule.every;
        let progress = self.perf.progress();
        let mut record = format!(
            "protocol agent-progress command={} state=running stage={}",
            self.command,
            progress.stage.name()
        );
        if let Some(items) = progress.items {
            let _ = write!(record, " done={} total={}", items.done, items.total);
        }
        if self.perf.load_meter().is_waiting_for_refresh() {
            record.push_str(" waiting=refresh-owner");
        }
        let _ = writeln!(record, " elapsed-ms={}", elapsed.as_millis());
        Some(record)
    }
}

/// Runs `command`, writing each progress record of `records` to `out` as it
/// falls due. Writing stops before `command`'s result is returned.
pub fn run_reporting_progress<T>(
    mut records: ProgressRecords<'_>,
    mut out: impl Write + Send,
    command: impl FnOnce() -> T,
) -> T {
    let perf = records.perf;
    let (finished, finish) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            loop {
                match finish.recv_timeout(records.wait_after(perf.elapsed())) {
                    Err(RecvTimeoutError::Timeout) => {
                        if let Some(record) = records.due_at(perf.elapsed()) {
                            let _ = out.write_all(record.as_bytes());
                            let _ = out.flush();
                        }
                    }
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        let result = command();
        drop(finished);
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::perf::Stage;
    use crate::history::Source;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn no_record_falls_due_before_first_interval() {
        let perf = CommandPerf::new(std::sync::Arc::new(crate::clock::TestClock::default()));
        let mut records = ProgressRecords::new("sessions", &perf, ProgressSchedule::DEFAULT);

        assert_eq!(records.due_at(ms(0)), None);
        assert_eq!(records.due_at(ms(1_999)), None);
    }

    #[test]
    fn record_falls_due_each_interval_naming_stage_then_running() {
        let perf = CommandPerf::new(std::sync::Arc::new(crate::clock::TestClock::default()));
        let codex = perf.load_meter().agent(Source::Codex);
        let mut records = ProgressRecords::new("sessions", &perf, ProgressSchedule::DEFAULT);

        perf.time(Stage::Load, || {
            codex.refreshing(860);
            for _ in 0..120 {
                codex.transcript_read();
            }
            assert_eq!(
                records.due_at(ms(2_000)).as_deref(),
                Some(
                    "protocol agent-progress command=sessions state=running stage=refresh done=120 total=860 elapsed-ms=2000\n"
                )
            );
            assert_eq!(records.due_at(ms(6_999)), None);
        });
        perf.time(Stage::Render, || {
            assert_eq!(
                records.due_at(ms(7_000)).as_deref(),
                Some(
                    "protocol agent-progress command=sessions state=running stage=render elapsed-ms=7000\n"
                )
            );
        });
    }

    #[test]
    fn command_finishing_before_first_interval_writes_no_record() {
        let perf = CommandPerf::new(std::sync::Arc::new(crate::clock::TestClock::default()));
        let mut out = Vec::new();

        let result = run_reporting_progress(
            ProgressRecords::new("read", &perf, ProgressSchedule::DEFAULT),
            &mut out,
            || "result",
        );

        assert_eq!(result, "result");
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn refresh_waiting_is_reported_for_current_command() {
        let perf = CommandPerf::new(std::sync::Arc::new(crate::clock::TestClock::default()));
        let other = CommandPerf::new(std::sync::Arc::new(crate::clock::TestClock::default()));
        let mut records = ProgressRecords::new("read", &perf, ProgressSchedule::DEFAULT);
        let mut unrelated = ProgressRecords::new("read", &other, ProgressSchedule::DEFAULT);
        let wait = perf.load_meter().wait_status();
        wait.set(Some(crate::history::OtherRearview::Rebuilding));
        assert!(
            records
                .due_at(ms(2_000))
                .unwrap()
                .contains(" waiting=refresh-owner")
        );
        assert!(!unrelated.due_at(ms(2_000)).unwrap().contains("waiting="));
        wait.set(None);
        assert!(!records.due_at(ms(7_000)).unwrap().contains("waiting="));
    }

    /// Output shared with the test, which learns when the first record is
    /// written and when the reporting thread lets go of it.
    struct WatchedOutput {
        written: Arc<Mutex<Vec<u8>>>,
        first_record: mpsc::Sender<()>,
        released: Arc<AtomicBool>,
    }

    impl Write for WatchedOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.written.lock().unwrap().extend_from_slice(bytes);
            let _ = self.first_record.send(());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for WatchedOutput {
        fn drop(&mut self) {
            self.released.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn records_stop_before_result_is_returned() {
        let perf = CommandPerf::new(std::sync::Arc::new(crate::clock::TestClock::default()));
        let written = Arc::new(Mutex::new(Vec::new()));
        let released = Arc::new(AtomicBool::new(false));
        let (first_record, first_record_written) = mpsc::channel();
        let every_moment = ProgressSchedule {
            first: Duration::ZERO,
            every: Duration::ZERO,
        };

        run_reporting_progress(
            ProgressRecords::new("search", &perf, every_moment),
            WatchedOutput {
                written: Arc::clone(&written),
                first_record,
                released: Arc::clone(&released),
            },
            || first_record_written.recv().unwrap(),
        );

        assert!(
            released.load(Ordering::SeqCst),
            "the reporting thread releases the output after the result"
        );
        let written = String::from_utf8(written.lock().unwrap().clone()).unwrap();
        assert!(
            written.starts_with("protocol agent-progress command=search state=running "),
            "{written}"
        );
        assert!(written.ends_with('\n'), "{written}");
    }
}
