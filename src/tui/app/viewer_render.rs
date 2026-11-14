//! Renders of the open session on a thread of their own: a render of a large
//! session with every tool output shown can take seconds, and the drawing
//! thread keeps reading keys and drawing while it runs.

use super::request_generation::{AwaitedRequest, RequestGeneration};
use crate::tui::viewer::{
    ParsedConversation, RenderOptions, RenderedConversation, render_parsed_conversation_while,
};
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// A render of the conversation that ends with `None` once `keep_going`
/// returns false: [`render_parsed_conversation_while`] outside tests.
pub(super) type RenderRows = Box<
    dyn Fn(&ParsedConversation, &RenderOptions, &dyn Fn() -> bool) -> Option<RenderedConversation>
        + Send,
>;

struct Request {
    conversation: Arc<ParsedConversation>,
    options: RenderOptions,
    generation: u64,
}

/// A finished render, and the generation it ran as.
pub(super) struct Rendered {
    pub(super) generation: u64,
    pub(super) conversation: RenderedConversation,
}

/// The app's end of the `viewer-render` thread. Dropping it stops the
/// running render at its next check, and a finished render waiting to be
/// taken is dropped with the thread; the drop doesn't wait for either.
pub(super) struct ViewerRenderWorker {
    requests: mpsc::Sender<Request>,
    rendered: mpsc::Receiver<Rendered>,
    generation: RequestGeneration,
    /// `None` once joined after a panic.
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ViewerRenderWorker {
    pub(super) fn spawn() -> Self {
        Self::spawn_with(Box::new(render_parsed_conversation_while))
    }

    /// The worker, rendering with `render`. It runs one render at a time:
    /// the newest request waiting, while the app still awaits its
    /// generation. A render superseded while it runs stops at its next check
    /// of `keep_going`.
    ///
    /// A finished render waits until the app takes it before the next
    /// starts, so the rows alive are at most the shown ones and one set
    /// built or waiting.
    pub(super) fn spawn_with(render: RenderRows) -> Self {
        let (requests, waiting) = mpsc::channel::<Request>();
        let (rendered_tx, rendered) = mpsc::sync_channel(0);
        let (generation, awaited) = RequestGeneration::start();
        let thread = std::thread::Builder::new()
            .name("viewer-render".into())
            .spawn(move || serve(&waiting, &rendered_tx, &awaited, &render))
            .expect("failed to spawn viewer render thread");
        Self {
            requests,
            rendered,
            generation,
            thread: Some(thread),
        }
    }

    /// Render `conversation` with `options`, superseding the running render.
    /// Returns the new render's generation.
    pub(super) fn render(
        &mut self,
        conversation: Arc<ParsedConversation>,
        options: RenderOptions,
    ) -> u64 {
        let generation = self.generation.advance();
        // The send fails only once the thread has panicked; the next
        // `take_rendered` resumes that panic.
        let _ = self.requests.send(Request {
            conversation,
            options,
            generation,
        });
        generation
    }

    /// Stop the running render at its next check. A render already waiting
    /// to be taken stays, under a generation the app no longer awaits.
    pub(super) fn stop(&self) {
        self.generation.stop_awaiting();
    }

    /// The finished render waiting to be taken, waiting up to `wait` for
    /// one. A panic on the render's thread resumes here.
    pub(super) fn take_rendered(&mut self, wait: Duration) -> Option<Rendered> {
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

    /// The render's thread ended: it ends early only by a panic, which
    /// resumes on this thread.
    fn resume_thread_panic(&mut self) -> ! {
        let thread = self
            .thread
            .take()
            .expect("the viewer's render thread is joined once");
        match thread.join() {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(()) => panic!("the viewer's render thread ended while the app awaited it"),
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
    rendered: &mpsc::SyncSender<Rendered>,
    awaited: &AwaitedRequest,
    render: &RenderRows,
) {
    while let Ok(oldest) = waiting.recv() {
        let request = waiting.try_iter().last().unwrap_or(oldest);
        let keep_going = || awaited.is_awaited(request.generation);
        let Some(conversation) = render(&request.conversation, &request.options, &keep_going)
        else {
            continue;
        };
        if !keep_going() {
            continue;
        }
        let sent = rendered.send(Rendered {
            generation: request.generation,
            conversation,
        });
        if sent.is_err() {
            return;
        }
    }
}
