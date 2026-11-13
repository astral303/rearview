//! The TUI's one background worker: the launch, list `Ctrl+R`, session
//! refreshes, renames and deletes run on it one at a time, and their messages
//! come back to the runtime through the supervisor.

use super::list_change::{ListChange, ListChangeOutcome};
use crate::error::{AppError, Result};
use crate::history::load_meter::LoadMeter;
use crate::history::provider::{DirectFileRead, open_direct_file};
use crate::history::sqlite_cache::{Coordinator, SHUTDOWN_RELEASE_WAIT};
use crate::history::{
    Cancellation, Environment, LoaderMessage, OperationScope, OtherRearview, Patience,
    RefreshOptions, RefreshOutcome, WaitStatus, stream_all_conversations,
};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

type Job = Box<dyn FnOnce() + Send>;

/// The worker thread, the running job's channel, and the waits jobs
/// publish. The runtime hands it a job only when [`Self::can_take_job`].
pub(crate) struct TaskSupervisor {
    environment: Environment,
    /// `None` once shut down: the worker ends after its running job.
    worker: Option<Sender<Job>>,
    /// The running launch's messages.
    launch: Option<Receiver<LoaderMessage>>,
    load_meter: Arc<LoadMeter>,
    /// The running list or session refresh's outcome.
    refresh: Option<Receiver<Result<RefreshOutcome>>>,
    /// The running rename or delete's outcome, and its outcome should the
    /// job stop without one.
    change: Option<(Receiver<ListChangeOutcome>, ListChangeOutcome)>,
    /// The running read of a file opened directly.
    direct_file: Option<Receiver<Result<DirectFileRead>>>,
    /// The wait a list or session refresh, a rename or a delete publishes.
    refresh_wait: Arc<WaitStatus>,
    /// Set at shutdown: the running job doesn't parse any more transcripts.
    cancellation: Cancellation,
}

impl TaskSupervisor {
    /// The worker for jobs on `environment`'s sessions. A launch records
    /// what it reads in `load_meter`.
    pub(crate) fn start(environment: Environment, load_meter: Arc<LoadMeter>) -> Self {
        let (worker, jobs) = mpsc::channel::<Job>();
        std::thread::spawn(move || {
            for job in jobs {
                // A job that panics drops its channel. The runtime reads a
                // dropped channel as a job that stopped, and the next job runs.
                let _ = catch_unwind(AssertUnwindSafe(job));
            }
        });
        Self {
            environment,
            worker: Some(worker),
            launch: None,
            load_meter,
            refresh: None,
            change: None,
            direct_file: None,
            refresh_wait: Arc::default(),
            cancellation: Cancellation::default(),
        }
    }

