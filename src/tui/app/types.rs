use crate::history::provider::walk::Canonicaliser;
use crate::history::{LoadProgress, OtherRearview, Workspace};
use crate::search::query::ParsedQuery;
use crate::semantic::types::{SemanticExplanation, SemanticScoreBreakdown};
use crate::tui::actions_menu::{MenuAvailability, SessionAction};
use crate::tui::viewer::{
    CallRange, EntryRelocation, MessageRange, ParsedConversation, RenderedLine, SearchMatch,
    ToolDisplayMode, ToolOutputId,
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

/// Result of running the TUI
pub enum Action {
    Select(PathBuf),
    Delete(PathBuf),
    Resume(PathBuf),
    ForkResume(PathBuf),
    /// Refresh the list from the sessions on disk; the TUI handles it and
    /// keeps running.
    Refresh,
    Quit,
}

/// Dialog overlay mode (for confirmations, menus)
#[derive(Clone, Debug, PartialEq)]
pub enum DialogMode {
    /// No dialog shown
    None,
    /// Confirming deletion of the selected conversation
    ConfirmDelete,
    /// Export menu (save to file)
    ExportMenu { selected: usize },
    /// Yank menu (copy to clipboard)
    YankMenu { selected: usize },
    /// Help overlay showing keyboard shortcuts
    Help { scroll: usize },
    /// Semantic result debug details
    SemanticDebug,
    /// The filters the load ran under
    ActiveFilters,
    /// Rename the selected conversation
    Rename { input: String, cursor: usize },
    /// The `Ctrl+X` actions menu; `selected` is the row `▶` marks, `None`
    /// when no action is available
    ActionsMenu {
        selected: Option<SessionAction>,
        availability: MenuAvailability,
    },
}

/// Main application mode
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum AppMode {
    /// List mode - browsing conversations
    List,
    /// View mode - reading a conversation
    View(ViewState),
}

/// The open session's pending refresh, as the viewer labels it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenSessionStatus {
    /// The session is not in the session cache yet.
    Loading,
    /// The viewer shows an out-of-date copy.
    Updating,
    /// The refresh waits for another rearview's activity.
    AnotherRearview(OtherRearview),
}

/// Where message navigation points. The call lives inside the message, so
/// replacing the message drops the call with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Focus {
    /// Index into `ViewState::message_ranges`.
    pub message_index: usize,
    /// Index into the whole of `ViewState::call_ranges`, not a position among
    /// the message's own calls. `None` while the message itself is focused,
    /// which is always so outside an expanded tool run.
    pub call_index: Option<usize>,
}

/// State for the conversation viewer
#[derive(Clone, Debug)]
pub struct ViewState {
    /// Path to the conversation file (stable identity)
    pub conversation_path: PathBuf,
    /// The agent that recorded the conversation. A session refresh of a file
    /// opened directly that the list doesn't show reads the file as this
    /// agent's. `None` for a file opened directly until the background worker
    /// reads it.
    pub conversation_source: Option<crate::history::Source>,
    /// The conversation's address in the session cache. Re-reads and exports
    /// read its newest copy through it. `None` for a file opened directly
    /// until the background worker lists it, or one outside every root.
    pub cache_address: Option<crate::history::sqlite_cache::SessionAddress>,
    /// The session ID as this conversation's provider recorded it. `None` when
    /// the transcript did not parse; a file name is not guaranteed to be the
    /// session ID.
    pub session_id: Option<String>,
    /// The open session's row's publication of its copy. It decides whether
    /// the view and its exports read a copy as current.
    pub publication: crate::history::sqlite_cache::Publication,
    /// The header's fields, and the source of its height: the catalogue row
    /// of the copy the view shows, or the open session's row while the view
    /// doesn't show a copy. `None` shows the file name alone.
    pub header: Option<HeaderRow>,
    /// The open view's conversation as parsed; empty until a file opened
    /// directly is read.
    pub parsed_conversation: Arc<ParsedConversation>,
    /// Current scroll position (line offset)
    pub scroll_offset: usize,
    /// Pre-rendered conversation lines, shared with the viewer's search
    pub rendered_lines: Arc<Vec<RenderedLine>>,
    /// Total content height in lines
    pub total_lines: usize,
    /// Tool display mode (hidden/truncated/full)
    pub tool_display: ToolDisplayMode,
    /// Whether to show thinking blocks
    pub show_thinking: bool,
    /// Whether to show timing information (timestamps + durations)
    pub show_timing: bool,
    /// The frame width the rows were rendered for; a frame of another width
    /// renders them again
    pub frame_width: usize,
    /// Search mode state
    pub search_mode: ViewSearchMode,
    /// Current search query
    pub search_query: String,
    /// The search's matches, top to bottom
    pub search_matches: Vec<SearchMatch>,
    /// Index into `search_matches` of the match `n`/`N` are sitting on
    pub current_match_index: usize,
    /// The search of `search_query` whose matches haven't arrived yet
    pub pending_search: Option<PendingViewSearch>,
    /// Message boundary ranges from rendering
    pub message_ranges: Vec<MessageRange>,
    /// The calls inside expanded tool runs, in line order
    pub call_ranges: Vec<CallRange>,
    /// Where message navigation points, `None` until a message is rendered
    pub focus: Option<Focus>,
    /// Whether message navigation mode is active (shows gutter indicator)
    pub message_nav_active: bool,
    /// Tool outputs expanded independently from global tool display mode
    pub expanded_tool_outputs: BTreeSet<ToolOutputId>,
    /// Tool output currently under the mouse cursor
    pub hovered_tool_output: Option<ToolOutputId>,
    /// How the view marks a refresh that changed the message count. Each such
    /// refresh replaces it.
    pub count_highlight: Option<CountHighlight>,
    /// True while the session is missing from the session cache, so the
    /// view doesn't show any messages.
    pub is_copy_missing: bool,
    /// The read of the conversation the rows were rendered from.
    pub rows_read: RowsRead,
}

