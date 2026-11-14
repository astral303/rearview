use super::session_store::{FinishedRefresh, ReadCause};
use super::{
    App, AppMode, CountHighlight, DialogMode, Focus, HeaderRow, MatchesArrival, OpenSessionStatus,
    PendingRender, PendingViewSearch, RowsRead, ViewSearchMode, ViewSpot, ViewState,
};
use crate::history::provider::{RenamedTitle, SessionRead, SessionTitle, TitleAndSummary};
use crate::history::sqlite_cache::{AdoptedCopy, Publication, SessionAddress};
use crate::history::{Conversation, InteractiveCache, InteractiveRead, Source};
use crate::tui::ui;
use crate::tui::viewer::{
    CallArea, CallRange, EntryInPlace, EntryRelocation, MessageRange, ParsedConversation,
    RenderOptions, RenderedConversation, RenderedLine, SearchMatch, ShownRows, ToolOutputId,
    ViewedConversation, entry_rendering_alone, parse_conversation_file_interactively,
    render_entry_in_place, update_matches_in_replaced_rows,
};
use ratatui::prelude::*;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// The pause after the last edit of the viewer's query before its search is
/// sent. The keys of a paste arrive less than the pause apart and run one
/// search.
pub(crate) const VIEW_SEARCH_PAUSE: Duration = Duration::from_millis(100);

/// The open session's row as its finished refresh read it, for the viewer's
/// read of the session from the cache.
pub(super) struct ReloadedSession {
    publication: Publication,
    session_id: Option<String>,
    /// The header of the row the refresh listed, or of the row a file outside
    /// every provider's session roots parsed into.
    row: Option<HeaderRow>,
    /// The title stored beside the transcript, as the listed row shows it.
    external_title: Option<SessionTitle>,
    /// The row's address in the cache; `None` for a file outside every
    /// provider's session roots.
    address: Option<SessionAddress>,
    /// The conversation a file outside every provider's session roots parsed
    /// into; `None` for a session the viewer reads from the cache.
    parsed: Option<Arc<ParsedConversation>>,
}

impl ReloadedSession {
    /// The file opened directly, outside every provider's session roots, as
    /// the background worker parsed it into `row`.
    pub(super) fn parsed(conversation: ParsedConversation, row: Option<&Conversation>) -> Self {
        Self {
            publication: Publication::Unpublished {
                discovered: Vec::new(),
            },
            session_id: row.map(|row| row.session_id.clone()),
            row: row.map(HeaderRow::of),
            external_title: None,
            address: None,
            parsed: Some(Arc::new(conversation)),
        }
    }
}

impl ViewState {
    /// Replace the view's publication with `reloaded`'s, or with the
    /// publication of the copy the read adopted in the row's place. The
    /// header shows `copy_row`, the catalogue row of the copy the view
    /// shows, or `reloaded`'s row while the view doesn't show a copy.
    fn show_reloaded_row(&mut self, reloaded: ReloadedHeader, copy_row: Option<Box<Conversation>>) {
        if let Some(row) = reloaded.row {
            self.header = Some(row);
        }
        self.publication = reloaded.publication;
        self.show_copy_header_and_adopt(
            copy_row,
            reloaded.external_title.as_ref(),
            reloaded.adopted,
        );
    }

    /// Show `copy_row`, the catalogue row of the copy the view shows, as the
    /// header, and read the open session's copies against `adopted`'s
    /// publication. Without a row, the header stays as it is.
    ///
    /// The header keeps the row's title and summary: a rename in rearview
    /// sets them before the copy has the new name. A copy the read adopted
    /// in the row's place names the session instead, with `external_title`,
    /// the title the row shows from beside the transcript, over its parse's
    /// title.
    fn show_copy_header_and_adopt(
        &mut self,
        copy_row: Option<Box<Conversation>>,
        external_title: Option<&SessionTitle>,
        adopted: Option<AdoptedCopy>,
    ) {
        let is_adopted = adopted.is_some();
        if let Some(adopted) = adopted {
            self.publication = adopted.publication;
        }
        let Some(copy_row) = copy_row else {
            return;
        };
        let mut header = HeaderRow::of_copy(*copy_row, external_title);
        if !is_adopted && let Some(row) = &self.header {
            header.custom_title = row.custom_title.clone();
            header.summary = row.summary.clone();
        }
        self.header = Some(header);
    }

    /// Show the open session renamed to `title`, as its row shows it.
    /// `renamed` is the place the rename put the title.
    pub(super) fn show_renamed(&mut self, title: &str, renamed: &RenamedTitle) {
        if let Some(header) = &mut self.header {
            let shown = renamed.shown(
                title,
                &TitleAndSummary {
                    custom_title: header.custom_title.take(),
                    summary: header.summary.take(),
                },
            );
            header.custom_title = shown.custom_title;
            header.summary = shown.summary;
        }
    }

    /// Show `title`, the title kept beside the open session's transcript, in
    /// the header, as [`TitleAndSummary::shown`] decides.
    fn show_retitled(&mut self, title: &SessionTitle) {
        if let Some(header) = &mut self.header {
            let shown = TitleAndSummary::shown(&TitleAndSummary::default(), Some(title));
            header.custom_title = shown.custom_title;
            header.summary = shown.summary;
        }
    }
}

/// The open session's refreshed row once its refresh is read again, with its
/// `publication`, and `adopted`, the copy the read adopted in its place.
struct ReloadedHeader {
    row: Option<HeaderRow>,
    external_title: Option<SessionTitle>,
    publication: Publication,
    adopted: Option<AdoptedCopy>,
}

/// A copy or export asked for while the open session's refresh was pending,
/// or an export asked for while its copy was out of date or missing.
pub(super) enum DeferredCopy {
    /// The export menu's `option`, to the clipboard or to a file.
    Export { option: usize, to_clipboard: bool },
    /// The focused message or call, in a viewport `viewport_height` rows high.
    Focused { viewport_height: usize },
}

/// A row the viewer opens, as the list held it when the open began.
pub(super) struct OpenedRow {
    path: PathBuf,
    source: Source,
    session_id: String,
    publication: Publication,
    cache_address: Option<SessionAddress>,
    external_title: Option<SessionTitle>,
    header: HeaderRow,
}

impl OpenedRow {
    pub(super) fn of(row: &Conversation) -> Self {
        Self {
            path: row.path.clone(),
            source: row.source,
            session_id: row.session_id.clone(),
            publication: row.publication.clone(),
            cache_address: row.cache_address.clone(),
            external_title: row.external_title.clone(),
            header: HeaderRow::of(row),
        }
    }

    /// The viewer's read of the row's copy in `cache`; `None` for a row
    /// without an address in the cache.
    fn read(&self, cache: &InteractiveCache) -> Option<InteractiveRead<ViewedConversation>> {
        let address = self.cache_address.as_ref()?;
        Some(parse_conversation_file_interactively(
            cache,
            address,
            &self.publication,
        ))
    }
}

/// A read of the session cache for the viewer, running on a thread of its
/// own so the drawing thread keeps drawing while it runs.
pub(super) struct ViewerRead<T> {
    read: mpsc::Receiver<T>,
    thread: std::thread::JoinHandle<()>,
}

/// A row the viewer opens, with the viewer's read of its copy.
pub(super) type RowRead = (OpenedRow, Option<InteractiveRead<ViewedConversation>>);

/// The open session's row as its finished refresh read it, with the viewer's
/// read of its copy; `None` for a row without an address in the cache.
pub(super) type ReloadRead = (ReloadedSession, Option<InteractiveRead<ViewedConversation>>);

impl ViewerRead<RowRead> {
    /// The viewer's read of `row`'s copy in `cache`.
    pub(super) fn of_row(row: OpenedRow, cache: InteractiveCache) -> Self {
        Self::start(move || {
            let read = row.read(&cache);
            (row, read)
        })
    }
}

impl<T: Send + 'static> ViewerRead<T> {
    fn start(read: impl FnOnce() -> T + Send + 'static) -> Self {
        let (send, receive) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("viewer-read".into())
            .spawn(move || {
                let _ = send.send(read());
            })
            .expect("failed to spawn the viewer's read thread");
        Self {
            read: receive,
            thread,
        }
    }

    /// The read, once it lands within `wait`; `Err(self)` while it runs. A
    /// panic on the read's thread resumes here.
    pub(super) fn finish(self, wait: Duration) -> Result<T, Self> {
        match self.read.recv_timeout(wait) {
            Ok(read) => Ok(read),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(self),
            Err(mpsc::RecvTimeoutError::Disconnected) => match self.thread.join() {
                Err(panic) => std::panic::resume_unwind(panic),
                Ok(()) => panic!("the viewer's read thread ended without its read"),
            },
        }
    }
}

/// The open session's reload, once its refresh finished: the viewer shows it
/// on the next frame, or once the read of its copy arrives. One reload runs
/// at a time, from its read until its rows show: a newer read would stop the
/// render of a reload's rows.
pub(super) enum PendingReload {
    /// A refresh's reload whose read starts once the previous reload's rows
    /// show ([`App::start_queued_reload`]).
    Queued(Box<ReloadedSession>),
    /// A file opened directly, already parsed, or a row without an address in
    /// the cache.
    Ready(Box<ReloadedSession>),
    /// At most one read runs: a newer refresh finishing meanwhile is
    /// `queued`, and its read starts once this one's rows show.
    Reading {
        read: ViewerRead<ReloadRead>,
        queued: Option<Box<ReloadedSession>>,
    },
}

