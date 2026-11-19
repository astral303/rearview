use super::session_store::{FinishedRefresh, ReadCause, SessionCopy};
use super::text_block_focus::list_of_item;
use super::viewer_render::Rendered;
use super::{
    App, AppMode, CopyIdentity, CountHighlight, DialogMode, Focus, HeaderRow, MatchesArrival,
    OpenSessionStatus, PendingRender, PendingViewSearch, RowsRead, TextBlockFocus, ViewSearchMode,
    ViewSpot, ViewState,
};
use crate::history::provider::{RenamedTitle, SessionRead, SessionTitle, TitleAndSummary};
use crate::history::sqlite_cache::{AdoptedCopy, EntryHashes, Publication, SessionAddress};
use crate::history::{Conversation, InteractiveCache, InteractiveRead, Source};
use crate::tui::ui;
use crate::tui::viewer::{
    CallArea, CallRange, ConversationTail, EntryRelocation, MessageInPlace, MessageRange,
    ParsedConversation, RenderOptions, RenderedConversation, RenderedLine, ReplacedEntries,
    ReplacedRows, RereadConversation, RowsToRender, SearchMatch, ShownRender, ShownRows,
    ToolOutputId, ViewedConversation, entry_rendering_alone, message_showing,
    parse_conversation_file_interactively, render_message_in_place,
    reread_conversation_interactively, splice_tail, tail_render_start,
    update_matches_in_replaced_rows,
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

/// The most rows of a tail the drawing thread searches for the shown
/// matches. That search doesn't stop, and the frame showing the tail waits
/// for it; a tail starting early in a long session can have most of its
/// rows. A larger tail's search runs on `viewer-search`, and the drawing
/// thread keeps drawing.
pub(super) const TAIL_ROWS_SEARCHED_WHILE_DRAWING: usize = 4_000;

/// The open session's row as its finished refresh read it, for the viewer's
/// read of the session from the cache.
pub(super) struct ReloadedSession {
    publication: Publication,
    /// The copy the row is published at; `None` for an unpublished row and
    /// a file outside every provider's session roots.
    copy: Option<CopyIdentity>,
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
            copy: None,
            session_id: row.map(|row| row.session_id.clone()),
            row: row.map(HeaderRow::of),
            external_title: None,
            address: None,
            parsed: Some(Arc::new(conversation)),
        }
    }

    /// True when the row is published at the copy the view shows.
    fn is_copy_of(&self, state: &ViewState) -> bool {
        self.copy.is_some() && self.copy == state.shown_copy
    }
}

impl ViewState {
    /// The options the conversation renders with: the view's modes, width
    /// and expanded outputs, `today` and `zone`.
    fn render_options(&self, today: chrono::NaiveDate, zone: crate::clock::Zone) -> RenderOptions {
        RenderOptions {
            tool_display: self.tool_display,
            show_thinking: self.show_thinking,
            show_timing: self.show_timing,
            content_width: self.content_width(),
            expanded_tool_outputs: self.expanded_tool_outputs.clone(),
            can_expand: true,
            today,
            zone,
        }
    }

    /// True while the search of the query is open and its matches have
    /// arrived: rows rendered again update those matches.
    fn shows_finished_search(&self) -> bool {
        self.search_mode != ViewSearchMode::Off
            && !self.search_query.is_empty()
            && self.pending_search.is_none()
    }

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

    /// Replace the view's conversation with `conversation`, read from the
    /// copy `copy` identifies when the read found it current, with
    /// `entry_hashes`, the hashes of its entries the read returned.
    fn replace_conversation(
        &mut self,
        conversation: Arc<ParsedConversation>,
        copy: Option<CopyIdentity>,
        entry_hashes: Option<EntryHashes>,
    ) {
        self.parsed_conversation = conversation;
        self.shown_copy = copy;
        self.entry_hashes = entry_hashes.map(Arc::new);
    }

    /// The shown copy's entry hashes for a reload's read to compare with its
    /// copy's; `None` when the view doesn't have them, or has another count
    /// of them than of entries. The reload then reads the whole copy.
    fn hashes_for_tail_read(&self) -> Option<Arc<EntryHashes>> {
        self.entry_hashes
            .clone()
            .filter(|hashes| hashes.len() == self.parsed_conversation.entry_count())
    }

    /// Show `arrived` in place of the shown rows, its messages and calls in
    /// place of theirs. Returns the rows no longer shown, and for a tail the
    /// rows it replaced.
    fn put_arrived_rows(
        &mut self,
        arrived: ArrivedRows,
    ) -> (Arc<Vec<RenderedLine>>, Option<ReplacedRows>) {
        match arrived {
            ArrivedRows::Whole(rendered) => {
                let replaced =
                    std::mem::replace(&mut self.rendered_lines, Arc::new(rendered.lines));
                self.message_ranges = rendered.messages;
                self.call_ranges = rendered.calls;
                self.detail_calls = rendered.detail_calls;
                (replaced, None)
            }
            ArrivedRows::Tail { row, tail } => {
                let lines = Arc::get_mut(&mut self.rendered_lines)
                    .expect("a tail shows once no search shares the shown rows");
                let shown = ShownRows {
                    lines,
                    messages: &mut self.message_ranges,
                    calls: &mut self.call_ranges,
                    detail_calls: &mut self.detail_calls,
                };
                let spliced = splice_tail(shown, tail, row);
                (Arc::new(spliced.removed), Some(spliced.replaced))
            }
        }
    }

    /// The rows to render with `options` once a tail is put after the kept
    /// entries of the conversation, in place of `replaced`: the tail
    /// [`tail_render_start`] finds after the shown rows. The whole
    /// conversation renders again while the shown rows are not a render of
    /// the conversation before the tail with the options they record:
    /// before the first rows show, while a render is pending, and while a
    /// message waits to render again in place.
    fn rows_after_tail(&self, replaced: &ReplacedEntries, options: &RenderOptions) -> RowsToRender {
        let are_rows_current = matches!(self.rows_read, RowsRead::Current)
            && self.pending_render.is_none()
            && self.entries_to_render_again.is_empty()
            && self.run_outputs_to_render_again.is_empty();
        match &self.rows_options {
            Some(rows_options) if are_rows_current => tail_render_start(
                ShownRender::before(
                    replaced,
                    &self.parsed_conversation,
                    &self.message_ranges,
                    rows_options,
                ),
                &self.parsed_conversation,
                replaced.first_changed(),
                options,
            ),
            _ => RowsToRender::Whole,
        }
    }

