//! The TUI's one background worker: the launch, list `Ctrl+R` and session
//! refreshes run on it one at a time, and their messages come back to the
//! runtime through the supervisor.

use crate::error::{AppError, Result};
use crate::history::load_meter::LoadMeter;
use crate::history::sqlite_cache::Coordinator;
use crate::history::{
    Environment, LoaderMessage, OperationScope, OtherRearview, Patience, RefreshOptions,
    RefreshOutcome, WaitStatus, stream_all_conversations,
};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

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
    /// The wait a list or session refresh publishes.
    refresh_wait: Arc<WaitStatus>,
}

impl TaskSupervisor {
    /// The worker for jobs on `environment`'s sessions. A launch records
    /// what it reads in `load_meter`.
    pub(crate) fn start(environment: Environment, load_meter: Arc<LoadMeter>) -> Self {
        let (worker, jobs) = mpsc::channel::<Job>();
        std::thread::spawn(move || {
            for job in jobs {
                // A job that panics drops its channel, which the runtime reads
                // as a job that stopped; the next job still runs.
                let _ = catch_unwind(AssertUnwindSafe(job));
            }
        });
        Self {
            environment,
            worker: Some(worker),
            launch: None,
            load_meter,
            refresh: None,
            refresh_wait: Arc::default(),
        }
    }

    /// True while the worker runs nothing and is not shut down.
    pub(crate) fn can_take_job(&self) -> bool {
        self.worker.is_some() && !self.is_running_a_job()
    }

    /// True while a job runs.
    pub(crate) fn is_running_a_job(&self) -> bool {
        self.launch.is_some() || self.refresh.is_some()
    }

    /// Run the launch, reading sessions with `options`. Its messages come
    /// through [`Self::next_launch_message`].
    pub(crate) fn launch(&mut self, options: RefreshOptions) {
        let (messages, received) = mpsc::channel();
        let environment = self.environment.clone();
        let meter = Arc::clone(&self.load_meter);
        self.launch = Some(received);
        self.run(Box::new(move || {
            stream_all_conversations(&messages, &environment, options, &meter);
        }));
    }

    /// Run `refresh` as one operation on the session cache that waits for
    /// refresh ownership as long as another rearview keeps it. Its outcome
    /// comes through [`Self::finished_refresh`], failed as
    /// [`OperationScope::finish`] fails.
    pub(crate) fn refresh(
        &mut self,
        refresh: impl FnOnce(&Environment, &OperationScope) -> Result<RefreshOutcome> + Send + 'static,
    ) {
        let (outcome, received) = mpsc::channel();
        let environment = self.environment.clone();
        let wait = Arc::clone(&self.refresh_wait);
        self.refresh = Some(received);
        self.run(Box::new(move || {
            let operation =
                OperationScope::new(environment.cache_base(), Patience::UntilAcquired, wait);
            let refreshed = refresh(&environment, &operation);
            let _ = outcome.send(operation.finish().and(refreshed));
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

    /// The other rearview the running job waits for; `None` while it does
    /// not wait or no job runs.
    pub(crate) fn waiting_for(&self) -> Option<OtherRearview> {
        if self.launch.is_some() {
            self.load_meter.waiting_for()
        } else if self.refresh.is_some() {
            self.refresh_wait.current()
        } else {
            None
        }
    }

    /// Take no more jobs, and give up this rearview's refresh ownership of
    /// the session cache: the running job writes nothing more, and a job
    /// waiting for ownership fails instead of acquiring it.
    pub(crate) fn shutdown(&mut self) {
        self.worker = None;
        if let Some(base) = self.environment.cache_base() {
            Coordinator::of(base).shutdown();
        }
    }
}
