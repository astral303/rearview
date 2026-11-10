//! The list's search worker and the reader of the rows' search texts. The
//! app reaches the worker through [`SearchCommands`] alone: the text
//! reader's replies and the worker's stop arrive on the commands' channel,
//! as inputs only this module builds.

use super::ListSearchMode;
use super::search_generation::{AwaitedSearch, SearchGeneration};
use crate::clock::Clock;
use crate::history::Conversation;
use crate::search::query::ParsedQuery;
use crate::search::{
    self, CopiesToRead, ReadFailure, SearchTextsInMemory, SearchableConversation,
    SessionsWithoutTexts, TextsRead,
};
use chrono::{DateTime, Local};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;

pub(super) enum SearchCommand {
    UpdateData {
        conversations: Arc<Vec<Arc<Conversation>>>,
        searchable: Arc<Vec<SearchableConversation>>,
    },
    Search {
        query: String,
        generation: u64,
        mode: ListSearchMode,
    },
}

/// One input on the search worker's channel.
pub(super) struct WorkerInput(Input);

enum Input {
    Command(SearchCommand),
    /// The reads of one batch of copies, from the text reader.
    TextsRead(Vec<TextsRead>),
    /// The app is gone: the worker stops, whatever reads still run.
    Stop,
}

impl From<SearchCommand> for WorkerInput {
    fn from(command: SearchCommand) -> Self {
        Self(Input::Command(command))
    }
}

#[cfg(test)]
impl WorkerInput {
    pub(super) fn texts_read(reads: Vec<TextsRead>) -> Self {
        Self(Input::TextsRead(reads))
    }

    pub(super) fn into_texts_read(self) -> Option<Vec<TextsRead>> {
        match self.0 {
            Input::TextsRead(reads) => Some(reads),
            Input::Command(_) | Input::Stop => None,
        }
    }
}

/// The app's end of the search worker's channel. Dropping it stops the
/// worker.
pub(super) struct SearchCommands(mpsc::Sender<WorkerInput>);

/// The search worker has ended and did not accept the command.
#[derive(Debug)]
pub(super) struct WorkerGone;

impl SearchCommands {
    /// Hand `command` to the worker. Fails once the worker is gone.
    pub(super) fn send(&self, command: SearchCommand) -> Result<(), WorkerGone> {
        self.0.send(command.into()).map_err(|_| WorkerGone)
    }

    /// Commands for a test to read from the returned receiver, in place of
    /// a worker.
    #[cfg(test)]
    pub(super) fn observed() -> (Self, ObservedCommands) {
        let (input, observed) = mpsc::channel();
        (Self(input), ObservedCommands(observed))
    }
}

impl Drop for SearchCommands {
    fn drop(&mut self) {
        let _ = self.0.send(WorkerInput(Input::Stop));
    }
}

/// The commands sent through [`SearchCommands::observed`].
#[cfg(test)]
pub(super) struct ObservedCommands(mpsc::Receiver<WorkerInput>);

#[cfg(test)]
impl ObservedCommands {
    pub(super) fn try_recv(&self) -> Result<SearchCommand, mpsc::TryRecvError> {
        loop {
            match self.0.try_recv()?.0 {
                Input::Command(command) => return Ok(command),
                Input::TextsRead(_) | Input::Stop => {}
            }
        }
    }
}

pub(super) struct SearchResponse {
    pub(super) filtered: Vec<usize>,
    pub(super) generation: u64,
    pub(super) mode: ListSearchMode,
    pub(super) evidence: HashMap<usize, search::LexicalEvidence>,
    /// True for a semantic query of quoted phrases alone: `filtered` are the
    /// semantic search's results, each an exact match. False for a lexical
    /// search and for the semantic search's lexical fallback.
    pub(super) is_exact_literal: bool,
}

/// A search the worker runs once it has applied every input waiting.
#[derive(Clone)]
pub(super) struct SearchRequest {
    query: String,
    pub(super) generation: u64,
    mode: ListSearchMode,
}