/// A reload whose read arrived, and the newer refresh's row to read once its
/// rows show.
pub(super) type LandedReload = (ReloadRead, Option<Box<ReloadedSession>>);

impl PendingReload {
    /// `reloaded`, reading its copy in `cache` when it has one.
    fn start(reloaded: Box<ReloadedSession>, cache: InteractiveCache) -> Self {
        if reloaded.parsed.is_some() {
            return Self::Ready(reloaded);
        }
        let Some(address) = reloaded.address.clone() else {
            return Self::Ready(reloaded);
        };
        Self::Reading {
            read: ViewerRead::start(move || {
                let read =
                    parse_conversation_file_interactively(&cache, &address, &reloaded.publication);
                (*reloaded, Some(read))
            }),
            queued: None,
        }
    }

    /// This reload followed by `newer`, a later refresh's: a read running
    /// keeps running with `newer` queued; otherwise `newer` replaces it.
    fn followed_by(self, newer: Box<ReloadedSession>) -> Self {
        match self {
            Self::Queued(_) | Self::Ready(_) => Self::Queued(newer),
            Self::Reading { read, .. } => Self::Reading {
                read,
                queued: Some(newer),
            },
        }
    }

    /// The reload and the row queued behind it, once its read arrives within
    /// `wait`; `Err(self)` while it runs or is queued.
    pub(super) fn finish(self, wait: Duration) -> Result<LandedReload, Self> {
        match self {
            Self::Queued(_) => Err(self),
            Self::Ready(reloaded) => Ok(((*reloaded, None), None)),
            Self::Reading { read, queued } => match read.finish(wait) {
                Ok(read) => Ok((read, queued)),
                Err(read) => Err(Self::Reading { read, queued }),
            },
        }
    }

    pub(super) fn is_reading(&self) -> bool {
        matches!(self, Self::Reading { .. })
    }
}

/// A finished refresh's read of the open session.
enum OpenSessionRead {
    Listed(Box<ReloadedSession>),
    /// Gone or empty, with the status that names which.
    Gone(&'static str),
    /// The read or the whole refresh failed, with the status to show.
    Failed(String),
    /// A list refresh read the title kept beside the transcript without
    /// reading the session again. The list's row shows the title once the
    /// list applies the refresh.
    Retitled(SessionTitle),
}

impl OpenSessionRead {
    /// `finished`'s read of the session `state` shows; `None` when it read
    /// neither that session nor its title.
    fn in_finished(finished: &FinishedRefresh, state: &ViewState) -> Option<Self> {
        let path = state.conversation_path.as_path();
        match finished {
            FinishedRefresh::Failed(error) => Some(Self::refresh_failed(error)),
            FinishedRefresh::Reads(reads) => {
                let read = reads.iter().find(|read| read.path == path)?;
                if let Some(failure) = &read.write_failure {
                    return Some(Self::refresh_failed(&failure.error));
                }
                Some(match &read.read {
                    SessionRead::Listed(row) => Self::listed(row),
                    SessionRead::Missing => Self::Gone("Session not found"),
                    SessionRead::Empty(_) => Self::Gone("Session is empty"),
                    SessionRead::Failed(..) | SessionRead::NotCached => Self::read_failed(),
                })
            }
            FinishedRefresh::Changes(changes) => {
                if let Some(updated) = changes
                    .updated
                    .iter()
                    .find(|updated| updated.row.path == path)
                {
                    return Some(match updated.next.write_error() {
                        Some(error) => Self::refresh_failed(error),
                        None => Self::listed(&updated.row),
                    });
                }
                if changes.removed.iter().any(|removed| removed == path) {
                    return Some(Self::Gone("Session not found"));
                }
                if let Some(kept) = changes.kept.iter().find(|kept| kept.path == path) {
                    return Some(match kept.next.write_error() {
                        Some(error) => Self::refresh_failed(error),
                        None => Self::read_failed(),
                    });
                }
                let (Some(source), Some(session_id)) =
                    (state.conversation_source, &state.session_id)
                else {
                    return None;
                };
                changes
                    .external_title_of(source, session_id)
                    .map(|title| Self::Retitled(title.clone()))
            }
        }
    }

    /// A refresh that failed with `error`, as a whole or at its write to the
    /// cache: the viewer keeps the copy it shows.
    fn refresh_failed(error: impl std::fmt::Display) -> Self {
        Self::Failed(format!("Refresh failed: {error}"))
    }

    fn listed(row: &Conversation) -> Self {
        Self::Listed(Box::new(ReloadedSession {
            publication: row.publication.clone(),
            session_id: Some(row.session_id.clone()),
            row: Some(HeaderRow::of(row)),
            external_title: row.external_title.clone(),
            address: row.cache_address.clone(),
            parsed: None,
        }))
    }

    fn read_failed() -> Self {
        Self::Failed(SESSION_READ_FAILED.to_owned())
    }
}

/// The status of a refresh that could not read the open session.
pub(super) const SESSION_READ_FAILED: &str = "Refresh failed: the session could not be read";

impl App {
    /// Open the selected session at once; the event loop defers through
    /// `request_open` and `complete_pending_open` instead.
    #[cfg(test)]
    pub fn enter_view_mode(&mut self, frame_width: usize) {
        let Some(selected) = self.selected else {
            return;
        };
        let Some(&conv_idx) = self.filtered.get(selected) else {
            return;
        };
        let row = OpenedRow::of(&self.conversations[conv_idx]);
        let read = row.read(&self.interactive_cache());
        self.show_opened(row, read, frame_width);
    }

    /// Show `row` in the viewer, from `read`, the viewer's read of its copy.
    pub(super) fn show_opened(
        &mut self,
        row: OpenedRow,
        read: Option<InteractiveRead<ViewedConversation>>,
        frame_width: usize,
    ) {
        use crate::tui::viewer::{content_width, render_parsed_conversation};

        let OpenedRow {
            path,
            source,
            session_id,
            publication,
            cache_address,
            external_title,
            header,
        } = row;
        let (copy, adopted) = match read {
            Some(read) => self
                .take_interactive(&path, read, ReadCause::UserAction)
                .current_or_older_with_adopted(),
            None => (None, None),
        };
        let is_copy_missing = copy.is_none();
        let (conversation, copy_row) = match copy {
            Some(copy) => (copy.conversation, copy.row),
            None => (ParsedConversation::empty(), None),
        };
        let conversation = Arc::new(conversation);
        let options = RenderOptions {
            tool_display: self.tool_display,
            show_thinking: self.show_thinking,
            show_timing: self.show_timing,
            content_width: content_width(frame_width, self.show_timing),
            expanded_tool_outputs: BTreeSet::new(),
            can_expand: true,
            today: self.today(),
            zone: self.clock().zone(),
        };
        let rendered = render_parsed_conversation(&conversation, &options);
        let total_lines = rendered.lines.len();
        let first_msg = (!rendered.messages.is_empty()).then_some(Focus {
            message_index: 0,
            call_index: None,
        });
        let mut state = ViewState {
            conversation_path: path,
            conversation_source: Some(source),
            cache_address,
            session_id: Some(session_id),
            publication,
            header: Some(header),
            parsed_conversation: conversation,
            scroll_offset: 0,
            rendered_lines: Arc::new(rendered.lines),
            total_lines,
            tool_display: self.tool_display,
            show_thinking: self.show_thinking,
            show_timing: self.show_timing,
            frame_width,
            search_mode: ViewSearchMode::Off,
            search_query: String::new(),
            search_matches: Vec::new(),
            current_match_index: 0,
            pending_search: None,
            message_ranges: rendered.messages,
            call_ranges: rendered.calls,
            detail_calls: rendered.detail_calls,
            focus: first_msg,
            message_nav_active: false,
            expanded_tool_outputs: BTreeSet::new(),
            hovered_tool_output: None,
            count_highlight: None,
            is_copy_missing,
            rows_read: RowsRead::Current,
            pending_render: None,
            entries_to_render_again: BTreeSet::new(),
            updating_since: None,
            reload_rows_shown_at: None,
        };
        state.show_copy_header_and_adopt(copy_row, external_title.as_ref(), adopted);
        self.app_mode = AppMode::View(state);
    }

    /// Ask for the open session to be read again (`Ctrl+R`): through a
    /// session refresh, or, for a file opened directly outside every
    /// provider's session roots, through the background worker's read of it.
    pub(super) fn request_open_session_refresh(&mut self) {
        let AppMode::View(state) = &self.app_mode else {
            return;
        };
        if self.store.refuses_session_refreshes() {
            self.direct_file_read
                .get_or_insert(super::DirectFileJob::Requested);
            return;
        }
        let path = state.conversation_path.clone();
        self.store.request_session_refresh(&path);
    }