/// The read of the open conversation that the viewer's rows show.
#[derive(Clone, Debug)]
pub enum RowsRead {
    /// `ViewState::parsed_conversation`.
    Current,
    /// A read before `ViewState::parsed_conversation`: a reload's read has
    /// arrived, and its rows have not. The relocation finds the rows'
    /// entries in the newer read.
    Earlier(EntryRelocation),
    /// The empty conversation of a file opened directly, before its first
    /// read shows from the top.
    BeforeFirstRead,
}

/// The fields of a session's row that the view's header shows. The row's
/// text stays with the row.
#[derive(Clone, Debug)]
pub struct HeaderRow {
    pub project_name: Option<String>,
    pub custom_title: Option<String>,
    pub summary: Option<String>,
    pub model: Option<String>,
    pub message_count: usize,
    pub duration_minutes: Option<u64>,
    pub total_tokens: u64,
    pub timestamp: chrono::DateTime<chrono::Local>,
}

impl HeaderRow {
    pub fn of(row: &crate::history::Conversation) -> Self {
        Self {
            project_name: row.project_name.clone(),
            custom_title: row.custom_title.clone(),
            summary: row.summary.clone(),
            model: row.model.clone(),
            message_count: row.message_count,
            duration_minutes: row.duration_minutes,
            total_tokens: row.total_tokens,
            timestamp: row.timestamp,
        }
    }

    /// The header of `copy_row`, a copy's catalogue row, showing `title`,
    /// the title stored beside the transcript, in place of its parse's.
    pub fn of_copy(
        mut copy_row: crate::history::Conversation,
        title: Option<&crate::history::provider::SessionTitle>,
    ) -> Self {
        if let Some(title) = title {
            crate::history::provider::apply_external_title(&mut copy_row, title);
        }
        Self::of(&copy_row)
    }
}

/// A refresh's change in the session's message count, marked until `until`:
/// the total in the bottom bar's line count shows in the session ID color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CountHighlight {
    pub until: crate::clock::Moment,
    /// Messages the refresh added below a reader at the bottom. While the view
    /// stays at the bottom, a `↓ N new` badge covers the start of the last
    /// content row.
    pub arrived_below: Option<usize>,
}

/// The viewer's search of its query, until its matches arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingViewSearch {
    /// The query changed; the search starts at `due`, after the pause, unless
    /// the query changes again.
    Paused { due: crate::clock::Moment },
    /// The search runs on the viewer's search thread since `started`.
    Running {
        /// The number marking the newest search: matches of any other search
        /// aren't shown.
        generation: u64,
        started: crate::clock::Moment,
        arrival: MatchesArrival,
        /// True after confirm (`Enter`) while the search was paused or
        /// running: the search closes when it doesn't find any match.
        closes_without_matches: bool,
    },
}

/// Where the view goes when a search's matches arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchesArrival {
    /// The view stays where it is, on the first match at or below its top
    /// row, as for the search of rows rendered again.
    InPlace,
    /// The view scrolls to the first match, unless it moved from `from` while
    /// the search ran.
    AtFirstMatch { from: ViewSpot },
}

/// The view's scroll position and focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewSpot {
    pub scroll_offset: usize,
    pub focus: Option<Focus>,
}

impl ViewSpot {
    pub fn of(state: &ViewState) -> Self {
        Self {
            scroll_offset: state.scroll_offset,
            focus: state.focus,
        }
    }
}