/// What the search worker searches: the rows, their search data, and the
/// search texts read into memory so far.
pub(super) struct SearchWorkerState {
    conversations: Arc<Vec<Arc<Conversation>>>,
    searchable: Arc<Vec<SearchableConversation>>,
    pub(super) texts: SearchTextsInMemory,
    copies_to_read: mpsc::Sender<TextsToRead>,
    /// The worker's own channel. The text reader replies on it.
    texts_reply: mpsc::Sender<WorkerInput>,
    /// The search last run, run again when texts arrive while the app awaits
    /// its generation.
    last_search: Option<SearchRequest>,
    awaited_search: AwaitedSearch,
    /// The `UpdateData` commands applied.
    updates_applied: u64,
}

/// A batch of copies for the text reader, and the channel its reads reply
/// on.
pub(super) struct TextsToRead {
    copies: CopiesToRead,
    reply: mpsc::Sender<WorkerInput>,
}

#[cfg(test)]
impl TextsToRead {
    pub(super) fn new(copies: CopiesToRead, reply: mpsc::Sender<WorkerInput>) -> Self {
        Self { copies, reply }
    }
}

/// The sessions without texts held, as the worker counts them once it has
/// applied `updates_applied` `UpdateData` commands.
#[derive(Clone, Copy)]
pub(super) struct TextsHeld {
    pub(super) updates_applied: u64,
    pub(super) sessions_without_texts: SessionsWithoutTexts,
}

/// The work the inputs waiting ask the worker for.
#[derive(Default)]
pub(super) struct PendingWork {
    pub(super) search: Option<SearchRequest>,
    /// True when the count of sessions without texts may have changed.
    texts_changed: bool,
    /// True once the app is gone.
    stop: bool,
}

impl SearchWorkerState {
    /// No rows yet; the copies whose texts are to be read go to
    /// `copies_to_read`, and their reads reply on `texts_reply`.
    pub(super) fn new(
        copies_to_read: mpsc::Sender<TextsToRead>,
        texts_reply: mpsc::Sender<WorkerInput>,
        awaited_search: AwaitedSearch,
    ) -> Self {
        Self {
            conversations: Arc::new(Vec::new()),
            searchable: Arc::new(Vec::new()),
            texts: SearchTextsInMemory::default(),
            copies_to_read,
            texts_reply,
            last_search: None,
            awaited_search,
            updates_applied: 0,
        }
    }

    /// Applies `input`: new rows and texts read change the worker's state
    /// at once, and `pending` collects the search to run and whether to
    /// stop. Texts read run the last search again over them unless a newer
    /// one waits, while the app awaits it.
    pub(super) fn apply(&mut self, input: WorkerInput, pending: &mut PendingWork) {
        match input.0 {
            Input::Command(SearchCommand::UpdateData {
                conversations,
                searchable,
            }) => {
                let copies = self.texts.keep_copies_of(&conversations);
                if !copies.copies.is_empty() {
                    self.send_to_reader(TextsToRead {
                        copies,
                        reply: self.texts_reply.clone(),
                    });
                }
                self.conversations = conversations;
                self.searchable = searchable;
                self.updates_applied += 1;
                pending.texts_changed = true;
            }
            Input::Command(SearchCommand::Search {
                query,
                generation,
                mode,
            }) => {
                pending.search = Some(SearchRequest {
                    query,
                    generation,
                    mode,
                });
            }
            Input::TextsRead(reads) => {
                for (copy, read) in reads {
                    self.texts.finish_read(copy, read);
                }
                pending.texts_changed = true;
                if pending.search.is_none() {
                    pending.search = self
                        .last_search
                        .clone()
                        .filter(|last| self.awaited_search.is_awaited(last.generation));
                }
            }
            Input::Stop => pending.stop = true,
        }
    }

    /// Sends `batch` to the text reader. When the reader has stopped, the
    /// batch's copies are recorded as failed reads instead of staying
    /// marked as being read.
    fn send_to_reader(&mut self, batch: TextsToRead) {
        if let Err(mpsc::SendError(unsent)) = self.copies_to_read.send(batch) {
            let failure = ReadFailure::Error("the search text reader stopped".into());
            for (copy, read) in unsent.copies.failed(&failure) {
                self.texts.finish_read(copy, read);
            }
        }
    }