    /// The open session's path as path display (`p`) and path copy (`Y`)
    /// show it: a file opened directly as the user typed it.
    pub(super) fn open_session_path_text(&self) -> Option<String> {
        let AppMode::View(state) = &self.app_mode else {
            return None;
        };
        let path = self
            .opened_file_typed_path
            .as_ref()
            .unwrap_or(&state.conversation_path);
        Some(path.display().to_string())
    }

    /// True while the open session's refresh waits, runs or awaits another
    /// refresh, until the viewer shows its reload's rows, and while the
    /// background worker reads the file opened directly.
    pub fn is_open_session_updating(&self) -> bool {
        match &self.app_mode {
            AppMode::View(state) => {
                self.direct_file_read.is_some() || self.is_reload_pending(state)
            }
            AppMode::List => false,
        }
    }

    /// True while the open session's refresh waits, runs or awaits another
    /// refresh, until the viewer shows its reload's rows.
    fn is_reload_pending(&self, state: &ViewState) -> bool {
        self.store.is_refresh_pending(&state.conversation_path)
            || self.pending_view_reload.is_some()
            || state.awaits_reload_rows()
    }

    /// The open session's pending refresh as the viewer labels it; `None`
    /// while no refresh of it is pending. A refresh awaiting another
    /// rearview's refresh, or running while its job waits for another
    /// rearview, is that rearview's activity.
    pub fn open_session_status(&self) -> Option<OpenSessionStatus> {
        let AppMode::View(state) = &self.app_mode else {
            return None;
        };
        if !self.is_open_session_updating() {
            return None;
        }
        let path = &state.conversation_path;
        let another_rearview = self.store.another_rearview_awaited_by(path).or_else(|| {
            self.cache_wait
                .filter(|_| self.store.is_in_running_session_refresh(path))
        });
        Some(match another_rearview {
            Some(activity) => OpenSessionStatus::AnotherRearview(activity),
            None if state.is_copy_missing => OpenSessionStatus::Loading,
            None => OpenSessionStatus::Updating,
        })
    }

    /// When the newest finished refresh read the open session: queue reading
    /// it again from the cache ([`Self::queue_open_session_reload`]); leave
    /// the viewer for a session that is gone or empty, or report the failed
    /// read.
    /// The list applies the refresh itself once the viewer closes. A file
    /// opened directly keeps the viewer when it is gone or empty.
    pub(super) fn follow_open_session(&mut self) {
        let AppMode::View(state) = &self.app_mode else {
            return;
        };
        let read = self
            .store
            .newest_finished()
            .and_then(|finished| OpenSessionRead::in_finished(finished, state));
        match read {
            Some(OpenSessionRead::Listed(reloaded)) => self.queue_open_session_reload(reloaded),
            Some(OpenSessionRead::Gone(status)) => {
                self.deferred_copy = None;
                if !self.single_file_mode {
                    self.exit_view_mode();
                }
                self.set_status(status.to_owned());
            }
            Some(OpenSessionRead::Failed(status)) => {
                self.set_status(status);
                self.run_deferred_copy();
            }
            Some(OpenSessionRead::Retitled(title)) => {
                if let AppMode::View(state) = &mut self.app_mode {
                    state.show_retitled(&title);
                }
            }
            None => {}
        }
    }

    /// Queue `reloaded`, a finished refresh's reload of the open session,
    /// behind the pending reload, which it replaces unless that one's read
    /// runs, and start it once the viewer awaits no reload's rows.
    pub(super) fn queue_open_session_reload(&mut self, reloaded: Box<ReloadedSession>) {
        self.pending_view_reload = Some(match self.pending_view_reload.take() {
            Some(pending) => pending.followed_by(reloaded),
            None => PendingReload::Queued(reloaded),
        });
        self.start_queued_reload();
    }

    /// Start the queued reload's read once the viewer awaits no reload's
    /// rows. A read arriving during their render would stop it, and on a
    /// live session the next refresh can finish before a large render does,
    /// so the rows would never show.
    pub(super) fn start_queued_reload(&mut self) {
        if matches!(&self.app_mode, AppMode::View(state) if state.awaits_reload_rows()) {
            return;
        }
        self.pending_view_reload = match self.pending_view_reload.take() {
            Some(PendingReload::Queued(reloaded)) => {
                Some(PendingReload::start(reloaded, self.interactive_cache()))
            }
            pending => pending,
        };
    }

    /// Show the open session again after its refresh finished, as `reloaded`
    /// lists it, from `read`, the viewer's read of its copy, or the file
    /// opened directly as parsed, keeping the reader's place, once its rows
    /// arrive. The first read of a file opened directly shows from the top,
    /// as an open does. A copy asked for meanwhile runs once nothing more is
    /// pending.
    pub(super) fn reload_open_session(
        &mut self,
        reloaded: ReloadedSession,
        read: Option<InteractiveRead<ViewedConversation>>,
    ) {
        let AppMode::View(state) = &self.app_mode else {
            return;
        };
        let path = state.conversation_path.clone();
        let is_first_read_of_opened_file = self.single_file_mode && state.is_copy_missing;
        let (conversation, copy_row, adopted) = match (reloaded.parsed, read) {
            (Some(conversation), _) => (Some(conversation), None, None),
            (None, Some(read)) => {
                match self
                    .take_interactive(&path, read, ReadCause::FinishedRefresh)
                    .current_or_older_with_adopted()
                {
                    (Some(copy), adopted) => (Some(Arc::new(copy.conversation)), copy.row, adopted),
                    (None, _) => (None, None, None),
                }
            }
            (None, None) => (None, None, None),
        };
        if let AppMode::View(state) = &mut self.app_mode {
            state.cache_address = reloaded.address;
        }
        if let Some(conversation) = conversation {
            let header = ReloadedHeader {
                row: reloaded.row,
                external_title: reloaded.external_title,
                publication: reloaded.publication,
                adopted,
            };
            if is_first_read_of_opened_file {
                if let AppMode::View(state) = &mut self.app_mode {
                    state.parsed_conversation = conversation;
                    state.show_reloaded_row(header, copy_row);
                    state.session_id = reloaded.session_id;
                    state.rows_read = RowsRead::BeforeFirstRead;
                }
                self.request_view_render();
            } else {
                self.show_reread_conversation(conversation, header, copy_row, reloaded.session_id);
            }
        }
        self.run_deferred_copy();
    }

    /// True when a copy or export of the open session waits for its
    /// pending refresh, or, for a file opened directly outside every
    /// session root, for a reload's rows: the copy reads the newest read,
    /// and until then the rows show an earlier one.
    pub(super) fn defers_copy(&self) -> bool {
        let AppMode::View(state) = &self.app_mode else {
            return false;
        };
        state.awaits_reload_rows()
            || (!self.store.refuses_session_refreshes() && self.is_open_session_updating())
    }

    /// Run the copy or export asked for while the open session's refresh
    /// was pending, once no refresh or reload of the open session is
    /// pending and a reload's rows show. An export whose copy is still out
    /// of date or missing reports the failed read.
    fn run_deferred_copy(&mut self) {
        let AppMode::View(state) = &self.app_mode else {
            return;
        };
        if self.is_reload_pending(state) {
            return;
        }
        match self.deferred_copy.take() {
            Some(DeferredCopy::Export {
                option,
                to_clipboard,
            }) => match self.current_export_entries(ReadCause::FinishedRefresh) {
                Some(entries) => self.export_now(option, to_clipboard, Some(entries)),
                None => self.set_status(SESSION_READ_FAILED.to_owned()),
            },
            Some(DeferredCopy::Focused { viewport_height }) => {
                self.copy_focused_now(viewport_height);
            }
            None => {}
        }
    }

    pub fn exit_view_mode(&mut self) {
        self.stop_view_search();
        if let Some(worker) = &self.viewer_render {
            worker.stop();
        }
        self.app_mode = AppMode::List;
        self.pending_view_reload = None;
        self.deferred_copy = None;
    }

    pub(super) fn start_view_search(&mut self) {
        self.stop_view_search();
        if let AppMode::View(ref mut state) = self.app_mode {
            state.search_mode = ViewSearchMode::Typing;
            state.search_query.clear();
            state.search_matches.clear();
            state.current_match_index = 0;
        }
    }

    pub(super) fn clear_view_search(&mut self) {
        self.stop_view_search();
        if let AppMode::View(ref mut state) = self.app_mode {
            state.search_mode = ViewSearchMode::Off;
            state.search_query.clear();
            state.search_matches.clear();
        }
    }

    /// Stop the viewer's running or paused search; its matches aren't shown.
    fn stop_view_search(&mut self) {
        if let Some(worker) = &self.viewer_search {
            worker.stop();
        }
        if let AppMode::View(ref mut state) = self.app_mode {
            state.pending_search = None;
        }
    }

    pub(super) fn clear_view_search_query(&mut self) -> bool {
        if let AppMode::View(ref mut state) = self.app_mode
            && !state.search_query.is_empty()
        {
            state.search_query.clear();
            self.schedule_view_search();
            return true;
        }
        false
    }

