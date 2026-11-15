//! Renders of the open session on the `viewer-render` thread: a render of a
//! large session with every tool output shown can take seconds, and the
//! drawing thread keeps reading keys and drawing while it runs.

use super::request_generation::{AwaitedRequest, RequestGeneration};
use crate::tui::viewer::{
    ParsedConversation, RenderOptions, RenderedConversation, RenderedLine, RowsToRender,
    render_parsed_conversation_while, render_tail_while,
};
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// A render of the conversation's rows that a [`RowsToRender`] selects,
/// ending with `None` once `keep_going` returns false:
/// [`render_rows_while`] outside tests.
pub(super) type RenderRows = Box<
    dyn Fn(
            &ParsedConversation,
            &RenderOptions,
            RowsToRender,
            &dyn Fn() -> bool,
        ) -> Option<RenderedConversation>
        + Send,
>;

/// The rows of `conversation` that `rows_to_render` selects, rendered with
/// `options`: every entry's, or the tail's from its start. `None` once
/// `keep_going` returns false.
pub(super) fn render_rows_while(
    conversation: &ParsedConversation,
    options: &RenderOptions,
    rows_to_render: RowsToRender,
    keep_going: &dyn Fn() -> bool,
) -> Option<RenderedConversation> {
    match rows_to_render {
        RowsToRender::Whole => render_parsed_conversation_while(conversation, options, keep_going),
        RowsToRender::Tail(start) => {
            render_tail_while(conversation, options, start.entry_index, keep_going)
        }
    }
}

struct Request {
    parsed_conversation: Arc<ParsedConversation>,
    options: RenderOptions,
    rows_to_render: RowsToRender,
    generation: u64,
}

/// A finished render, the generation it ran as, the rows it rendered and
/// the options it rendered them with.
pub(super) struct Rendered {
    pub(super) generation: u64,
    pub(super) rows_to_render: RowsToRender,
    pub(super) options: RenderOptions,
    pub(super) rendered_conversation: RenderedConversation,
}

/// The app's end of the `viewer-render` thread. Dropping it stops the
/// running render at its next check, and a finished render waiting to be
/// taken is dropped with the thread; the drop doesn't wait for either.
pub(super) struct ViewerRenderWorker {
    requests: mpsc::Sender<Request>,
    rendered: mpsc::Receiver<Rendered>,
    /// A render taken and kept for the next take, without its rows released.
    kept: Option<Rendered>,
    /// Rows the app no longer shows, one set for each render it took.
    released: mpsc::Sender<Arc<Vec<RenderedLine>>>,
    generation: RequestGeneration,
    /// `None` once joined after a panic.
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ViewerRenderWorker {
    pub(super) fn spawn() -> Self {
        Self::spawn_with(Box::new(render_rows_while))
    }

    /// The worker, rendering with `render`. It runs one render at a time:
    /// the newest request waiting, while the app still awaits its
    /// generation. A render superseded while it runs stops at its next check
    /// of `keep_going`.
    ///
    /// After sending a finished render, the worker waits for the rows the app
    /// no longer shows ([`Self::release`]) and frees them before it starts
    /// the next render. The rows alive are at most the shown ones and one set
    /// built or waiting to be taken.
    pub(super) fn spawn_with(render: RenderRows) -> Self {
        let (requests, waiting) = mpsc::channel::<Request>();
        let (rendered_tx, rendered) = mpsc::channel();
        let (released, to_free) = mpsc::channel();
        let (generation, awaited) = RequestGeneration::start();
        let thread = std::thread::Builder::new()
            .name("viewer-render".into())
            .spawn(move || serve(&waiting, &rendered_tx, &to_free, &awaited, &render))
            .expect("failed to spawn the viewer-render thread");
        Self {
            requests,
            rendered,
            kept: None,
            released,
            generation,
            thread: Some(thread),
        }
    }

    /// Render the rows `rows_to_render` selects of `parsed_conversation` with
    /// `options`, superseding the running render. Returns the new render's
    /// generation.
    pub(super) fn render(
        &mut self,
        parsed_conversation: Arc<ParsedConversation>,
        options: RenderOptions,
        rows_to_render: RowsToRender,
    ) -> u64 {
        let generation = self.generation.advance();
        // The send fails only once the thread has panicked; the next
        // `take_rendered` resumes that panic.
        let _ = self.requests.send(Request {
            parsed_conversation,
            options,
            rows_to_render,
            generation,
        });
        generation
    }

    /// Stop the running render at its next check. A render already waiting
    /// to be taken stays, under a generation the app no longer awaits.
    pub(super) fn stop(&self) {
        self.generation.stop_awaiting();
    }

    /// Free `rows` on the `viewer-render` thread. Call it once for each
    /// render taken, with the rows that render replaced or, when it isn't
    /// shown, its own: the next render starts only after it.
    pub(super) fn release(&self, rows: Arc<Vec<RenderedLine>>) {
        // The send fails only once the thread has panicked; the rows are
        // freed here then.
        let _ = self.released.send(rows);
    }

    /// Keep `rendered`, taken but not shown yet, for the next take. Its rows
    /// are released once that take shows or drops it, and the next render
    /// waits until then.
    pub(super) fn keep_for_next_take(&mut self, rendered: Rendered) {
        self.kept = Some(rendered);
    }

    /// The finished render waiting to be taken, waiting up to `wait` for
    /// one. A panic on the `viewer-render` thread resumes here.
    pub(super) fn take_rendered(&mut self, wait: Duration) -> Option<Rendered> {
        if let Some(kept) = self.kept.take() {
            return Some(kept);
        }
        let rendered = if wait.is_zero() {
            match self.rendered.try_recv() {
                Ok(rendered) => rendered,
                Err(mpsc::TryRecvError::Empty) => return None,
                Err(mpsc::TryRecvError::Disconnected) => self.resume_thread_panic(),
            }
        } else {
            match self.rendered.recv_timeout(wait) {
                Ok(rendered) => rendered,
                Err(mpsc::RecvTimeoutError::Timeout) => return None,
                Err(mpsc::RecvTimeoutError::Disconnected) => self.resume_thread_panic(),
            }
        };
        Some(rendered)
    }

    /// The `viewer-render` thread ended: it ends early only by a panic,
    /// which resumes on this thread.
    fn resume_thread_panic(&mut self) -> ! {
        let thread = self
            .thread
            .take()
            .expect("the viewer-render thread is joined once");
        match thread.join() {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(()) => panic!("the viewer-render thread ended while the app awaited it"),
        }
    }
}

impl Drop for ViewerRenderWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve(
    waiting: &mpsc::Receiver<Request>,
    rendered: &mpsc::Sender<Rendered>,
    to_free: &mpsc::Receiver<Arc<Vec<RenderedLine>>>,
    awaited: &AwaitedRequest,
    render: &RenderRows,
) {
    while let Ok(oldest) = waiting.recv() {
        let request = waiting.try_iter().last().unwrap_or(oldest);
        let keep_going = || awaited.is_awaited(request.generation);
        let Some(rendered_conversation) = render(
            &request.parsed_conversation,
            &request.options,
            request.rows_to_render,
            &keep_going,
        ) else {
            continue;
        };
        let sent = rendered.send(Rendered {
            generation: request.generation,
            rows_to_render: request.rows_to_render,
            options: request.options,
            rendered_conversation,
        });
        if sent.is_err() {
            return;
        }
        let Ok(rows) = to_free.recv() else {
            return;
        };
        drop(rows);
    }
}