    /// Show `reloaded`, the refresh of an unchanged shown copy, without a
    /// read or a render. The header keeps the copy row's fields,
    /// as a reload of the same copy shows them. The deferred copy runs in
    /// the pass that finds nothing waiting to show, after the first call of
    /// `focus_first_call_of` is focused.
    fn show_row_over_its_copy(&mut self, reloaded: ReloadedSession, now: crate::clock::Moment) {
        if let (Some(header), Some(row)) = (&mut self.header, &reloaded.row) {
            header.take_title_and_summary_of(row);
        }
        self.publication = reloaded.publication;
        self.cache_address = reloaded.address;
        self.session_id = reloaded.session_id;
        self.reload_rows_shown_at = Some(now);
        self.runs_deferred_copy_once_shown = true;
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
            header.take_title_and_summary_of(row);
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

/// Rows a render returned to show in place of the viewer's.
enum ArrivedRows {
    /// A render of every entry, in place of every shown row.
    Whole(RenderedConversation),
    /// A render of the tail, in place of the shown rows from `row` on.
    Tail {
        row: usize,
        tail: RenderedConversation,
    },
}

impl ArrivedRows {
    fn into_lines(self) -> Vec<RenderedLine> {
        match self {
            Self::Whole(rendered) | Self::Tail { tail: rendered, .. } => rendered.lines,
        }
    }
}

/// The entries of a [`RereadConversation`] as the view takes them, without
/// the copy's entry hashes and catalogue row.
enum ReloadedEntries {
    /// Every entry, as a conversation that replaces the shown one.
    Whole(Arc<ParsedConversation>),
    /// The entries from the first changed entry on. The view puts them
    /// after the entries it keeps of the shown conversation.
    Tail(ConversationTail),
}

/// [`ReloadedEntries`] with the values the view shows with them: the
/// identity and the entry hashes of the copy read, the header, and the
/// session's id.
pub(super) struct ReloadedConversation {
    entries: ReloadedEntries,
    /// The copy the entries were read from, when the read found it current.
    copy: Option<CopyIdentity>,
    /// One hash per entry of that copy.
    entry_hashes: Option<EntryHashes>,
    /// The refreshed row.
    header: ReloadedHeader,
    /// The catalogue row of the copy read.
    copy_row: Option<Box<Conversation>>,
    session_id: Option<String>,
}

/// The open session's refreshed row once its refresh is read again, with its
/// `publication`, and `adopted`, the copy the read adopted in its place.
struct ReloadedHeader {
    row: Option<HeaderRow>,
    external_title: Option<SessionTitle>,
    publication: Publication,
    adopted: Option<AdoptedCopy>,
}

/// The viewer's read of the open session, its next step recorded, or the
/// file opened directly as parsed.
struct ViewerCopy<C> {
    /// The copy's entries as the read returned them: a conversation for an
    /// open, [`ReloadedEntries`] for a reload. `None` when the copy is
    /// missing.
    entries: Option<C>,
    /// The catalogue row of the copy.
    row: Option<Box<Conversation>>,
    /// The copy the read adopted in the row's place.
    adopted: Option<AdoptedCopy>,
    /// The copy's identity, when the read found it current.
    identity: Option<CopyIdentity>,
    /// One hash per entry of the copy, as [`ViewState::entry_hashes`]
    /// keeps them.
    entry_hashes: Option<EntryHashes>,
}

impl<C> ViewerCopy<C> {
    /// `copy`, a read's copy. `parts` separates it into its entries, one
    /// hash per entry and its catalogue row.
    fn of<V>(
        copy: SessionCopy<V>,
        parts: impl FnOnce(V) -> (C, EntryHashes, Option<Box<Conversation>>),
    ) -> Self {
        let (viewed, adopted, is_current) = match copy {
            SessionCopy::Current(current) => {
                let (viewed, adopted) = current.into_parts();
                (viewed, adopted, true)
            }
            SessionCopy::Older(viewed) => (viewed, None, false),
            SessionCopy::Missing => return Self::missing(),
        };
        let (conversation, entry_hashes, row) = parts(viewed);
        let identity = if is_current {
            row.as_deref().and_then(CopyIdentity::of)
        } else {
            None
        };
        Self {
            entries: Some(conversation),
            row,
            adopted,
            identity,
            entry_hashes: Some(entry_hashes),
        }
    }

    fn missing() -> Self {
        Self {
            entries: None,
            row: None,
            adopted: None,
            identity: None,
            entry_hashes: None,
        }
    }
}

impl ViewerCopy<ReloadedEntries> {
    /// The file opened directly, parsed into `conversation`.
    fn parsed(conversation: Arc<ParsedConversation>) -> Self {
        Self {
            entries: Some(ReloadedEntries::Whole(conversation)),
            ..Self::missing()
        }
    }
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
pub(super) type ReloadRead = (ReloadedSession, Option<ReloadedCopy>);

/// A reload's read of the open session's copy before its next step is
/// recorded: a [`RereadConversation`] with the hashes it was compared with.
pub(super) struct ReloadedCopy {
    reread: InteractiveRead<RereadConversation>,
    /// The shown copy's entry hashes the read compared its copy's with;
    /// `None` for a read of the whole copy without a comparison.
    compared_with: Option<Arc<EntryHashes>>,
}

impl ReloadedCopy {
    /// The copy `cache` keeps at `address` for a row whose publication is
    /// `publication`: its tail after the entries equal to the shown copy's,
    /// by `shown_hashes`, or the whole copy without them.
    fn read(
        cache: &InteractiveCache,
        address: &SessionAddress,
        publication: &Publication,
        shown_hashes: Option<Arc<EntryHashes>>,
    ) -> Self {
        let reread = match &shown_hashes {
            Some(shown) => reread_conversation_interactively(cache, address, publication, shown),
            None => parse_conversation_file_interactively(cache, address, publication)
                .map(|viewed| RereadConversation::Whole(Box::new(viewed))),
        };
        Self {
            reread,
            compared_with: shown_hashes,
        }
    }
}

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
    /// show ([`App::start_or_show_queued_reload`]).
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
    /// A read's tail that arrived while a render shared the shown
    /// conversation. The view puts it after the entries it keeps on the
    /// first pass when nothing else shares the conversation. A newer refresh
    /// finishing meanwhile is `queued`.
    WaitingToExtend {
        tail: Box<ReloadedConversation>,
        queued: Option<Box<ReloadedSession>>,
    },
}

/// A reload ready to show, and the newer refresh's row to read once its
/// rows show.
pub(super) struct ReadyReload {
    pub(super) arrived: ArrivedReload,
    pub(super) queued: Option<Box<ReloadedSession>>,
}

pub(super) enum ArrivedReload {
    /// A read that arrived, or a file opened directly as parsed.
    Read(Box<ReloadRead>),
    /// A tail that waited for the shown conversation.
    WaitingTail(Box<ReloadedConversation>),
}

impl PendingReload {
    /// `reloaded`, reading its copy in `cache` when it has one: the tail
    /// after the entries equal to the shown copy's, by `shown_hashes`, or
    /// the whole copy without them.
    fn start(
        reloaded: Box<ReloadedSession>,
        cache: InteractiveCache,
        shown_hashes: Option<Arc<EntryHashes>>,
    ) -> Self {
        if reloaded.parsed.is_some() {
            return Self::Ready(reloaded);
        }
        let Some(address) = reloaded.address.clone() else {
            return Self::Ready(reloaded);
        };
        Self::Reading {
            read: ViewerRead::start(move || {
                let copy =
                    ReloadedCopy::read(&cache, &address, &reloaded.publication, shown_hashes);
                (*reloaded, Some(copy))
            }),
            queued: None,
        }
    }

    /// This reload followed by `newer`, a later refresh's: a read running or
    /// a tail waiting stays with `newer` queued; otherwise `newer` replaces
    /// it.
    fn followed_by(self, newer: Box<ReloadedSession>) -> Self {
        match self {
            Self::Queued(_) | Self::Ready(_) => Self::Queued(newer),
            Self::Reading { read, .. } => Self::Reading {
                read,
                queued: Some(newer),
            },
            Self::WaitingToExtend { tail, .. } => Self::WaitingToExtend {
                tail,
                queued: Some(newer),
            },
        }
    }

    /// The reload and the row queued behind it, once its read arrives within
    /// `wait`; `Err(self)` while it runs or is queued. A waiting tail is
    /// returned at once.
    pub(super) fn finish(self, wait: Duration) -> Result<ReadyReload, Self> {
        match self {
            Self::Queued(_) => Err(self),
            Self::Ready(reloaded) => Ok(ReadyReload {
                arrived: ArrivedReload::Read(Box::new((*reloaded, None))),
                queued: None,
            }),
            Self::Reading { read, queued } => match read.finish(wait) {
                Ok(read) => Ok(ReadyReload {
                    arrived: ArrivedReload::Read(Box::new(read)),
                    queued,
                }),
                Err(read) => Err(Self::Reading { read, queued }),
            },
            Self::WaitingToExtend { tail, queued } => Ok(ReadyReload {
                arrived: ArrivedReload::WaitingTail(tail),
                queued,
            }),
        }
    }

    pub(super) fn is_reading(&self) -> bool {
        matches!(self, Self::Reading { .. })
    }

    pub(super) fn is_waiting_to_extend(&self) -> bool {
        matches!(self, Self::WaitingToExtend { .. })
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
            copy: CopyIdentity::of(row),
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

/// The status of a copy of a message, block or item without text.
pub(super) const NO_TEXT_TO_COPY: &str = "No text content in this message";

impl App {
    /// Open the selected session at once, reading and rendering it on this
    /// thread; the event loop defers through `request_open` and
    /// `complete_pending_open` instead.
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
        let state = self.take_opened_view(row, read, frame_width);
        let options = self.view_render_options(&state);
        let rendered =
            crate::tui::viewer::render_parsed_conversation(&state.parsed_conversation, &options);
        self.app_mode = AppMode::View(state);
        // A first render shows from the top whatever the viewport's height.
        self.show_render(0, ArrivedRows::Whole(rendered), options);
    }

    /// The viewer's state for `row`, from `read`, the viewer's read of its
    /// copy, with its render requested of `viewer-render` at `frame_width`.
    /// [`Self::receive_view_render`] shows the viewer once its rows arrive.
    pub(super) fn request_opened_view_rows(
        &mut self,
        row: OpenedRow,
        read: Option<InteractiveRead<ViewedConversation>>,
        frame_width: usize,
    ) -> ViewState {
        let now = self.clock().now();
        let mut state = self.take_opened_view(row, read, frame_width);
        let options = self.view_render_options(&state);
        request_render(
            &mut self.viewer_render,
            &mut state,
            options,
            RowsToRender::Whole,
            now,
        );
        state
    }

    /// The viewer's state for `row`, from `read`, before any render: without
    /// rows, at the top, nothing focused. Its first render's rows show from
    /// the top, the first message focused ([`Self::show_render`]). Records
    /// the read's next step.
    fn take_opened_view(
        &mut self,
        row: OpenedRow,
        read: Option<InteractiveRead<ViewedConversation>>,
        frame_width: usize,
    ) -> ViewState {
        let OpenedRow {
            path,
            source,
            session_id,
            publication,
            cache_address,
            external_title,
            header,
        } = row;
        let copy = match read {
            Some(read) => self.take_viewer_read(&path, read, ReadCause::UserAction),
            None => ViewerCopy::missing(),
        };
        let mut state = ViewState::initial(
            path,
            Some(source),
            Some(session_id),
            self.tool_display,
            self.show_thinking,
        );
        state.cache_address = cache_address;
        state.publication = publication;
        state.header = Some(header);
        state.is_copy_missing = copy.entries.is_none();
        if let Some(conversation) = copy.entries {
            state.replace_conversation(conversation, copy.identity, copy.entry_hashes);
        }
        state.show_timing = self.show_timing;
        state.frame_width = frame_width;
        state.show_copy_header_and_adopt(copy.row, external_title.as_ref(), copy.adopted);
        state
    }

    /// `read`, the viewer's read of the session at `path` for `cause`, with
    /// its next step recorded.
    fn take_viewer_read(
        &mut self,
        path: &std::path::Path,
        read: InteractiveRead<ViewedConversation>,
        cause: ReadCause,
    ) -> ViewerCopy<Arc<ParsedConversation>> {
        ViewerCopy::of(self.take_interactive(path, read, cause), |viewed| {
            (
                Arc::new(viewed.conversation),
                viewed.entry_hashes,
                viewed.row,
            )
        })
    }

    /// `read`, a reload's read of the session at `path` after its refresh
    /// finished, with its next step recorded.
    fn take_reload_read(
        &mut self,
        path: &std::path::Path,
        read: InteractiveRead<RereadConversation>,
    ) -> ViewerCopy<ReloadedEntries> {
        let copy = self.take_interactive(path, read, ReadCause::FinishedRefresh);
        ViewerCopy::of(copy, |reread| match reread {
            RereadConversation::Whole(viewed) => {
                let viewed = *viewed;
                (
                    ReloadedEntries::Whole(Arc::new(viewed.conversation)),
                    viewed.entry_hashes,
                    viewed.row,
                )
            }
            RereadConversation::Tail(viewed) => (
                ReloadedEntries::Tail(viewed.tail),
                viewed.entry_hashes,
                viewed.row,
            ),
        })
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
        if let Some(read) = self
            .store
            .newest_finished()
            .and_then(|finished| OpenSessionRead::in_finished(finished, state))
        {
            self.follow_open_session_read(read);
        }
    }

    /// Follow each finished refresh the list has not applied yet, oldest
    /// first, as [`Self::follow_open_session`] follows a refresh finishing in
    /// the viewer, until one leaves the viewer. Call it once an open shows
    /// its viewer: the list doesn't apply refreshes while an open is pending.
    pub(super) fn follow_unapplied_refreshes(&mut self) {
        let AppMode::View(state) = &self.app_mode else {
            return;
        };
        let reads: Vec<OpenSessionRead> = self
            .store
            .finished_refreshes()
            .filter_map(|finished| OpenSessionRead::in_finished(finished, state))
            .collect();
        for read in reads {
            if !matches!(self.app_mode, AppMode::View(_)) {
                return;
            }
            self.follow_open_session_read(read);
        }
    }

    /// Follow `read`, a finished refresh's read of the open session.
    fn follow_open_session_read(&mut self, read: OpenSessionRead) {
        match read {
            OpenSessionRead::Listed(reloaded) => self.queue_open_session_reload(reloaded),
            OpenSessionRead::Gone(status) => {
                self.deferred_copy = None;
                if !self.single_file_mode {
                    self.exit_view_mode();
                }
                self.set_status(status.to_owned());
            }
            OpenSessionRead::Failed(status) => {
                self.set_status(status);
                self.run_deferred_copy();
            }
            OpenSessionRead::Retitled(title) => {
                if let AppMode::View(state) = &mut self.app_mode {
                    state.show_retitled(&title);
                }
            }
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
        self.start_or_show_queued_reload();
    }

    /// Once the viewer awaits no reload's rows, show the queued reload at
    /// once when it is the refresh of an unchanged shown copy, or
    /// start its read otherwise. A read arriving during a reload's render
    /// would stop it, and on a live session the next refresh can finish
    /// before a large render does, so the rows would never show.
    pub(super) fn start_or_show_queued_reload(&mut self) {
        if matches!(&self.app_mode, AppMode::View(state) if state.awaits_reload_rows()) {
            return;
        }
        let now = self.clock().now();
        match self.pending_view_reload.take() {
            Some(PendingReload::Queued(reloaded)) => {
                if let AppMode::View(state) = &mut self.app_mode
                    && reloaded.is_copy_of(state)
                {
                    state.show_row_over_its_copy(*reloaded, now);
                } else {
                    let shown_hashes = match &self.app_mode {
                        AppMode::View(state) => state.hashes_for_tail_read(),
                        AppMode::List => None,
                    };
                    self.pending_view_reload = Some(PendingReload::start(
                        reloaded,
                        self.interactive_cache(),
                        shown_hashes,
                    ));
                }
            }
            pending => self.pending_view_reload = pending,
        }
    }

    /// Read `reloaded`'s whole copy, in place of a read that compared its
    /// copy with other hashes than the view's. The reload queued behind it
    /// stays queued.
    fn read_whole_copy_again(&mut self, reloaded: ReloadedSession) {
        let queued = self.take_queued_reload();
        let reading = PendingReload::start(Box::new(reloaded), self.interactive_cache(), None);
        self.pending_view_reload = Some(match queued {
            Some(queued) => reading.followed_by(queued),
            None => reading,
        });
    }

    /// The refresh's row queued for a read once the arrived reload's rows
    /// show, taken out of the pending reload.
    fn take_queued_reload(&mut self) -> Option<Box<ReloadedSession>> {
        match self.pending_view_reload.take() {
            Some(PendingReload::Queued(queued)) => Some(queued),
            pending => {
                self.pending_view_reload = pending;
                None
            }
        }
    }

    /// Show the open session again after its refresh finished, as `reloaded`
    /// lists it, from `read`, the viewer's read of its copy, or the file
    /// opened directly as parsed, keeping the reader's place, once its rows
    /// arrive. The first read of a file opened directly shows from the top,
    /// as an open does. A copy asked for meanwhile runs once nothing more is
    /// pending.
    ///
    /// A read that compared its copy with other hashes than the view's is
    /// dropped, and the whole copy is read in its place: its tail follows
    /// other entries than the view keeps.
    pub(super) fn reload_open_session(
        &mut self,
        reloaded: ReloadedSession,
        read: Option<ReloadedCopy>,
    ) {
        let AppMode::View(state) = &self.app_mode else {
            return;
        };
        if let Some(compared) = read.as_ref().and_then(|copy| copy.compared_with.as_ref())
            && !state
                .entry_hashes
                .as_ref()
                .is_some_and(|shown| Arc::ptr_eq(shown, compared))
        {
            return self.read_whole_copy_again(reloaded);
        }
        let path = state.conversation_path.clone();
        let is_first_read_of_opened_file = self.single_file_mode && state.is_copy_missing;
        let copy = match (reloaded.parsed, read) {
            (Some(conversation), _) => ViewerCopy::parsed(conversation),
            (None, Some(copy)) => self.take_reload_read(&path, copy.reread),
            (None, None) => ViewerCopy::missing(),
        };
        if let AppMode::View(state) = &mut self.app_mode {
            state.cache_address = reloaded.address;
        }
        if let Some(entries) = copy.entries {
            let header = ReloadedHeader {
                row: reloaded.row,
                external_title: reloaded.external_title,
                publication: reloaded.publication,
                adopted: copy.adopted,
            };
            match entries {
                ReloadedEntries::Whole(conversation) if is_first_read_of_opened_file => {
                    if let AppMode::View(state) = &mut self.app_mode {
                        state.replace_conversation(conversation, copy.identity, copy.entry_hashes);
                        state.show_reloaded_row(header, copy.row);
                        state.session_id = reloaded.session_id;
                        state.rows_read = RowsRead::BeforeFirstRead;
                    }
                    self.request_view_render();
                }
                entries => {
                    self.show_reloaded_conversation(Box::new(ReloadedConversation {
                        entries,
                        copy: copy.identity,
                        entry_hashes: copy.entry_hashes,
                        header,
                        copy_row: copy.row,
                        session_id: reloaded.session_id,
                    }));
                }
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
            (MatchesArrival::KeepsSelected { selected }, _) => {
                state.current_match_index = selected;
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

    /// Show `reloaded`, the open session read again, where the reader was,
    /// once its rows arrive (see [`Self::show_render`]). Expanded rows stay
    /// expanded where their entry is still there. The header shows the
    /// catalogue row of the copy read, or the refreshed row without it, at
    /// once. The previous reload's rows have shown
    /// ([`Self::start_or_show_queued_reload`]): the shown rows are
    /// `parsed_conversation`'s.
    ///
    /// - A whole read's conversation replaces the shown one, and every row
    ///   renders again.
    /// - A tail is put after the entries the shown conversation keeps, and
    ///   the rows render from the start [`ViewState::rows_after_tail`]
    ///   finds.
    /// - A tail whose conversation a render shares is not put in place: the
    ///   render is stopped, the tail waits in `pending_view_reload` for a
    ///   later pass, and this returns false.
    pub(super) fn show_reloaded_conversation(
        &mut self,
        reloaded: Box<ReloadedConversation>,
    ) -> bool {
        let (now, today, zone) = (self.clock().now(), self.today(), self.clock().zone());
        let AppMode::View(state) = &mut self.app_mode else {
            return true;
        };
        if matches!(reloaded.entries, ReloadedEntries::Tail(_))
            && Arc::get_mut(&mut state.parsed_conversation).is_none()
        {
            self.wait_to_extend(reloaded);
            return false;
        }
        let ReloadedConversation {
            entries,
            copy,
            entry_hashes,
            header,
            copy_row,
            session_id,
        } = *reloaded;
        let (into_new_read, replaced) = match entries {
            ReloadedEntries::Whole(conversation) => {
                let relocation =
                    EntryRelocation::between(&state.parsed_conversation, &conversation);
                state.parsed_conversation = conversation;
                (relocation, None)
            }
            ReloadedEntries::Tail(tail) => {
                let conversation = Arc::get_mut(&mut state.parsed_conversation)
                    .expect("no render shares the conversation: checked above");
                let replaced = conversation.extend_with(tail);
                let relocation = EntryRelocation::after_extension(conversation, &replaced);
                (relocation, Some(replaced))
            }
        };
        state.shown_copy = copy;
        state.entry_hashes = entry_hashes.map(Arc::new);
        state.expanded_tool_outputs = state
            .expanded_tool_outputs
            .iter()
            .filter_map(|id| into_new_read.tool_output_id(id))
            .collect();
        state.focus_first_call_of = state
            .focus_first_call_of
            .and_then(|entry| into_new_read.get(entry));
        let options = state.render_options(today, zone);
        let rows_to_render = match &replaced {
            Some(replaced) => state.rows_after_tail(replaced, &options),
            None => RowsToRender::Whole,
        };
        state.rows_read = RowsRead::Earlier(into_new_read);
        state.show_reloaded_row(header, copy_row);
        state.session_id = session_id;
        request_render(&mut self.viewer_render, state, options, rows_to_render, now);
        true
    }

    /// Keep `tail` in `pending_view_reload` until no render shares the shown
    /// conversation, and stop the render running over it. The worker frees
    /// its share of the conversation at the render's next check, or, for a
    /// render that already finished, once the app releases the rows it took.
    /// The reload queued behind the tail stays queued.
    fn wait_to_extend(&mut self, tail: Box<ReloadedConversation>) {
        if let Some(worker) = &self.viewer_render {
            worker.stop();
        }
        let queued = self.take_queued_reload();
        self.pending_view_reload = Some(PendingReload::WaitingToExtend { tail, queued });
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
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        request_render(
            &mut self.viewer_render,
            state,
            options,
            RowsToRender::Whole,
            now,
        );
    }

    /// True while a render of the open conversation runs and its rows
    /// haven't shown, or a message waits to render again.
    pub fn is_view_render_pending(&self) -> bool {
        matches!(&self.app_mode, AppMode::View(state)
            if state.pending_render.is_some()
                || !state.entries_to_render_again.is_empty()
                || !state.run_outputs_to_render_again.is_empty())
    }

    /// Schedule the rows of the outputs `ids` of `parsed_conversation` to
    /// render as the expanded set now has them, once the shown rows can
    /// change (see [`Self::render_messages_again`]). An entry that renders
    /// alone is recorded and renders again in place, unless its output
    /// doesn't toggle in the view's modes, such as a tool call in
    /// `tools·all`. In summary mode, any other output is a tool run or an
    /// output inside one: it is recorded, and the run renders again in
    /// place. Outside summary mode, an output that doesn't render alone is
    /// dropped: only a message's outputs toggle, and there a message renders
    /// alone.
    fn schedule_toggled_outputs_render(&mut self, ids: &BTreeSet<ToolOutputId>) {
        let AppMode::View(state) = &self.app_mode else {
            return;
        };
        let options = self.view_render_options(state);
        let mut entries = Vec::new();
        let mut run_outputs = Vec::new();
        for id in ids {
            match entry_rendering_alone(&state.parsed_conversation, &options, id) {
                Some(entry) if id.toggles_alone_in(&options) => entries.push(entry),
                Some(_) => {}
                None if options.tool_display.is_summary() => run_outputs.push(id.clone()),
                None => {}
            }
        }
        if let AppMode::View(state) = &mut self.app_mode {
            state.entries_to_render_again.extend(entries);
            state.run_outputs_to_render_again.extend(run_outputs);
        }
    }

    /// Render each message waiting to render again in place of its rows,
    /// once no render is pending and no search shares the rows, keeping the
    /// reader's place as a whole render does: each entry waiting, and the
    /// tool run whose rows show each run output waiting. A running search
    /// sharing the rows is stopped, and the messages render on a pass after
    /// it frees them. The shown matches are updated in the messages' rows
    /// alone, and the first at or below the top row is selected, as after a
    /// whole render's search; a running search is sent again over the new
    /// rows. A whole render is requested when the rows don't have the
    /// extent a message rendered in place relies on.
    fn render_messages_again(&mut self, viewport_height: usize) {
        let AppMode::View(state) = &self.app_mode else {
            return;
        };
        let is_any_waiting = !state.entries_to_render_again.is_empty()
            || !state.run_outputs_to_render_again.is_empty();
        if !is_any_waiting || state.pending_render.is_some() {
            return;
        }
        let options = self.view_render_options(state);
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let place = ViewPlace::of(state, None);
        let updates_shown_matches = state.shows_finished_search();
        let Some(lines) = Arc::get_mut(&mut state.rendered_lines) else {
            return self.stop_search_sharing_rows();
        };
        let conversation = Arc::clone(&state.parsed_conversation);
        let mut entries = std::mem::take(&mut state.entries_to_render_again);
        // Find each run output's run now: the shown rows are the newest
        // render's.
        entries.extend(
            std::mem::take(&mut state.run_outputs_to_render_again)
                .iter()
                .filter_map(|id| message_showing(lines, &state.message_ranges, id)),
        );
        let mut is_extent_broken = false;
        for entry in entries {
            let shown = ShownRows {
                lines,
                messages: &mut state.message_ranges,
                calls: &mut state.call_ranges,
                detail_calls: &mut state.detail_calls,
            };
            let replaced = match render_message_in_place(&conversation, &options, shown, entry) {
                MessageInPlace::Replaced(replaced) => replaced,
                MessageInPlace::Unchanged => continue,
                MessageInPlace::ExtentBroken => {
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
            state.rows_options = None;
            return self.request_view_render();
        }
        if let Some(rows_options) = &mut state.rows_options {
            rows_options.expanded_tool_outputs = options.expanded_tool_outputs;
        }
        if updates_shown_matches {
            state.current_match_index =
                first_match_at_or_below(&state.search_matches, state.scroll_offset);
        } else {
            self.search_shown_rows_again();
        }
    }

    /// Show the newest render's rows once they arrive, waiting up to `wait`
    /// for a finished render. Call it on every pass, with the viewer open or
    /// closed: the `viewer-render` thread starts the next render only once
    /// the finished one is taken and its rows released.
    ///
    /// - A render finished for an earlier request or a closed viewer isn't
    ///   shown.
    /// - A tail's rows show on a pass when no search shares the shown rows;
    ///   a running search sharing them is stopped.
    /// - The rows no longer shown, or the render's when it isn't shown, are
    ///   sent to the `viewer-render` thread to be freed there.
    /// - Once a reload's rows show, the reload queued behind it starts its
    ///   read.
    /// - Once a pending open's rows arrive, its viewer shows them and follows
    ///   the finished refreshes the list has not applied yet.
    /// - A panic on the `viewer-render` thread resumes here.
    /// - Then the messages waiting to render again render in the rows shown.
    /// - Once nothing waits to show, the first call of the message of the
    ///   entry in `focus_first_call_of` is focused.
    /// - Then, after a render's rows showed, the deferred copy runs.
    pub(crate) fn receive_view_render(&mut self, viewport_height: usize, wait: Duration) {
        self.receive_render(viewport_height, wait);
        self.render_messages_again(viewport_height);
        if self.is_view_render_pending() {
            return;
        }
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let focus_first_call_of = state.focus_first_call_of.take();
        let runs_deferred_copy = std::mem::take(&mut state.runs_deferred_copy_once_shown);
        if let Some(entry) = focus_first_call_of {
            self.focus_first_call_of(entry, viewport_height);
        }
        if runs_deferred_copy {
            self.run_deferred_copy();
        }
    }

    fn receive_render(&mut self, viewport_height: usize, wait: Duration) {
        let Some(rendered) = self
            .viewer_render
            .as_mut()
            .and_then(|worker| worker.take_rendered(wait))
        else {
            return;
        };
        let generation = rendered.generation;
        let awaits = |state: &ViewState| {
            state
                .pending_render
                .is_some_and(|pending| pending.generation == generation)
        };
        if matches!(&self.app_mode, AppMode::View(state) if awaits(state)) {
            self.show_awaited_render(viewport_height, rendered);
        } else if self.opening_view().is_some_and(awaits) {
            self.finish_open(viewport_height, rendered);
        } else {
            self.release_rows(Arc::new(rendered.rendered_conversation.lines));
        }
    }

    /// Show `rendered`, the render the viewer awaits, and release the rows it
    /// replaces. A tail shows in place of the shown rows from its start's row
    /// on once no search shares them: until then it is kept for the next
    /// pass, and a running search sharing the rows is stopped. Once a
    /// reload's rows show, the reload queued behind it starts.
    pub(super) fn show_awaited_render(&mut self, viewport_height: usize, rendered: Rendered) {
        let arrived = match rendered.rows_to_render {
            RowsToRender::Whole => ArrivedRows::Whole(rendered.rendered_conversation),
            RowsToRender::Tail(_) if self.are_shown_rows_shared() => {
                self.stop_search_sharing_rows();
                if let Some(worker) = &mut self.viewer_render {
                    worker.keep_for_next_take(rendered);
                }
                return;
            }
            RowsToRender::Tail(start) => ArrivedRows::Tail {
                row: start.row,
                tail: rendered.rendered_conversation,
            },
        };
        self.show_awaited(viewport_height, arrived, rendered.options);
    }

    /// True while the viewer is open and a search shares its shown rows.
    fn are_shown_rows_shared(&mut self) -> bool {
        let AppMode::View(state) = &mut self.app_mode else {
            return false;
        };
        Arc::get_mut(&mut state.rendered_lines).is_none()
    }

    /// Stop the running search, which shares the shown rows: it frees them
    /// at its next check. It stays in `pending_search`, and is sent again
    /// over the new rows once they show.
    fn stop_search_sharing_rows(&self) {
        if let AppMode::View(state) = &self.app_mode
            && matches!(
                state.pending_search,
                Some(PendingViewSearch::Running { .. })
            )
            && let Some(worker) = &self.viewer_search
        {
            worker.stop();
        }
    }

    /// Show `arrived`, the rows the viewer awaits, rendered with `options`,
    /// and release the rows they replace. Once a reload's rows show, the
    /// reload queued behind it starts.
    fn show_awaited(
        &mut self,
        viewport_height: usize,
        arrived: ArrivedRows,
        options: RenderOptions,
    ) {
        if let AppMode::View(state) = &mut self.app_mode {
            state.pending_render = None;
            state.runs_deferred_copy_once_shown = true;
        }
        let replaced = self.show_render(viewport_height, arrived, options);
        self.release_rows(replaced);
        self.start_or_show_queued_reload();
    }

    /// Free `rows`, no longer shown, on the `viewer-render` thread.
    fn release_rows(&self, rows: Arc<Vec<RenderedLine>>) {
        if let Some(worker) = &self.viewer_render {
            worker.release(rows);
        }
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
        state.render_options(self.today(), self.clock().zone())
    }

    /// Show `arrived`, rows of the open conversation rendered with
    /// `options`, keeping the anchor message on its row and the focused call
    /// focused.
    ///
    /// Rows of an earlier read (`RowsRead::Earlier`) are a reload's: the
    /// anchor and the focused call move into the newer read through the
    /// relocation; a reader at the bottom stays at the bottom so new
    /// messages show; and messages the newer read adds or drops set the
    /// view's `CountHighlight`.
    ///
    /// Returns the rows no longer shown: the replaced ones, or `arrived`'s
    /// without a viewer.
    fn show_render(
        &mut self,
        viewport_height: usize,
        arrived: ArrivedRows,
        options: RenderOptions,
    ) -> Arc<Vec<RenderedLine>> {
        let now = self.clock().now();
        let AppMode::View(state) = &mut self.app_mode else {
            return Arc::new(arrived.into_lines());
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

        let (released, tail_rows) = state.put_arrived_rows(arrived);
        state.total_lines = state.rendered_lines.len();
        state.rows_options = Some(options);
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

        match tail_rows {
            Some(replaced) => self.update_matches_in_tail(&replaced),
            None => self.search_shown_rows_again(),
        }
        released
    }

    /// Update the shown matches, a finished search's, in the rows a tail
    /// `replaced`: the selected match stays when it starts above them, and
    /// otherwise the first match at or below the top row is selected, as
    /// after a whole render's search. A tail of more than
    /// [`TAIL_ROWS_SEARCHED_WHILE_DRAWING`] rows sends the search of the
    /// shown rows to `viewer-search`, and its matches arrive with the same
    /// selection. While a search is pending, the rows are searched again.
    fn update_matches_in_tail(&mut self, replaced: &ReplacedRows) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        if !state.shows_finished_search() {
            return self.search_shown_rows_again();
        }
        let selected_above_tail = Some(state.current_match_index).filter(|&selected| {
            state
                .search_matches
                .get(selected)
                .is_some_and(|found| found.first_row() < replaced.old.start)
        });
        if replaced.new_len > TAIL_ROWS_SEARCHED_WHILE_DRAWING {
            state.search_matches.clear();
            let arrival = match selected_above_tail {
                Some(selected) => MatchesArrival::KeepsSelected { selected },
                None => MatchesArrival::InPlace,
            };
            return self.send_view_search(arrival, false);
        }
        update_matches_in_replaced_rows(
            &mut state.search_matches,
            &state.rendered_lines,
            &state.search_query,
            replaced,
        );
        if selected_above_tail.is_none() {
            state.current_match_index =
                first_match_at_or_below(&state.search_matches, state.scroll_offset);
        }
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
                arrival: MatchesArrival::InPlace | MatchesArrival::KeepsSelected { .. },
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
    ///
    /// Inside a message whose blocks `→` focused, the blocks and a focused
    /// list's items are stops too, until the focus leaves the message.
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
        if let Some(text_block) = focus.text_block {
            if let Some(next) = state.text_stop_after(focus.message_index, text_block) {
                return Some(Focus {
                    text_block: Some(next),
                    ..focus
                });
            }
            return Self::next_stop(state, Focus::message(focus.message_index));
        }
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
        if let Some(text_block) = focus.text_block {
            if let Some(previous) = state.text_stop_before(focus.message_index, text_block) {
                return Some(Focus {
                    text_block: Some(previous),
                    ..focus
                });
            }
            return Self::prev_stop(state, Focus::message(focus.message_index));
        }
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
            call_index: Self::message_calls(state, message_index).next(),
            ..Focus::message(message_index)
        }
    }

    /// The same from below, so one `[` undoes the `]` that left the run.
    fn last_stop_in(state: &ViewState, message_index: usize) -> Focus {
        Focus {
            call_index: Self::message_calls(state, message_index).last(),
            ..Focus::message(message_index)
        }
    }

    fn ensure_focus_visible(state: &mut ViewState, viewport_height: usize) {
        let Some(focus) = state.focus else {
            return;
        };
        let row = match (focus.call_index, state.focused_text()) {
            (Some(call), _) => state.call_ranges.get(call).map(|c| c.input.start_line),
            (None, Some(text)) => Some(text.rows.start),
            (None, None) => {
                Self::focused_message_range(state).map(|(_, message)| message.start_line)
            }
        };
        if let Some(row) = row {
            Self::ensure_line_visible(state, row, viewport_height);
        }
    }

    fn set_focused_message(state: &mut ViewState, message_index: usize) {
        if state.focused_message() != Some(message_index) {
            state.focus = Some(Focus::message(message_index));
        }
    }

    /// Focus `text_block` of the focused message and scroll its first row
    /// into view, as [`Self::focus_call`] does a call's.
    fn focus_text_block(state: &mut ViewState, text_block: TextBlockFocus, viewport_height: usize) {
        let Some(focus) = &mut state.focus else {
            return;
        };
        focus.text_block = Some(text_block);
        if let Some(text) = state.focused_text() {
            Self::ensure_line_visible(state, text.rows.start, viewport_height);
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

    /// At a message stop, `→` first expands what it expands today: a tool
    /// run, a truncated task report, reply or tool output. Once nothing is
    /// left to expand and no call to focus, it focuses the message's first
    /// block ([`ViewState::first_text_stop`]). On a list it focuses the
    /// first item.
    pub(super) fn expand_focused(&mut self, viewport_height: usize) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        if let Some(focus) = state.focus
            && let Some(text_block) = focus.text_block
        {
            if let Some(item) = state.first_item_stop(focus.message_index, text_block) {
                Self::focus_text_block(state, item, viewport_height);
            }
            return;
        }
        let ids = Self::focused_tool_output_ids(state);
        if ids.is_empty() {
            return Self::focus_first_text_block(state, viewport_height);
        }
        let message_stop_entry = match state.focused_call() {
            Some(_) => None,
            None => Self::focused_message_range(state).map(|(_, message)| message.entry_index),
        };
        if !Self::expand_all(state, ids.clone()) {
            if message_stop_entry.is_some() {
                self.focus_first_call(viewport_height);
                if let AppMode::View(state) = &mut self.app_mode {
                    Self::focus_first_text_block(state, viewport_height);
                }
            }
            return;
        }
        self.schedule_toggled_outputs_render(&ids);
        let Some(entry) = message_stop_entry else {
            return;
        };
        if !self.is_view_render_pending() {
            return self.focus_first_call(viewport_height);
        }
        if let AppMode::View(state) = &mut self.app_mode {
            // The run's calls show in the new rows; the first is focused
            // once they show.
            state.focus_first_call_of = state.entry_in_newest_read(entry);
        }
    }

    /// Focus the first block of the focused message, at its message stop in
    /// message navigation, as [`ViewState::first_text_stop`] finds it.
    fn focus_first_text_block(state: &mut ViewState, viewport_height: usize) {
        let Some(focus) = state.focus else {
            return;
        };
        if !state.message_nav_active || focus.call_index.is_some() {
            return;
        }
        if let Some(first) = state.first_text_stop(focus.message_index) {
            Self::focus_text_block(state, first, viewport_height);
        }
    }

    /// `←` moves the focus from an item to its list, or to its message when
    /// the list is the message's only block, and from a block to its
    /// message; elsewhere it collapses as it does today.
    pub(super) fn collapse_focused(&mut self) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        if let Some(focus) = state.focus
            && let Some(text_block) = focus.text_block
        {
            state.focus = Some(Focus {
                text_block: state.text_stop_outward(focus.message_index, text_block),
                ..focus
            });
            return;
        }
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
    /// a focused block's clickable rows (a truncated reply's), otherwise the
    /// focused message's own rows, in `parsed_conversation`.
    fn focused_tool_output_ids(state: &ViewState) -> BTreeSet<ToolOutputId> {
        let shown_ids = match (Self::focused_call_range(state), state.focused_text()) {
            (Some(call), _) => call
                .areas()
                .filter(|area| Self::is_expandable(state, area))
                .map(|area| area.id.clone())
                .collect(),
            (None, Some(text)) => state.rendered_lines[text.rows]
                .iter()
                .filter(|line| line.clickable)
                .filter_map(|line| line.tool_output_id.clone())
                .collect(),
            (None, None) => Self::focused_message_tool_output_ids(state),
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
            && (focus.call_index.is_some() || focus.text_block.is_some())
        {
            *focus = Focus::message(focus.message_index);
            changed = true;
        }
        if let Some(message_index) = message_idx
            && (!state.message_nav_active || state.focused_message() != Some(message_index))
        {
            state.message_nav_active = true;
            state.focus = Some(Focus::message(message_index));
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
        let text_block = Self::clamped_text_block(state, &viewport, message_index);
        state.focus = Some(Focus {
            message_index,
            call_index,
            text_block,
        });
    }

    /// The block or item inside `message_index` the focus lands on: the
    /// focused one while it is on screen. A focused item or block that
    /// drifted off an edge moves to the on-screen item of its list nearest
    /// that edge, otherwise to the nearest on-screen block. `None` when the
    /// focus moved to another message, or when no block of the message is on
    /// screen.
    fn clamped_text_block(
        state: &ViewState,
        viewport: &std::ops::Range<usize>,
        message_index: usize,
    ) -> Option<TextBlockFocus> {
        let mut focused = state
            .focus
            .filter(|focus| focus.message_index == message_index)?
            .text_block?;
        let blocks = state.text_blocks_of(message_index);
        let block = blocks.get(focused.block)?;
        let is_on_screen =
            |rows: &std::ops::Range<usize>| FocusDrift::of(rows.clone(), viewport).is_on_screen();
        if let Some(item) = focused.item {
            let items = block.items(&state.rendered_lines);
            let landed = items
                .iter()
                .find(|shown| shown.item == item)
                .map(|shown| FocusDrift::of(shown.rows.clone(), viewport))
                .and_then(|drift| {
                    let on_screen = items
                        .iter()
                        .filter(|shown| is_on_screen(&shown.rows))
                        .map(|shown| shown.item);
                    drift.clamp(Some(item), on_screen)
                });
            if landed.is_some() {
                return Some(TextBlockFocus {
                    item: landed,
                    ..focused
                });
            }
            focused = list_of_item(&blocks, focused)?;
        }
        let on_screen = blocks
            .iter()
            .enumerate()
            .filter(|(_, shown)| is_on_screen(&shown.rows))
            .map(|(index, _)| index);
        FocusDrift::of(block.rows.clone(), viewport)
            .clamp(Some(focused.block), on_screen)
            .map(|block| TextBlockFocus { block, item: None })
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
                self.set_status(NO_TEXT_TO_COPY.to_owned());
            }
            Some(text) => self.copy_to_clipboard("Message", &text),
            None => self.set_status("Message not found".to_owned()),
        }
    }

    /// Copy the focused call, block or message; while the open session's
    /// refresh is pending, once it finishes, so the copy holds the newest
    /// messages.
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
        } else if !self.copy_focused_text() {
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

    /// Render the viewer's rows again, or the pending open's, for a frame
    /// `frame_width` columns wide when they were requested for another width.
    pub fn check_view_resize(&mut self, frame_width: usize) {
        let (now, today, zone) = (self.clock().now(), self.today(), self.clock().zone());
        let state = match &mut self.app_mode {
            AppMode::View(state) => Some(state),
            AppMode::List => self
                .pending_open
                .as_mut()
                .and_then(super::PendingOpen::opening_view_mut),
        };
        let Some(state) = state.filter(|state| state.frame_width != frame_width) else {
            return;
        };
        state.frame_width = frame_width;
        let options = state.render_options(today, zone);
        request_render(
            &mut self.viewer_render,
            state,
            options,
            RowsToRender::Whole,
            now,
        );
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

/// Ask `worker`, the `viewer-render` thread, started by the first request,
/// for a render of the rows `rows_to_render` selects of `state`'s
/// conversation with `options`, made at `now`, superseding the running
/// render.
fn request_render(
    worker: &mut Option<super::viewer_render::ViewerRenderWorker>,
    state: &mut ViewState,
    options: RenderOptions,
    rows_to_render: RowsToRender,
    now: crate::clock::Moment,
) {
    let generation = worker
        .get_or_insert_with(super::viewer_render::ViewerRenderWorker::spawn)
        .render(
            Arc::clone(&state.parsed_conversation),
            options,
            rows_to_render,
        );
    debug_assert!(
        rows_to_render == RowsToRender::Whole
            || (state.entries_to_render_again.is_empty()
                && state.run_outputs_to_render_again.is_empty()),
        "a tail is requested while no message waits to render again"
    );
    // The render has every expanded output in its options.
    state.entries_to_render_again.clear();
    state.run_outputs_to_render_again.clear();
    match &mut state.pending_render {
        Some(pending) => pending.generation = generation,
        None => {
            state.pending_render = Some(PendingRender {
                generation,
                since: now,
            });
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
/// screen, and the focused call or block, to find again in new rows.
struct ViewPlace {
    anchor: Option<ScrollAnchor>,
    focused_call_id: Option<ToolOutputId>,
    focused_text: Option<PlacedTextBlock>,
    scroll_offset: usize,
}

/// A focused block, and the texts of its message. The block stays focused
/// in the new rows when the message shows these texts unchanged there.
struct PlacedTextBlock {
    text_block: TextBlockFocus,
    message_texts: Vec<Arc<str>>,
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
        let focused_text = state.focus.and_then(|focus| {
            Some(PlacedTextBlock {
                text_block: focus.text_block?,
                message_texts: state.message_texts(focus.message_index),
            })
        });
        Self {
            anchor,
            focused_call_id,
            focused_text,
            scroll_offset: state.scroll_offset,
        }
    }

    /// Focus the anchor message and the focused call in `state`'s new rows,
    /// and scroll the anchor message back to its row on screen. The focused
    /// block stays focused, as far as the new rows show it, when the
    /// focused message's text is unchanged; otherwise the message is.
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
        let resolved_text = |message_index: usize| {
            let placed = self.focused_text.as_ref()?;
            (state.message_texts(message_index) == placed.message_texts)
                .then(|| state.shown_text_block(message_index, placed.text_block))
                .flatten()
        };
        let focus = resolved_idx.map(|message_index| Focus {
            message_index,
            call_index: resolved_call,
            text_block: resolved_text(message_index),
        });
        state.focus = focus;

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