    pub(super) fn delete_view_search_word_backwards(&mut self) {
        if let AppMode::View(ref mut state) = self.app_mode {
            let trimmed = state.search_query.trim_end();
            if let Some(last_space) = trimmed.rfind(|c: char| c.is_whitespace()) {
                state.search_query.truncate(last_space + 1);
            } else {
                state.search_query.clear();
            }
        }
        self.schedule_view_search();
    }

    pub(super) fn push_view_search_char(&mut self, c: char) {
        if let AppMode::View(ref mut state) = self.app_mode {
            state.search_query.push(c);
        }
        self.schedule_view_search();
    }

    pub(super) fn backspace_view_search(&mut self) {
        if let AppMode::View(ref mut state) = self.app_mode {
            state.search_query.pop();
        }
        self.schedule_view_search();
    }

    /// Confirm (`Enter`) in the search line: the search stays on its
    /// matches, or closes when it didn't find any. A paused search starts at
    /// once; once a running search returns, the search stays on its matches,
    /// or closes when it didn't find any.
    pub(super) fn commit_view_search(&mut self) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        match &mut state.pending_search {
            None if state.search_matches.is_empty() => state.search_mode = ViewSearchMode::Off,
            None => state.search_mode = ViewSearchMode::Active,
            Some(PendingViewSearch::Running {
                closes_without_matches,
                ..
            }) => {
                *closes_without_matches = true;
                state.search_mode = ViewSearchMode::Active;
            }
            Some(PendingViewSearch::Paused { .. }) => {
                state.search_mode = ViewSearchMode::Active;
                self.send_query_search(true);
            }
        }
    }

    /// Clear the edited query's matches, and schedule its search for the end
    /// of the pause after typing (`VIEW_SEARCH_PAUSE`). An empty query isn't
    /// searched.
    fn schedule_view_search(&mut self) {
        let due = self.clock().now() + VIEW_SEARCH_PAUSE;
        self.stop_view_search();
        if let AppMode::View(state) = &mut self.app_mode {
            state.search_matches.clear();
            state.current_match_index = 0;
            state.pending_search =
                (!state.search_query.is_empty()).then_some(PendingViewSearch::Paused { due });
        }
    }

    /// Send the viewer's paused search once the pause after typing has
    /// passed.
    pub(crate) fn send_due_view_search(&mut self) {
        let now = self.clock().now();
        if let AppMode::View(state) = &self.app_mode
            && let Some(PendingViewSearch::Paused { due }) = state.pending_search
            && now >= due
        {
            self.send_query_search(false);
        }
    }

    /// Send the search of the edited query: when its matches arrive, the view
    /// scrolls to the first, unless the view moved while the search ran.
    fn send_query_search(&mut self, closes_without_matches: bool) {
        let AppMode::View(state) = &self.app_mode else {
            return;
        };
        let from = ViewSpot::of(state);
        self.send_view_search(
            MatchesArrival::AtFirstMatch { from },
            closes_without_matches,
        );
    }

    /// Send the search of a re-render's rows: the view stays where it is when
    /// its matches arrive.
    fn send_rerender_search(&mut self, closes_without_matches: bool) {
        self.send_view_search(MatchesArrival::InPlace, closes_without_matches);
    }

    /// Send the search of the view's rows for its query to the viewer's
    /// search thread, superseding the running search.
    fn send_view_search(&mut self, arrival: MatchesArrival, closes_without_matches: bool) {
        let started = self.clock().now();
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let worker = self
            .viewer_search
            .get_or_insert_with(super::viewer_search::ViewerSearchWorker::spawn);
        let generation = worker.search(
            Arc::clone(&state.rendered_lines),
            state.search_query.clone(),
        );
        state.pending_search = Some(PendingViewSearch::Running {
            generation,
            started,
            arrival,
            closes_without_matches,
        });
    }

    /// True while the viewer's search is paused or running.
    pub fn is_view_search_pending(&self) -> bool {
        matches!(&self.app_mode, AppMode::View(state) if state.pending_search.is_some())
    }

    /// Show the matches of the viewer's running search once they arrive,
    /// waiting up to `wait` for them. Matches of an older search, or of a
    /// search the view stopped, are dropped. A panic on the search's thread
    /// resumes here.
    pub(crate) fn receive_view_search_results(&mut self, wait: Duration) {
        let Some(found) = self
            .viewer_search
            .as_mut()
            .and_then(|worker| worker.newest_found(wait))
        else {
            return;
        };
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let Some(PendingViewSearch::Running {
            generation,
            arrival,
            closes_without_matches,
            ..
        }) = state.pending_search
        else {
            return;
        };
        if found.generation != generation {
            return;
        }
        state.pending_search = None;
        state.search_matches = found.matches;
        let first_row = state.search_matches.first().map(SearchMatch::first_row);
        match (arrival, first_row) {
            (MatchesArrival::AtFirstMatch { from }, Some(first_row))
                if from == ViewSpot::of(state) =>
            {
                state.current_match_index = 0;
                state.scroll_offset = first_row;
                Self::focus_message_at_line(state, first_row);
            }
            (MatchesArrival::InPlace, _) => {
                state.current_match_index =
                    first_match_at_or_below(&state.search_matches, state.scroll_offset);
            }
            _ => {
                state.current_match_index = state
                    .current_match_index
                    .min(state.search_matches.len().saturating_sub(1));
            }
        }
        if closes_without_matches && state.search_matches.is_empty() {
            state.search_mode = ViewSearchMode::Off;
        }
    }

    pub(super) fn next_search_match(&mut self, viewport_height: usize) {
        if let AppMode::View(ref mut state) = self.app_mode {
            if state.search_matches.is_empty() {
                return;
            }
            state.current_match_index =
                (state.current_match_index + 1) % state.search_matches.len();
            Self::show_current_match(state, viewport_height);
        }
    }

    pub(super) fn prev_search_match(&mut self, viewport_height: usize) {
        if let AppMode::View(ref mut state) = self.app_mode {
            if state.search_matches.is_empty() {
                return;
            }
            state.current_match_index = if state.current_match_index == 0 {
                state.search_matches.len() - 1
            } else {
                state.current_match_index - 1
            };
            Self::show_current_match(state, viewport_height);
        }
    }

    /// Scroll the current match onto the screen, its last row too when the
    /// match fits the viewport, and focus its message.
    fn show_current_match(state: &mut ViewState, viewport_height: usize) {
        let found = &state.search_matches[state.current_match_index];
        let (first_row, last_row) = (found.first_row(), found.last_row());
        let viewport_end = state.scroll_offset + viewport_height;
        if first_row < state.scroll_offset || first_row >= viewport_end {
            state.scroll_offset = first_row;
        } else if last_row >= viewport_end && last_row - first_row < viewport_height {
            state.scroll_offset = last_row + 1 - viewport_height;
        }
        Self::focus_message_at_line(state, first_row);
    }

    pub(super) fn toggle_view_tools(&mut self) {
        if let AppMode::View(ref mut state) = self.app_mode {
            state.tool_display = state.tool_display.next();
            self.tool_display = state.tool_display;
            self.request_view_render();
        }
    }

    pub(super) fn toggle_view_thinking(&mut self) {
        if let AppMode::View(ref mut state) = self.app_mode {
            state.show_thinking = !state.show_thinking;
            self.show_thinking = state.show_thinking;
            self.request_view_render();
        }
    }

    pub(super) fn toggle_view_timing(&mut self) {
        if let AppMode::View(ref mut state) = self.app_mode {
            state.show_timing = !state.show_timing;
            self.show_timing = state.show_timing;
            self.request_view_render();
        }
    }