    pub(super) fn search(
        &mut self,
        request: SearchRequest,
        now: DateTime<Local>,
    ) -> SearchResponse {
        let conversations = self.conversations.as_slice();
        let filtered = search::search_with_texts(
            conversations,
            &self.searchable,
            &self.texts,
            &request.query,
            now,
        );
        let parsed = ParsedQuery::parse(&request.query);
        let evidence =
            search::build_session_list_evidence(conversations, &self.texts, &filtered, &parsed);
        let response = SearchResponse {
            filtered,
            generation: request.generation,
            mode: request.mode,
            evidence,
            is_exact_literal: request.mode == ListSearchMode::Semantic && parsed.is_quoted_only(),
        };
        self.last_search = Some(request);
        response
    }
}

/// Start the search worker, ranking freshness against `clock`'s time. The
/// rows' search texts are read into memory on a reader thread of its own;
/// a session's search reads its title, summary and project name alone until
/// its texts arrive, and the last search runs again when they do.
pub(super) fn spawn_search_worker(clock: Arc<dyn Clock>) -> SearchWorker {
    let (input_tx, input_rx) = mpsc::channel::<WorkerInput>();
    let (res_tx, res_rx) = mpsc::channel::<SearchResponse>();
    let (texts_held_tx, texts_held_rx) = mpsc::channel::<TextsHeld>();
    let (generation, awaited_search) = SearchGeneration::start();
    let texts_reply = input_tx.clone();

    std::thread::Builder::new()
        .name("search-worker".into())
        .spawn(move || {
            let mut state = SearchWorkerState::new(
                spawn_text_reader(CopiesToRead::read),
                texts_reply,
                awaited_search,
            );
            while let Ok(input) = input_rx.recv() {
                let mut pending = PendingWork::default();
                state.apply(input, &mut pending);
                while let Ok(waiting) = input_rx.try_recv() {
                    state.apply(waiting, &mut pending);
                }
                if pending.stop {
                    return;
                }
                // Send the results first: once the app's count of sessions
                // loading reaches zero, the results over every session's
                // texts are on their channel.
                if let Some(request) = pending.search {
                    let now = clock.wall().with_timezone(&Local);
                    if res_tx.send(state.search(request, now)).is_err() {
                        return;
                    }
                }
                let held = TextsHeld {
                    updates_applied: state.updates_applied,
                    sessions_without_texts: state.texts.sessions_without_texts(),
                };
                if pending.texts_changed && texts_held_tx.send(held).is_err() {
                    return;
                }
            }
        })
        .expect("failed to spawn search worker thread");

    SearchWorker {
        commands: SearchCommands(input_tx),
        results: res_rx,
        texts_held: texts_held_rx,
        generation,
    }
}

/// The app's ends of the search worker's channels.
pub(super) struct SearchWorker {
    pub(super) commands: SearchCommands,
    pub(super) results: mpsc::Receiver<SearchResponse>,
    /// The sessions without texts held, whenever the count may have changed.
    pub(super) texts_held: mpsc::Receiver<TextsHeld>,
    /// The search generation, whose awaited search the worker reads.
    pub(super) generation: SearchGeneration,
}

/// Start the thread reading the search texts of each batch of copies sent
/// to it with `read`, in parallel, and replying with each batch's reads
/// together. A batch whose read panics replies with every copy failed. The
/// thread stops once the search worker is gone.
pub(super) fn spawn_text_reader(
    read: fn(&CopiesToRead) -> Vec<TextsRead>,
) -> mpsc::Sender<TextsToRead> {
    let (batches_tx, batches_rx) = mpsc::channel::<TextsToRead>();
    std::thread::Builder::new()
        .name("search-text-reader".into())
        .spawn(move || {
            while let Ok(TextsToRead { copies, reply }) = batches_rx.recv() {
                let reads =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read(&copies)))
                        .unwrap_or_else(|panic| {
                            let message = panic
                                .downcast_ref::<String>()
                                .map(String::as_str)
                                .or_else(|| panic.downcast_ref::<&str>().copied())
                                .unwrap_or("a panic without a message");
                            let cause = format!("the search text reader panicked: {message}");
                            copies.failed(&ReadFailure::Error(cause.into()))
                        });
                let _ = reply.send(WorkerInput(Input::TextsRead(reads)));
            }
        })
        .expect("failed to spawn search text reader thread");
    batches_tx
}
