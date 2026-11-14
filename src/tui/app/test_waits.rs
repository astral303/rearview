//! Waits for the reads that run on threads of their own, and a gate that
//! holds them.

use super::App;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The longest a test waits for a viewer's read of a fixture session before
/// it fails: a watchdog for a read that never lands.
pub(crate) const VIEWER_READ_LANDS: Duration = Duration::from_secs(60);

/// Shows the open session's pending reload, as
/// [`App::complete_pending_view_refresh`] does, waiting up to
/// [`VIEWER_READ_LANDS`] for its read, then for its render. False when no
/// reload is pending; panics when its read doesn't arrive in time, so a slow
/// run fails at the wait and not at a later assertion.
pub(crate) fn finish_pending_view_reload(app: &mut App, viewport_height: usize) -> bool {
    if app.pending_view_reload.is_none() {
        return false;
    }
    assert!(
        app.complete_pending_view_refresh(VIEWER_READ_LANDS),
        "the open session's reload read lands within {VIEWER_READ_LANDS:?}"
    );
    finish_view_render(app, viewport_height);
    true
}

/// Shows the viewer's pending render once it arrives, as the event loop
/// does, failing after [`VIEWER_READ_LANDS`]. Does nothing without a pending
/// render.
pub(crate) fn finish_view_render(app: &mut App, viewport_height: usize) {
    let deadline = std::time::Instant::now() + VIEWER_READ_LANDS;
    while app.is_view_render_pending() {
        assert!(
            std::time::Instant::now() < deadline,
            "the viewer's render arrives within {VIEWER_READ_LANDS:?}"
        );
        app.receive_view_render(viewport_height, Duration::from_millis(10));
    }
}

/// [`App::complete_pending_open`] until the viewer shows the session, or the
/// open fails, failing after [`VIEWER_READ_LANDS`]. True when an open was
/// pending.
pub(crate) fn finish_pending_open(app: &mut App, frame_width: usize) -> bool {
    let deadline = std::time::Instant::now() + VIEWER_READ_LANDS;
    let mut advanced = false;
    while app.complete_pending_open(frame_width, Duration::from_millis(10)) {
        assert!(
            std::time::Instant::now() < deadline,
            "the viewer's read lands"
        );
        advanced = true;
    }
    advanced
}

/// The viewer's pending search to its end, as the event loop runs it: the
/// pause after typing passes on the app's clock, then the matches arrive and
/// are shown, within [`VIEWER_READ_LANDS`]. Does nothing without a pending search.
pub(crate) fn finish_view_search(app: &mut App) {
    if let super::AppMode::View(state) = &app.app_mode
        && let Some(super::PendingViewSearch::Paused { due }) = state.pending_search
    {
        app.clock()
            .sleep(due.saturating_duration_since(app.clock().now()));
    }
    app.send_due_view_search();
    let deadline = std::time::Instant::now() + VIEWER_READ_LANDS;
    while app.is_view_search_pending() {
        assert!(
            std::time::Instant::now() < deadline,
            "the viewer's search returns its matches within {VIEWER_READ_LANDS:?}"
        );
        app.receive_view_search_results(Duration::from_millis(10));
    }
}

/// Holds every session cache connection opened in a cache on a thread
/// `holds` picks, until [`Self::open`].
pub(crate) struct ConnectionGate {
    is_open: Arc<(Mutex<bool>, Condvar)>,
    _hooks: crate::history::sqlite_cache::connection::test_hooks::Installed,
}

impl ConnectionGate {
    pub(crate) fn closed(
        cache: &Path,
        holds: impl Fn(&std::thread::Thread) -> bool + Send + Sync + 'static,
    ) -> Self {
        let is_open = Arc::new((Mutex::new(false), Condvar::new()));
        let gate = Arc::clone(&is_open);
        let _hooks = crate::history::sqlite_cache::connection::test_hooks::on_every_connection(
            cache,
            move |_| {
                if holds(&std::thread::current()) {
                    let (is_open, opened) = &*gate;
                    let guard = is_open.lock().unwrap();
                    drop(opened.wait_while(guard, |is_open| !*is_open).unwrap());
                }
            },
        );
        Self { is_open, _hooks }
    }

    pub(crate) fn open(&self) {
        let (is_open, opened) = &*self.is_open;
        *is_open.lock().unwrap() = true;
        opened.notify_all();
    }
}