    /// One job's operation on the session cache: it waits for refresh
    /// ownership as long as another rearview keeps it, publishes its wait,
    /// and stops at shutdown.
    fn job_operation(&self) -> impl FnOnce(&Environment) -> OperationScope + Send + 'static {
        let wait = Arc::clone(&self.refresh_wait);
        let cancellation = self.cancellation.clone();
        move |environment| {
            OperationScope::new(environment.cache_base(), Patience::UntilAcquired, wait)
                .cancelled_by(&cancellation)
        }
    }

    /// The session cache this supervisor's jobs read and write.
    pub(crate) fn cache_base(&self) -> &crate::history::CacheBase {
        self.environment.cache_base()
    }

    /// True while the worker doesn't run a job and is not shut down.
    pub(crate) fn can_take_job(&self) -> bool {
        self.worker.is_some() && !self.is_running_a_job()
    }

    /// True while a job runs.
    pub(crate) fn is_running_a_job(&self) -> bool {
        self.launch.is_some()
            || self.refresh.is_some()
            || self.change.is_some()
            || self.direct_file.is_some()
    }

    /// Run the launch, reading sessions with `options`. Its messages come
    /// through [`Self::next_launch_message`].
    pub(crate) fn launch(&mut self, options: RefreshOptions) {
        let (messages, received) = mpsc::channel();
        let environment = self.environment.clone();
        let meter = Arc::clone(&self.load_meter);
        let cancellation = self.cancellation.clone();
        self.launch = Some(received);
        self.run(Box::new(move || {
            stream_all_conversations(&messages, &environment, options, &meter, &cancellation);
        }));
    }

    /// Run `refresh` as one operation on the session cache that waits for
    /// refresh ownership as long as another rearview keeps it. Its outcome
    /// comes through [`Self::finished_refresh`], failed with the low-space
    /// error of [`OperationScope::finish`].
    pub(crate) fn refresh(
        &mut self,
        refresh: impl FnOnce(&Environment, &OperationScope) -> Result<RefreshOutcome> + Send + 'static,
    ) {
        let (outcome, received) = mpsc::channel();
        let environment = self.environment.clone();
        let operation = self.job_operation();
        self.refresh = Some(received);
        self.run(Box::new(move || {
            let operation = operation(&environment);
            let refreshed = refresh(&environment, &operation);
            let _ = outcome.send(operation.finish().fail_if_low_space().and(refreshed));
        }));
    }

    /// Make `change` as one operation on the session cache that waits for
    /// refresh ownership as long as another rearview keeps it. Its outcome
    /// comes through [`Self::finished_change`].
    pub(crate) fn change(&mut self, change: ListChange) {
        let (outcome, received) = mpsc::channel();
        let environment = self.environment.clone();
        let operation = self.job_operation();
        self.change = Some((received, change.stopped()));
        self.run(Box::new(move || {
            let operation = operation(&environment);
            let changed = change.make(&operation);
            // Ignore the operation's end: a rename or delete doesn't check
            // free space, and its own outcome carries the error of a write
            // that stopped it.
            let _ = operation.finish();
            let _ = outcome.send(changed);
        }));
    }

    /// Read the file at `path`, opened directly, with the preview `show_last`
    /// selects, as one operation on the session cache that waits for refresh
    /// ownership as long as another rearview keeps it. Its read comes through
    /// [`Self::finished_direct_file`], failed with the low-space error of
    /// [`OperationScope::finish`].
    pub(crate) fn read_direct_file(&mut self, path: PathBuf, show_last: bool) {
        let (read, received) = mpsc::channel();
        let environment = self.environment.clone();
        let operation = self.job_operation();
        self.direct_file = Some(received);
        self.run(Box::new(move || {
            let operation = operation(&environment);
            let opened = open_direct_file(&environment, &path, show_last, &operation);
            let _ = read.send(operation.finish().fail_if_low_space().and(opened));
        }));
    }

    /// Hand `job` to the worker. After shutdown the job is dropped, and its
    /// channel reads as a job that stopped.
    fn run(&self, job: Job) {
        if let Some(worker) = &self.worker {
            let _ = worker.send(job);
        }
    }

    /// The launch's next message; `None` while none waits or no launch runs.
    /// The launch is over after [`LoaderMessage::Done`] or a fatal error. A
    /// launch that stops without either ends with `Done`, so the list shows
    /// what it streamed.
    pub(crate) fn next_launch_message(&mut self) -> Option<LoaderMessage> {
        let message = match self.launch.as_ref()?.try_recv() {
            Ok(message) => message,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => LoaderMessage::Done,
        };
        if matches!(message, LoaderMessage::Done | LoaderMessage::Fatal(_)) {
            self.launch = None;
        }
        Some(message)
    }

    /// The running refresh's outcome once it has one: a refresh that stopped
    /// without one fails with [`AppError::RefreshStopped`].
    pub(crate) fn finished_refresh(&mut self) -> Option<Result<RefreshOutcome>> {
        let outcome = match self.refresh.as_ref()?.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(AppError::RefreshStopped),
        };
        self.refresh = None;
        Some(outcome)
    }

    /// The running rename or delete's outcome once it has one: a job that
    /// stopped without one fails with [`AppError::RefreshStopped`].
    pub(crate) fn finished_change(&mut self) -> Option<ListChangeOutcome> {
        let (received, _) = self.change.as_ref()?;
        let finished = match received.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => None,
        };
        let (_, stopped) = self.change.take()?;
        Some(finished.unwrap_or(stopped))
    }

    /// The running read of a file opened directly once it has one: a read
    /// that stopped without one fails with [`AppError::RefreshStopped`].
    pub(crate) fn finished_direct_file(&mut self) -> Option<Result<DirectFileRead>> {
        let read = match self.direct_file.as_ref()?.try_recv() {
            Ok(read) => read,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(AppError::RefreshStopped),
        };
        self.direct_file = None;
        Some(read)
    }

    /// The other rearview the running job waits for; `None` while it does
    /// not wait or no job runs.
    pub(crate) fn waiting_for(&self) -> Option<OtherRearview> {
        if self.launch.is_some() {
            self.load_meter.waiting_for()
        } else if self.refresh.is_some() || self.change.is_some() || self.direct_file.is_some() {
            self.refresh_wait.current()
        } else {
            None
        }
    }

    /// Stop taking jobs, stop the running one before its next parse, and
    /// release this rearview's refresh ownership of the session cache: the
    /// running job doesn't write anything more, and a job waiting for
    /// ownership fails instead of acquiring it.
    pub(crate) fn shutdown(&mut self) {
        self.shutdown_releasing_within(SHUTDOWN_RELEASE_WAIT);
    }

    /// [`Self::shutdown`], waiting at most `busy_lock_wait` to release each
    /// ownership while another connection holds SQLite's writer lock.
    fn shutdown_releasing_within(&mut self, busy_lock_wait: Duration) {
        self.worker = None;
        self.cancellation.cancel();
        Coordinator::of(self.environment.cache_base()).shutdown(busy_lock_wait);
    }
}

/// The longest the supervisor's drop waits for a free-space check begun
/// before the shutdown.
const SPACE_CHECK_WAIT: Duration = Duration::from_secs(1);

impl Drop for TaskSupervisor {
    /// Shut the worker down, then wait up to [`SPACE_CHECK_WAIT`] for a
    /// free-space check begun before the shutdown, so the cache base's
    /// low-space latch is final when rearview reads it; a check still
    /// running then can no longer set it. Each release is tried once,
    /// without a wait: the exit tick's shutdown, when one ran, already
    /// waited.
    ///
    /// The running job is not joined, because a parse group can run for
    /// seconds. After a resume or a select, its parse group keeps using CPU
    /// until it ends; it doesn't write anything.
    fn drop(&mut self) {
        self.shutdown_releasing_within(Duration::ZERO);
        self.environment
            .cache_base()
            .low_space()
            .wait_for_checks(SPACE_CHECK_WAIT);
    }
}