/// Search mode within view
#[derive(Clone, Debug, PartialEq, Default)]
pub enum ViewSearchMode {
    #[default]
    Off,
    /// Typing search query
    Typing,
    /// Search active, navigating results
    Active,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TuiSearchOptions {
    pub default_mode: ListSearchMode,
}

/// The sessions a list starts out showing.
#[derive(Default)]
pub struct ListScope {
    /// True when the list starts filtered to `workspace` (`--local`).
    pub workspace_filter: bool,
    /// The workspace the filter narrows to; `None` when the current
    /// directory could not be read.
    pub workspace: Option<Workspace>,
    /// Projects whose sessions the list leaves out.
    pub exclude_projects: Vec<String>,
}

/// A file the user opened directly (`rearview <file>`).
#[derive(Clone, Debug)]
pub struct DirectFile {
    /// The path as the user typed it, for the viewer's path display (`p`)
    /// and path copy (`Y`).
    pub typed: PathBuf,
    /// The path the cache finds the session's copy by.
    pub canonical: PathBuf,
}

impl DirectFile {
    /// The file at `typed`, canonicalised through `canonicaliser`. Fails when
    /// the path doesn't canonicalise.
    pub fn canonicalise(
        typed: PathBuf,
        canonicaliser: &dyn Canonicaliser,
    ) -> std::io::Result<Self> {
        let canonical = canonicaliser.canonicalise(&typed)?;
        Ok(Self { typed, canonical })
    }
}

impl Default for TuiSearchOptions {
    fn default() -> Self {
        Self {
            default_mode: ListSearchMode::Lexical,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ListSearchMode {
    #[default]
    Lexical,
    Semantic,
}

impl ViewState {
    /// The view before its first render: an empty conversation, timing off,
    /// no frame width yet.
    pub(super) fn initial(
        conversation_path: PathBuf,
        conversation_source: Option<crate::history::Source>,
        session_id: Option<String>,
        tool_display: ToolDisplayMode,
        show_thinking: bool,
    ) -> Self {
        Self {
            conversation_path,
            conversation_source,
            cache_address: None,
            session_id,
            publication: crate::history::sqlite_cache::Publication::Unpublished {
                discovered: Vec::new(),
            },
            header: None,
            parsed_conversation: Arc::new(ParsedConversation::empty()),
            scroll_offset: 0,
            rendered_lines: Arc::default(),
            total_lines: 0,
            tool_display,
            show_thinking,
            show_timing: false,
            frame_width: 0,
            search_mode: ViewSearchMode::Off,
            search_query: String::new(),
            search_matches: Vec::new(),
            current_match_index: 0,
            pending_search: None,
            message_ranges: Vec::new(),
            call_ranges: Vec::new(),
            focus: None,
            message_nav_active: false,
            expanded_tool_outputs: BTreeSet::new(),
            hovered_tool_output: None,
            count_highlight: None,
            is_copy_missing: false,
            rows_read: RowsRead::Current,
        }
    }

    /// True when the last row is on screen, as after the view sticks to the
    /// bottom.
    pub fn is_at_bottom(&self, viewport_height: usize) -> bool {
        self.scroll_offset >= self.total_lines.saturating_sub(viewport_height)
    }

    /// The width every wrap uses: the frame minus the ledger columns, which
    /// hold the timestamp column while timing is shown.
    pub fn content_width(&self) -> usize {
        crate::tui::viewer::content_width(self.frame_width, self.show_timing)
    }

    pub fn focused_message(&self) -> Option<usize> {
        self.focus.map(|focus| focus.message_index)
    }

    pub fn focused_call(&self) -> Option<usize> {
        self.focus.and_then(|focus| focus.call_index)
    }
}

impl ListSearchMode {
    pub fn label(self) -> &'static str {
        match self {
            ListSearchMode::Lexical => "lex",
            ListSearchMode::Semantic => "sem",
        }
    }
}

pub const LIST_LINES_PER_ITEM: usize = 3;

pub fn list_lines_per_item(_mode: ListSearchMode, query: &str) -> usize {
    let parsed = ParsedQuery::parse(query);
    if !parsed.literals().is_empty() {
        4
    } else {
        LIST_LINES_PER_ITEM
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum SemanticProgress {
    #[default]
    Idle,
    InitializingModel,
    CacheReady,
    Embedding {
        completed: usize,
        total: usize,
    },
    Ranking,
    Complete,
    EmptyCorpus,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SemanticResultMetadata {
    pub score_breakdown: SemanticScoreBreakdown,
    pub explanation: SemanticExplanation,
}

/// Loading state for the TUI
#[derive(Clone, Debug)]
pub enum LoadingState {
    /// Still loading: `loaded` rows so far, and how far the loader has got
    /// through the source it is on, once it has said.
    Loading {
        loaded: usize,
        progress: Option<LoadProgress>,
    },
    /// All conversations loaded and ready
    Ready,
}