    /// Show `conversation`, the open session read again, where the reader
    /// was, once its rows arrive (see [`Self::show_render`]). Expanded rows
    /// stay expanded where their entry is still there. The header shows
    /// `copy_row`, the conversation's copy's catalogue row, or `header`'s row
    /// without it, at once. The previous reload's rows have shown
    /// ([`Self::start_queued_reload`]), so the shown rows are
    /// `parsed_conversation`'s.
    fn show_reread_conversation(
        &mut self,
        conversation: Arc<crate::tui::viewer::ParsedConversation>,
        header: ReloadedHeader,
        copy_row: Option<Box<Conversation>>,
        session_id: Option<String>,
    ) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let into_new_read = EntryRelocation::between(&state.parsed_conversation, &conversation);
        state.expanded_tool_outputs = state
            .expanded_tool_outputs
            .iter()
            .filter_map(|id| into_new_read.tool_output_id(id))
            .collect();
        if let Some(pending) = &mut state.pending_render {
            pending.focus_first_call_of = pending
                .focus_first_call_of
                .and_then(|entry| into_new_read.get(entry));
        }
        state.rows_read = RowsRead::Earlier(into_new_read);
        state.show_reloaded_row(header, copy_row);
        state.session_id = session_id;
        state.parsed_conversation = conversation;
        self.request_view_render();
    }

    /// Ask the `viewer-render` thread for a render of the open conversation
    /// with the view's current options, superseding the running render. The
    /// shown rows stay until [`Self::receive_view_render`] shows the new
    /// ones.
    pub(super) fn request_view_render(&mut self) {
        let now = self.clock().now();
        let AppMode::View(state) = &self.app_mode else {
            return;
        };
        let options = self.view_render_options(state);
        let conversation = Arc::clone(&state.parsed_conversation);
        let generation = self
            .viewer_render
            .get_or_insert_with(super::viewer_render::ViewerRenderWorker::spawn)
            .render(conversation, options);
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        // The render has every expanded output in its options.
        state.entries_to_render_again.clear();
        match &mut state.pending_render {
            Some(pending) => pending.generation = generation,
            None => {
                state.pending_render = Some(PendingRender {
                    generation,
                    since: now,
                    focus_first_call_of: None,
                });
            }
        }
    }

    /// True while a render of the open conversation runs and its rows
    /// haven't shown, or an entry waits to render again.
    pub fn is_view_render_pending(&self) -> bool {
        matches!(&self.app_mode, AppMode::View(state)
            if state.pending_render.is_some() || !state.entries_to_render_again.is_empty())
    }

    /// Schedule the rows of the outputs `ids` of `parsed_conversation` to
    /// render as the expanded set now has them. An entry that renders alone
    /// is recorded, and renders again in place once
    /// the shown rows can change (see [`Self::render_entries_again`]), unless
    /// its output doesn't toggle in the view's modes, such as a tool call in
    /// `tools·all`. Any other output, such as a tool run in summary mode,
    /// requests a whole render.
    fn schedule_toggled_outputs_render(&mut self, ids: &BTreeSet<ToolOutputId>) {
        let AppMode::View(state) = &self.app_mode else {
            return;
        };
        let options = self.view_render_options(state);
        let mut entries = Vec::new();
        for id in ids {
            match entry_rendering_alone(&state.parsed_conversation, &options, id) {
                Some(entry) if id.toggles_alone_in(&options) => entries.push(entry),
                Some(_) => {}
                None => return self.request_view_render(),
            }
        }
        if let AppMode::View(state) = &mut self.app_mode {
            state.entries_to_render_again.extend(entries);
        }
    }

    /// Render each entry waiting to render again in place of its rows, once
    /// no render is pending and no search shares the rows, keeping the
    /// reader's place as a whole render does. A running search sharing the
    /// rows is stopped, and the entries render on a pass after it frees
    /// them. The shown matches are updated in the entries' rows alone, and
    /// the first at or below the top row is selected, as after a whole
    /// render's search; a running search is sent again over the new rows.
    /// A whole render is requested when the rows don't have the extent an
    /// entry rendered in place relies on.
    fn render_entries_again(&mut self, viewport_height: usize) {
        let AppMode::View(state) = &self.app_mode else {
            return;
        };
        if state.entries_to_render_again.is_empty() || state.pending_render.is_some() {
            return;
        }
        let options = self.view_render_options(state);
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let place = ViewPlace::of(state, None);
        let Some(lines) = Arc::get_mut(&mut state.rendered_lines) else {
            // The stopped search frees the rows at its next check. It stays
            // in `pending_search`, and is sent again over the new rows once
            // the entries render.
            if matches!(
                state.pending_search,
                Some(PendingViewSearch::Running { .. })
            ) && let Some(worker) = &self.viewer_search
            {
                worker.stop();
            }
            return;
        };
        let conversation = Arc::clone(&state.parsed_conversation);
        let updates_shown_matches = state.search_mode != ViewSearchMode::Off
            && !state.search_query.is_empty()
            && state.pending_search.is_none();
        let mut is_extent_broken = false;
        for entry in std::mem::take(&mut state.entries_to_render_again) {
            let shown = ShownRows {
                lines,
                messages: &mut state.message_ranges,
                calls: &mut state.call_ranges,
                detail_calls: &mut state.detail_calls,
            };
            let replaced = match render_entry_in_place(&conversation, &options, shown, entry) {
                EntryInPlace::Replaced(replaced) => replaced,
                EntryInPlace::Unchanged => continue,
                EntryInPlace::ExtentBroken => {
                    is_extent_broken = true;
                    break;
                }
            };
            if updates_shown_matches {
                update_matches_in_replaced_rows(
                    &mut state.search_matches,
                    lines,
                    &state.search_query,
                    &replaced,
                );
            }
        }
        state.total_lines = lines.len();
        place.restore(state, viewport_height);
        if is_extent_broken {
            // The whole render shows every output as the expanded set has it.
            self.request_view_render();
        } else if updates_shown_matches {
            state.current_match_index =
                first_match_at_or_below(&state.search_matches, state.scroll_offset);
        } else {
            self.search_shown_rows_again();
        }
    }

    /// Show the newest render's rows once they arrive, waiting up to `wait`
    /// for a finished render. Call it on every pass, with the viewer open or
    /// closed: the `viewer-render` thread starts the next render only once
    /// the finished one is taken and its rows released. A render finished
    /// for an earlier request or a closed viewer isn't shown. The rows no
    /// longer shown, or the render's when it isn't shown, are sent to the
    /// `viewer-render` thread to be freed there. Once a reload's rows show,
    /// the reload queued behind it starts its read. A panic on the
    /// `viewer-render` thread resumes here. Then the entries waiting to
    /// render again do, in the rows shown.
    pub(crate) fn receive_view_render(&mut self, viewport_height: usize, wait: Duration) {
        self.receive_whole_render(viewport_height, wait);
        self.render_entries_again(viewport_height);
    }

    fn receive_whole_render(&mut self, viewport_height: usize, wait: Duration) {
        let Some(worker) = self.viewer_render.as_mut() else {
            return;
        };
        let Some(rendered) = worker.take_rendered(wait) else {
            return;
        };
        let awaited = match &self.app_mode {
            AppMode::View(state) => state
                .pending_render
                .filter(|pending| pending.generation == rendered.generation),
            AppMode::List => None,
        };
        let Some(pending) = awaited else {
            worker.release(Arc::new(rendered.rendered_conversation.lines));
            return;
        };
        if let AppMode::View(state) = &mut self.app_mode {
            state.pending_render = None;
        }
        let replaced = self.show_render(viewport_height, rendered.rendered_conversation);
        if let Some(worker) = &self.viewer_render {
            worker.release(replaced);
        }
        if let Some(entry) = pending.focus_first_call_of {
            self.focus_first_call_of(entry, viewport_height);
        }
        self.start_queued_reload();
        self.run_deferred_copy();
    }

    /// Focus the first call of the focused message when that message is
    /// `entry`'s.
    fn focus_first_call_of(&mut self, entry: usize, viewport_height: usize) {
        let AppMode::View(state) = &self.app_mode else {
            return;
        };
        let is_focused = Self::focused_message_range(state)
            .is_some_and(|(_, message)| message.entry_index == entry);
        if is_focused {
            self.focus_first_call(viewport_height);
        }
    }

    /// The options `state`'s conversation renders with: the view's modes,
    /// width and expanded outputs, today's date and the clock's zone.
    pub(super) fn view_render_options(&self, state: &ViewState) -> RenderOptions {
        RenderOptions {
            tool_display: state.tool_display,
            show_thinking: state.show_thinking,
            show_timing: state.show_timing,
            content_width: state.content_width(),
            expanded_tool_outputs: state.expanded_tool_outputs.clone(),
            can_expand: true,
            today: self.today(),
            zone: self.clock().zone(),
        }
    }

    /// Show `rendered`, a render of the open conversation, keeping the anchor
    /// message on its row and the focused call focused.
    ///
    /// Rows of an earlier read (`RowsRead::Earlier`) are a reload's: the
    /// anchor and the focused call move into the newer read through the
    /// relocation; a reader at the bottom stays at the bottom so new
    /// messages show; and messages the newer read adds or drops set the
    /// view's `CountHighlight`.
    ///
    /// Returns the rows no longer shown: the replaced ones, or `rendered`'s
    /// without a viewer.
    fn show_render(
        &mut self,
        viewport_height: usize,
        rendered: RenderedConversation,
    ) -> Arc<Vec<RenderedLine>> {
        let now = self.clock().now();
        let AppMode::View(state) = &mut self.app_mode else {
            return Arc::new(rendered.lines);
        };
        let rows_read = std::mem::replace(&mut state.rows_read, RowsRead::Current);
        let relocation = match &rows_read {
            RowsRead::Earlier(relocation) => Some(relocation),
            RowsRead::Current | RowsRead::BeforeFirstRead => None,
        };
        let is_reload = relocation.is_some();
        let stick_to_bottom = is_reload && state.is_at_bottom(viewport_height);
        let gone_messages = relocation.map_or(0, |relocation| {
            state
                .message_ranges
                .iter()
                .filter(|message| relocation.get(message.entry_index).is_none())
                .count()
        });
        let place = ViewPlace::of(state, relocation);

        state.total_lines = rendered.lines.len();
        let replaced = std::mem::replace(&mut state.rendered_lines, Arc::new(rendered.lines));
        state.message_ranges = rendered.messages;
        state.call_ranges = rendered.calls;
        state.detail_calls = rendered.detail_calls;
        if !matches!(rows_read, RowsRead::Current) {
            state.is_copy_missing = false;
            state.reload_rows_shown_at = Some(now);
        }
        if is_reload {
            state.hovered_tool_output = None;
        }

        place.restore(state, viewport_height);
        let max_scroll = state.total_lines.saturating_sub(viewport_height);
        if stick_to_bottom {
            state.scroll_offset = max_scroll;
            if state.message_nav_active {
                Self::sync_focus_to_scroll(state, viewport_height);
            }
        }

        // Messages are counted by entry, not by range: a display key pressed
        // during the reload's render changes how many ranges the same
        // messages make.
        if let Some(relocation) = relocation {
            let earlier_entries = relocation.later_indices_of_earlier_entries();
            let new_messages = state
                .message_ranges
                .iter()
                .filter(|message| !earlier_entries.contains(&message.entry_index))
                .count();
            if new_messages + gone_messages > 0 {
                state.count_highlight = Some(CountHighlight {
                    until: now + COUNT_HIGHLIGHT,
                    arrived_below: (stick_to_bottom && new_messages > 0).then_some(new_messages),
                });
            }
        }

        self.search_shown_rows_again();
        replaced
    }

    /// Search the query again over new rows, without the pause: the old
    /// matches point at rows that moved. A paused search reads the new rows
    /// when it is sent.
    fn search_shown_rows_again(&mut self) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        if state.search_mode == ViewSearchMode::Off || state.search_query.is_empty() {
            return;
        }
        state.search_matches.clear();
        match state.pending_search {
            Some(PendingViewSearch::Paused { .. }) => {}
            Some(PendingViewSearch::Running {
                arrival: MatchesArrival::AtFirstMatch { .. },
                closes_without_matches,
                ..
            }) => self.send_query_search(closes_without_matches),
            Some(PendingViewSearch::Running {
                arrival: MatchesArrival::InPlace,
                closes_without_matches,
                ..
            }) => self.send_rerender_search(closes_without_matches),
            None => self.send_rerender_search(false),
        }
    }

    /// `]` and `[` walk one list of stops: a message with no expanded run, or
    /// each call of one that has. Every `]` is undone by one `[`, so
    /// overshooting a run's end costs nothing.
    ///
    /// The run as a whole is not a stop. `←` focuses it, and `]` from there
    /// leaves the run rather than stepping back into its calls.
    fn step_focus(&mut self, viewport_height: usize, step: fn(&ViewState, Focus) -> Option<Focus>) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        if state.message_ranges.is_empty() {
            return;
        }
        if !state.message_nav_active {
            state.message_nav_active = true;
            Self::sync_focus_to_scroll(state, viewport_height);
        }
        let Some(stop) = state.focus.and_then(|focus| step(state, focus)) else {
            return;
        };
        state.focus = Some(stop);
        Self::ensure_focus_visible(state, viewport_height);
    }

    fn next_stop(state: &ViewState, focus: Focus) -> Option<Focus> {
        if let Some(call) = focus.call_index {
            let calls = Self::message_calls(state, focus.message_index);
            if calls.contains(&(call + 1)) {
                return Some(Focus {
                    call_index: Some(call + 1),
                    ..focus
                });
            }
        }
        let next_message = focus.message_index + 1;
        (next_message < state.message_ranges.len())
            .then(|| Self::first_stop_in(state, next_message))
    }

    fn prev_stop(state: &ViewState, focus: Focus) -> Option<Focus> {
        if let Some(call) = focus.call_index {
            let calls = Self::message_calls(state, focus.message_index);
            if let Some(prev) = call.checked_sub(1).filter(|prev| calls.contains(prev)) {
                return Some(Focus {
                    call_index: Some(prev),
                    ..focus
                });
            }
        }
        let prev_message = focus.message_index.checked_sub(1)?;
        Some(Self::last_stop_in(state, prev_message))
    }

    /// Where a step down into `message_index` lands: the first call of an
    /// expanded run, or the message itself when it has no calls.
    fn first_stop_in(state: &ViewState, message_index: usize) -> Focus {
        Focus {
            message_index,
            call_index: Self::message_calls(state, message_index).next(),
        }
    }

    /// The same from below, so one `[` undoes the `]` that left the run.
    fn last_stop_in(state: &ViewState, message_index: usize) -> Focus {
        Focus {
            message_index,
            call_index: Self::message_calls(state, message_index).last(),
        }
    }

    fn ensure_focus_visible(state: &mut ViewState, viewport_height: usize) {
        let Some(focus) = state.focus else {
            return;
        };
        let row = match focus.call_index {
            Some(call) => state.call_ranges.get(call).map(|c| c.input.start_line),
            None => Self::focused_message_range(state).map(|(_, message)| message.start_line),
        };
        if let Some(row) = row {
            Self::ensure_line_visible(state, row, viewport_height);
        }
    }

    fn set_focused_message(state: &mut ViewState, message_index: usize) {
        if state.focused_message() != Some(message_index) {
            state.focus = Some(Focus {
                message_index,
                call_index: None,
            });
        }
    }

    fn focus_message_at_line(state: &mut ViewState, line_idx: usize) {
        let found = state
            .message_ranges
            .iter()
            .position(|m| line_idx >= m.start_line && line_idx < m.end_line);
        if let Some(idx) = found {
            state.message_nav_active = true;
            Self::set_focused_message(state, idx);
        }
    }

    pub(super) fn focus_next(&mut self, viewport_height: usize) {
        self.step_focus(viewport_height, Self::next_stop);
    }

    pub(super) fn focus_prev(&mut self, viewport_height: usize) {
        self.step_focus(viewport_height, Self::prev_stop);
    }

    fn focused_call_is_active(&self) -> bool {
        matches!(&self.app_mode, AppMode::View(state) if state.focused_call().is_some())
    }

    fn focus_first_call(&mut self, viewport_height: usize) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let calls = Self::focused_message_calls(state);
        if !calls.is_empty() {
            Self::focus_call(state, calls.start, viewport_height);
        }
    }

    fn focus_call(state: &mut ViewState, call: usize, viewport_height: usize) {
        let Some(focus) = &mut state.focus else {
            return;
        };
        focus.call_index = Some(call);
        let first_row = state.call_ranges[call].input.start_line;
        Self::ensure_line_visible(state, first_row, viewport_height);
    }

    fn focused_message_calls(state: &ViewState) -> std::ops::Range<usize> {
        match state.focused_message() {
            Some(message) => Self::message_calls(state, message),
            None => 0..0,
        }
    }

    /// The calls of `message`, as a range of indices into `call_ranges`. Empty
    /// unless the message is a tool run someone expanded.
    fn message_calls(state: &ViewState, message: usize) -> std::ops::Range<usize> {
        let Some(message) = state.message_ranges.get(message) else {
            return 0..0;
        };
        let message_rows = message.start_line..message.end_line;
        let first = state
            .call_ranges
            .partition_point(|call| call.input.start_line < message_rows.start);
        let count = state.call_ranges[first..]
            .iter()
            .take_while(|call| message_rows.contains(&call.input.start_line))
            .count();
        first..first + count
    }

    fn focused_call_range(state: &ViewState) -> Option<&CallRange> {
        state
            .focused_call()
            .and_then(|call| state.call_ranges.get(call))
    }

    pub(super) fn expand_focused(&mut self, viewport_height: usize) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let ids = Self::focused_tool_output_ids(state);
        if ids.is_empty() {
            return;
        }
        let message_stop_entry = match state.focused_call() {
            Some(_) => None,
            None => Self::focused_message_range(state).map(|(_, message)| message.entry_index),
        };
        if !Self::expand_all(state, ids.clone()) {
            if message_stop_entry.is_some() {
                self.focus_first_call(viewport_height);
            }
            return;
        }
        self.schedule_toggled_outputs_render(&ids);
        if let Some(entry) = message_stop_entry
            && let AppMode::View(state) = &mut self.app_mode
        {
            // The run's calls show in the new rows; the first is focused
            // once they arrive.
            let entry_in_newest_read = state.entry_in_newest_read(entry);
            if let Some(pending) = &mut state.pending_render {
                pending.focus_first_call_of = entry_in_newest_read;
            }
        }
    }

    pub(super) fn collapse_focused(&mut self) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let ids = Self::focused_tool_output_ids(state);
        if Self::collapse_all(state, ids.clone()) {
            self.schedule_toggled_outputs_render(&ids);
            return;
        }
        if let Some(focus) = &mut state.focus
            && focus.call_index.is_some()
        {
            focus.call_index = None;
        }
    }

    pub(super) fn toggle_focused(&mut self) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let ids = Self::focused_tool_output_ids(state);
        if ids.is_empty() {
            return;
        }
        let is_every_id_expanded = ids
            .iter()
            .all(|id| state.expanded_tool_outputs.contains(id));
        if is_every_id_expanded {
            Self::collapse_all(state, ids.clone());
        } else {
            Self::expand_all(state, ids.clone());
        }
        self.schedule_toggled_outputs_render(&ids);
    }

    /// True when at least one id was collapsed before.
    fn expand_all(state: &mut ViewState, ids: BTreeSet<ToolOutputId>) -> bool {
        let mut was_collapsed = false;
        for id in ids {
            was_collapsed |= state.expanded_tool_outputs.insert(id);
        }
        was_collapsed
    }

    /// True when at least one id was expanded before.
    fn collapse_all(state: &mut ViewState, ids: BTreeSet<ToolOutputId>) -> bool {
        let mut was_expanded = false;
        for id in ids {
            was_expanded |= state.expanded_tool_outputs.remove(&id);
        }
        was_expanded
    }

    /// The ids the focused stop toggles: a focused call's expandable areas,
    /// otherwise the focused message's own rows, in `parsed_conversation`.
    fn focused_tool_output_ids(state: &ViewState) -> BTreeSet<ToolOutputId> {
        let shown_ids = match Self::focused_call_range(state) {
            Some(call) => call
                .areas()
                .filter(|area| Self::is_expandable(state, area))
                .map(|area| area.id.clone())
                .collect(),
            None => Self::focused_message_tool_output_ids(state),
        };
        shown_ids
            .into_iter()
            .filter_map(|id| state.id_in_newest_read(id))
            .collect()
    }

    /// The renderer marks a row clickable only where a click toggles it: a
    /// truncated body, its `(N more lines...)` row, or an expanded body.
    fn is_expandable(state: &ViewState, area: &CallArea) -> bool {
        state.rendered_lines[area.start_line..area.end_line]
            .iter()
            .any(|line| line.clickable)
    }

    pub(super) fn sync_focus_after_scroll(&mut self, viewport_height: usize) {
        if let AppMode::View(ref mut state) = self.app_mode
            && state.message_nav_active
        {
            Self::sync_focus_to_scroll(state, viewport_height);
        }
    }

    pub fn scroll_view(&mut self, delta: isize, viewport_height: usize) {
        if let AppMode::View(ref mut state) = self.app_mode {
            if state.search_mode == ViewSearchMode::Typing {
                return;
            }
            let max_scroll = state.total_lines.saturating_sub(viewport_height);
            let new_offset = if delta >= 0 {
                state
                    .scroll_offset
                    .saturating_add(delta as usize)
                    .min(max_scroll)
            } else {
                state.scroll_offset.saturating_sub((-delta) as usize)
            };
            state.scroll_offset = new_offset;
            self.sync_focus_after_scroll(viewport_height);
        }
    }

    pub fn scroll_mouse(&mut self, delta: isize, viewport_height: usize) {
        if self.dialog_mode != DialogMode::None {
            return;
        }

        match self.app_mode {
            AppMode::List => {
                self.clear_selection_anchor_on_loaded_list();
                self.move_selection_by(delta.signum());
            }
            AppMode::View(_) => self.scroll_view(delta, viewport_height),
        }
    }

    fn view_line_at_row(&self, row: u16, frame_area: Rect) -> Option<usize> {
        let AppMode::View(state) = &self.app_mode else {
            return None;
        };
        if self.dialog_mode != DialogMode::None {
            return None;
        }
        let layout = ui::view_layout_rects(frame_area, self, state);
        if row < layout.content.y || row >= layout.content.y.saturating_add(layout.content.height) {
            return None;
        }
        Some(state.scroll_offset + (row - layout.content.y) as usize)
    }

    fn message_idx_at_line(ranges: &[MessageRange], line_idx: usize) -> Option<usize> {
        let idx = ranges.partition_point(|m| m.end_line <= line_idx);
        ranges
            .get(idx)
            .is_some_and(|m| line_idx >= m.start_line && line_idx < m.end_line)
            .then_some(idx)
    }

    fn view_tool_output_at_line(&self, line_idx: usize) -> Option<ToolOutputId> {
        let AppMode::View(state) = &self.app_mode else {
            return None;
        };
        state.rendered_lines.get(line_idx).and_then(|line| {
            if line.clickable {
                line.tool_output_id.clone()
            } else {
                None
            }
        })
    }

    pub fn handle_view_mouse_move(&mut self, row: u16, frame_area: Rect) -> bool {
        let next = self
            .view_line_at_row(row, frame_area)
            .and_then(|line_idx| self.view_tool_output_at_line(line_idx));
        let AppMode::View(state) = &mut self.app_mode else {
            return false;
        };
        if state.hovered_tool_output == next {
            return false;
        }
        state.hovered_tool_output = next;
        true
    }

    pub fn handle_view_click(&mut self, row: u16, frame_area: Rect) -> bool {
        let Some(line_idx) = self.view_line_at_row(row, frame_area) else {
            return false;
        };
        let tool_output = self.view_tool_output_at_line(line_idx);
        let message_idx = if let AppMode::View(state) = &self.app_mode {
            Self::message_idx_at_line(&state.message_ranges, line_idx)
        } else {
            None
        };
        if tool_output.is_none() && message_idx.is_none() {
            return false;
        }

        let AppMode::View(state) = &mut self.app_mode else {
            return false;
        };
        let mut changed = false;
        if let Some(focus) = &mut state.focus
            && focus.call_index.is_some()
        {
            focus.call_index = None;
            changed = true;
        }
        if let Some(message_index) = message_idx
            && (!state.message_nav_active || state.focused_message() != Some(message_index))
        {
            state.message_nav_active = true;
            state.focus = Some(Focus {
                message_index,
                call_index: None,
            });
            changed = true;
        }
        let mut toggled_output = None;
        if let Some(id) = tool_output {
            if let Some(id_in_newest_read) = state.id_in_newest_read(id.clone()) {
                Self::toggle_expanded_tool_output(state, id_in_newest_read.clone());
                toggled_output = Some(id_in_newest_read);
            }
            state.hovered_tool_output = Some(id);
            changed = true;
        }
        // The rows don't depend on the focus: only a tool output expanded or
        // collapsed renders them again.
        if let Some(id) = toggled_output {
            self.schedule_toggled_outputs_render(&BTreeSet::from([id]));
        }
        changed
    }

    /// Rows inside the message's calls are excluded: in `tools·sum` they
    /// belong to the call stops, so an expanded run yields its run row alone.
    fn focused_message_tool_output_ids(state: &ViewState) -> BTreeSet<ToolOutputId> {
        if !state.message_nav_active {
            return BTreeSet::new();
        }
        let Some((message_index, message)) = Self::focused_message_range(state) else {
            return BTreeSet::new();
        };
        let calls = &state.call_ranges[Self::message_calls(state, message_index)];
        let is_inside_a_call = |row: usize| calls.iter().any(|call| call.contains_line(row));
        message
            .rows()
            .filter(|&row| !is_inside_a_call(row))
            .filter_map(|row| state.rendered_lines.get(row))
            .filter(|line| line.clickable)
            .filter_map(|line| line.tool_output_id.clone())
            .collect()
    }

    fn focused_message_range(state: &ViewState) -> Option<(usize, &MessageRange)> {
        let message_index = state.focused_message()?;
        let message = state.message_ranges.get(message_index)?;
        Some((message_index, message))
    }

    fn toggle_expanded_tool_output(state: &mut ViewState, id: ToolOutputId) {
        if !state.expanded_tool_outputs.remove(&id) {
            state.expanded_tool_outputs.insert(id);
        }
    }

    /// Keep the focus where the scroll left it on screen. A focus that drifted
    /// off an edge moves to the on-screen stop nearest that edge: the first
    /// when it drifted off the top, the last when it drifted off the bottom.
    ///
    /// Never scrolls. The scroll keys call this after moving the offset.
    fn sync_focus_to_scroll(state: &mut ViewState, viewport_height: usize) {
        if state.message_ranges.is_empty() {
            return;
        }
        let viewport = state.scroll_offset..state.scroll_offset + viewport_height;
        let drift = Self::focused_message_drift(state, &viewport);
        let Some(message_index) = Self::clamped_message(state, &viewport, drift) else {
            return;
        };
        let call_index = Self::clamped_call(state, &viewport, message_index, drift);
        state.focus = Some(Focus {
            message_index,
            call_index,
        });
    }

    /// A view with no message focused yet reads as drifted off the top, so the
    /// focus lands on the first message on screen.
    fn focused_message_drift(state: &ViewState, viewport: &std::ops::Range<usize>) -> FocusDrift {
        match state
            .focused_message()
            .and_then(|index| state.message_ranges.get(index))
        {
            Some(message) => FocusDrift::of(message.rows(), viewport),
            None => FocusDrift::OffTop,
        }
    }

    /// `None` when the focus drifted off an edge and no message is on screen,
    /// which leaves the focus where it was.
    fn clamped_message(
        state: &ViewState,
        viewport: &std::ops::Range<usize>,
        drift: FocusDrift,
    ) -> Option<usize> {
        let on_screen = state
            .message_ranges
            .iter()
            .enumerate()
            .filter(|(_, message)| FocusDrift::of(message.rows(), viewport).is_on_screen())
            .map(|(index, _)| index);
        drift.clamp(state.focused_message(), on_screen)
    }

    /// The call inside `message_index` the focus lands on. `None` when the
    /// message has no calls, or none on screen. That is the clearing the other
    /// focus moves do when they leave a message.
    fn clamped_call(
        state: &ViewState,
        viewport: &std::ops::Range<usize>,
        message_index: usize,
        message_drift: FocusDrift,
    ) -> Option<usize> {
        let calls = Self::message_calls(state, message_index);
        let focused = state.focused_call().filter(|call| calls.contains(call));
        // A call still in the focused message drifts on its own rows. One left
        // behind in another message inherits the message's drift, so both
        // levels land on the same edge.
        let drift = match focused {
            Some(call) => Self::call_drift(&state.call_ranges[call], viewport),
            None => message_drift,
        };
        let on_screen = calls.filter(|&call| state.call_ranges[call].overlaps(viewport));
        drift.clamp(focused, on_screen)
    }

    /// A call's input and result are separate areas, with other calls' rows
    /// possibly between them. A viewport that fell into that gap counts as off
    /// the top, since the input row is the one the focus marks.
    fn call_drift(call: &CallRange, viewport: &std::ops::Range<usize>) -> FocusDrift {
        if call.overlaps(viewport) {
            FocusDrift::OnScreen
        } else if call.input.start_line >= viewport.end {
            FocusDrift::OffBottom
        } else {
            FocusDrift::OffTop
        }
    }

    fn ensure_line_visible(state: &mut ViewState, line_idx: usize, viewport_height: usize) {
        let max_scroll = state.total_lines.saturating_sub(viewport_height);
        if line_idx < state.scroll_offset || line_idx >= state.scroll_offset + viewport_height {
            state.scroll_offset = line_idx.min(max_scroll);
        }
    }

    fn copy_focused_message(&mut self, viewport_height: usize) {
        if let AppMode::View(ref mut state) = self.app_mode
            && !state.message_nav_active
        {
            state.message_nav_active = true;
            Self::sync_focus_to_scroll(state, viewport_height);
        }

        let AppMode::View(state) = &self.app_mode else {
            return;
        };
        let Some(entry_index) = state
            .focused_message()
            .and_then(|index| state.message_ranges.get(index))
            .map(|message| message.entry_index)
        else {
            return;
        };
        let options = crate::tui::export::ExportOptions {
            show_tools: state.tool_display.is_visible(),
            show_thinking: state.show_thinking,
        };
        // The entry as the viewer read it: the file may have grown since.
        let text = state
            .parsed_conversation
            .entry(entry_index)
            .map(|entry| crate::tui::export::format_entry_for_clipboard(entry, options));

        match text {
            Some(text) if text.is_empty() => {
                self.set_status("No text content in this message".to_owned());
            }
            Some(text) => self.copy_to_clipboard("Message", &text),
            None => self.set_status("Message not found".to_owned()),
        }
    }

    /// Copy the focused call or message; while the open session's refresh is
    /// pending, once it finishes, so the copy holds the newest messages.
    pub(super) fn copy_focused(&mut self, viewport_height: usize) {
        if self.defers_copy() {
            self.deferred_copy = Some(DeferredCopy::Focused { viewport_height });
            return;
        }
        self.copy_focused_now(viewport_height);
    }

    fn copy_focused_now(&mut self, viewport_height: usize) {
        if self.focused_call_is_active() {
            self.copy_focused_call();
        } else {
            self.copy_focused_message(viewport_height);
        }
    }

    fn copy_focused_call(&mut self) {
        match self.focused_call_text() {
            Ok(text) => self.copy_to_clipboard("Call", &text),
            Err(e) => self.set_status(e),
        }
    }

    fn focused_call_text(&self) -> Result<String, String> {
        let AppMode::View(state) = &self.app_mode else {
            return Err("Not viewing a conversation".to_string());
        };
        let call = Self::focused_call_range(state).ok_or_else(|| "No call focused".to_string())?;
        let conversation = &state.parsed_conversation;
        crate::tui::export::extract_call_text(
            |entry_index| conversation.entry(entry_index),
            call.input.location,
            call.result.as_ref().map(|result| result.location),
        )
    }

    pub fn check_view_resize(&mut self, frame_width: usize) {
        if let AppMode::View(ref mut state) = self.app_mode
            && state.frame_width != frame_width
        {
            state.frame_width = frame_width;
            self.request_view_render();
        }
    }
}

