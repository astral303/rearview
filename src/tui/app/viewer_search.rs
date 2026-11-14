//! The viewer's `/` search, on a thread of its own: a search of a large
//! session's rows can take over a second, and the drawing thread keeps
//! reading keys while it runs.

use super::search_generation::{AwaitedSearch, SearchGeneration};
use crate::tui::viewer::{RenderedLine, SearchMatch, find_matches_while};
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// A search of the rows for the query that ends with `None` once
/// `keep_going` returns false: [`find_matches_while`] outside tests.
pub(super) type SearchRows =
    Box<dyn Fn(&[RenderedLine], &str, &dyn Fn() -> bool) -> Option<Vec<SearchMatch>> + Send>;

struct Request {
    rows: Arc<Vec<RenderedLine>>,
    query: String,
    generation: u64,
}

/// The matches a search found, and the generation it ran as.
pub(super) struct Found {
    pub(super) generation: u64,
    pub(super) matches: Vec<SearchMatch>,
}

/// The app's end of the `viewer-search` thread. Dropping it stops the
/// running search at its next check; the thread ends once that search
/// returns, and the rows it searched are freed then.
pub(super) struct ViewerSearchWorker {
    requests: mpsc::Sender<Request>,
    found: mpsc::Receiver<Found>,
    generation: SearchGeneration,
    /// `None` once joined after a panic.
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ViewerSearchWorker {
    pub(super) fn spawn() -> Self {
        Self::spawn_with(Box::new(find_matches_while))
    }

    /// The worker, searching with `search`. It runs one search at a time:
    /// the newest request waiting, while the app still awaits its
    /// generation. A search superseded while it runs stops at its next
    /// check of `keep_going`.
    pub(super) fn spawn_with(search: SearchRows) -> Self {
        let (requests, waiting) = mpsc::channel::<Request>();
        let (found_tx, found) = mpsc::channel();
        let (generation, awaited) = SearchGeneration::start();
        let thread = std::thread::Builder::new()
            .name("viewer-search".into())
            .spawn(move || serve(&waiting, &found_tx, &awaited, &search))
            .expect("failed to spawn viewer search thread");
        Self {
            requests,
            found,
            generation,
            thread: Some(thread),
        }
    }

    /// Search `rows` for `query`, superseding the running search. Returns
    /// the new search's generation.
    pub(super) fn search(&mut self, rows: Arc<Vec<RenderedLine>>, query: String) -> u64 {
        let generation = self.generation.advance();
        // The send fails only once the thread has panicked; the next
        // `newest_found` resumes that panic.
        let _ = self.requests.send(Request {
            rows,
            query,
            generation,
        });
        generation
    }

    /// Stop the running search at its next check. A search past its last
    /// check still sends its matches, under a generation the app no longer
    /// awaits.
    pub(super) fn stop(&self) {
        self.generation.stop_awaiting();
    }

    /// The newest search found, waiting up to `wait` for one. A panic on the
    /// search's thread resumes here.
    pub(super) fn newest_found(&mut self, wait: Duration) -> Option<Found> {
        let first = if wait.is_zero() {
            match self.found.try_recv() {
                Ok(found) => found,
                Err(mpsc::TryRecvError::Empty) => return None,
                Err(mpsc::TryRecvError::Disconnected) => self.resume_thread_panic(),
            }
        } else {
            match self.found.recv_timeout(wait) {
                Ok(found) => found,
                Err(mpsc::RecvTimeoutError::Timeout) => return None,
                Err(mpsc::RecvTimeoutError::Disconnected) => self.resume_thread_panic(),
            }
        };
        Some(self.found.try_iter().last().unwrap_or(first))
    }

    /// The search's thread ended: it ends early only by a panic, which
    /// resumes on this thread.
    fn resume_thread_panic(&mut self) -> ! {
        let thread = self
            .thread
            .take()
            .expect("the viewer's search thread is joined once");
        match thread.join() {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(()) => panic!("the viewer's search thread ended while the app awaited it"),
        }
    }
}

impl Drop for ViewerSearchWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve(
    waiting: &mpsc::Receiver<Request>,
    found: &mpsc::Sender<Found>,
    awaited: &AwaitedSearch,
    search: &SearchRows,
) {
    while let Ok(oldest) = waiting.recv() {
        let request = waiting.try_iter().last().unwrap_or(oldest);
        let keep_going = || awaited.is_awaited(request.generation);
        let Some(matches) = search(&request.rows, &request.query, &keep_going) else {
            continue;
        };
        let sent = found.send(Found {
            generation: request.generation,
            matches,
        });
        if sent.is_err() {
            return;
        }
    }
}