/// The rows the focus marks, relative to the viewport after a scroll: still
/// on screen, or off an edge.
#[derive(Clone, Copy, Debug)]
enum FocusDrift {
    OnScreen,
    OffTop,
    OffBottom,
}

impl FocusDrift {
    fn of(rows: std::ops::Range<usize>, viewport: &std::ops::Range<usize>) -> Self {
        if rows.end <= viewport.start {
            Self::OffTop
        } else if rows.start >= viewport.end {
            Self::OffBottom
        } else {
            Self::OnScreen
        }
    }

    fn is_on_screen(self) -> bool {
        matches!(self, Self::OnScreen)
    }

    fn clamp(
        self,
        current: Option<usize>,
        mut on_screen: impl DoubleEndedIterator<Item = usize>,
    ) -> Option<usize> {
        match self {
            Self::OnScreen => current,
            Self::OffTop => on_screen.next(),
            Self::OffBottom => on_screen.next_back(),
        }
    }
}

/// How long the view marks a refresh's change in the message count.
const COUNT_HIGHLIGHT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Clone, Copy, Debug)]
struct ScrollAnchor {
    entry_index: usize,
    relative_row: isize,
}

/// The reader's place in the shown rows: the anchor message and its row on
/// screen, and the focused call, to find again in new rows.
struct ViewPlace {
    anchor: Option<ScrollAnchor>,
    focused_call_id: Option<ToolOutputId>,
    scroll_offset: usize,
}

impl ViewPlace {
    /// `state`'s place, its entries moved into a newer read through
    /// `relocation` when the new rows are that read's.
    fn of(state: &ViewState, relocation: Option<&EntryRelocation>) -> Self {
        let anchor = capture_anchor(
            &state.message_ranges,
            state.scroll_offset,
            state.focused_message(),
            state.message_nav_active,
        )
        .and_then(|anchor| match relocation {
            Some(relocation) => Some(ScrollAnchor {
                entry_index: relocation.get_or_previous(anchor.entry_index)?,
                ..anchor
            }),
            None => Some(anchor),
        });
        let focused_call_id = App::focused_call_range(state)
            .map(|call| call.input.id.clone())
            .and_then(|id| match relocation {
                Some(relocation) => relocation.tool_output_id(&id),
                None => Some(id),
            });
        Self {
            anchor,
            focused_call_id,
            scroll_offset: state.scroll_offset,
        }
    }

    /// Focus the anchor message and the focused call in `state`'s new rows,
    /// and scroll the anchor message back to its row on screen.
    fn restore(self, state: &mut ViewState, viewport_height: usize) {
        let max_scroll = state.total_lines.saturating_sub(viewport_height);

        let resolved_idx = self
            .anchor
            .and_then(|a| find_message_idx_or_prev(&state.message_ranges, a.entry_index))
            .or_else(|| (!state.message_ranges.is_empty()).then_some(0));
        // The call is found again by its id, not its index, so rows opening
        // above it do not move the focus.
        let resolved_call = self.focused_call_id.and_then(|id| {
            state
                .call_ranges
                .iter()
                .position(|call| call.input.id == id)
        });
        state.focus = resolved_idx.map(|message_index| Focus {
            message_index,
            call_index: resolved_call,
        });

        state.scroll_offset = match (self.anchor, resolved_idx) {
            (Some(a), Some(idx)) => {
                let new_msg = &state.message_ranges[idx];
                let rel = if new_msg.entry_index == a.entry_index {
                    a.relative_row
                } else {
                    a.relative_row.min(0)
                };
                let raw = new_msg.start_line as isize - rel;
                raw.clamp(0, max_scroll as isize) as usize
            }
            _ => self.scroll_offset.min(max_scroll),
        };
    }
}

fn capture_anchor(
    ranges: &[MessageRange],
    scroll_offset: usize,
    focused: Option<usize>,
    nav_active: bool,
) -> Option<ScrollAnchor> {
    if ranges.is_empty() {
        return None;
    }

    let msg = if nav_active {
        focused.and_then(|i| ranges.get(i))
    } else {
        None
    }
    .unwrap_or_else(|| {
        let i = ranges.partition_point(|m| m.start_line < scroll_offset);
        ranges.get(i).unwrap_or_else(|| ranges.last().unwrap())
    });

    Some(ScrollAnchor {
        entry_index: msg.entry_index,
        relative_row: msg.start_line as isize - scroll_offset as isize,
    })
}

/// The index of the first of `matches` starting at or below `top_row`; the
/// last match when every match starts above it.
fn first_match_at_or_below(matches: &[SearchMatch], top_row: usize) -> usize {
    matches
        .iter()
        .position(|found| found.first_row() >= top_row)
        .unwrap_or(matches.len().saturating_sub(1))
}

fn find_message_idx_or_prev(ranges: &[MessageRange], entry_index: usize) -> Option<usize> {
    if ranges.is_empty() {
        return None;
    }
    match ranges.binary_search_by_key(&entry_index, |m| m.entry_index) {
        Ok(idx) => Some(idx),
        Err(0) => Some(0),
        Err(idx) => Some(idx - 1),
    }
}
