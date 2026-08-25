use crate::config::KeyBindings;
use crate::history::{
    Conversation, FilterTerm, LoadProgress, Workspace, format_short_name_from_path,
    process_conversation_file,
};
use crate::search::{self, SearchableConversation};
#[cfg(test)]
use crate::semantic::types::{SemanticExplanation, SemanticScoreBreakdown};
use crate::tui::export::{ClipboardDestination, copy_to_system_clipboard};
#[cfg(test)]
use crate::tui::semantic_worker::{SemanticSearchMessage, SemanticWorkerCommand};
#[cfg(test)]
use crate::tui::ui;
use crate::tui::viewer::ToolDisplayMode;
#[cfg(test)]
use crate::tui::viewer::ToolOutputId;
#[cfg(test)]
use chrono::Local;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

mod dialog_state;
mod input_controller;
mod list_state;
mod search_state;
mod types;
mod view_state;
#[allow(unused_imports)]
use search_state::{SearchCommand, SearchResponse, SemanticSearchState, spawn_search_worker};
#[allow(unused_imports)]
pub use types::{
    Action, AppMode, DialogMode, Focus, LIST_LINES_PER_ITEM, ListSearchMode, LoadingState,
    SemanticProgress, SemanticResultMetadata, TuiSearchOptions, ViewSearchMode, ViewState,
    list_lines_per_item,
};

/// App state
pub struct App {
    /// All loaded conversations
    conversations: Vec<Conversation>,
    /// Shared search data snapshot for background workers
    conversations_snapshot: Arc<Vec<Conversation>>,
    /// Shared semantic conversation snapshot for background workers
    semantic_conversations_snapshot: Arc<Vec<Arc<Conversation>>>,
    /// Version of the semantic corpus snapshot
    semantic_corpus_version: u64,
    /// Version of the semantic scope snapshot
    semantic_scope_version: u64,
    /// Last corpus version sent to the semantic worker
    semantic_sent_corpus_version: u64,
    /// Last scope signature sent to the semantic worker
    semantic_sent_scope_signature: Option<(u64, Arc<Vec<usize>>)>,
    /// Precomputed search data
    searchable: Vec<SearchableConversation>,
    /// Indices into conversations, sorted by current score
    filtered: Vec<usize>,
    /// Currently selected index into filtered (None if no results)
    selected: Option<usize>,
    /// Current search query
    query: String,
    /// Cursor position in query (character index, not byte)
    cursor_pos: usize,
    /// Loading state
    loading_state: LoadingState,
    /// Current dialog overlay (confirm, menu)
    dialog_mode: DialogMode,
    /// Main app mode (list or view)
    app_mode: AppMode,
    /// Status message with timestamp for auto-clear
    status_message: Option<(String, std::time::Instant)>,
    /// Persistent view setting: tool display mode
    tool_display: ToolDisplayMode,
    /// Persistent view setting: whether to show thinking blocks
    show_thinking: bool,
    /// Persistent view setting: whether to show timing information
    show_timing: bool,
    /// Whether the app is running in single file mode (direct input, no list)
    single_file_mode: bool,
    /// Configurable keybindings
    keys: KeyBindings,
    /// Whether workspace filter is active (only show current project's conversations)
    workspace_filter: bool,
    /// `None` when the current directory could not be read, which disables the
    /// workspace filter
    workspace: Option<Workspace>,
    /// Exact project names hidden from list-mode display
    excluded_projects: HashSet<String>,
    /// Channel to send commands to the background search worker
    search_tx: mpsc::Sender<SearchCommand>,
    /// Channel to receive results from the background search worker
    search_rx: mpsc::Receiver<SearchResponse>,
    /// Monotonic generation counter for search requests
    search_generation: u64,
    /// Whether a search is currently in-flight on the worker thread
    search_in_flight: bool,
    /// Current list search mode
    list_search_mode: ListSearchMode,
    /// Semantic TUI state
    semantic_search: SemanticSearchState,
    /// Cached lexical evidence produced outside the render path
    lexical_evidence: HashMap<usize, search::LexicalEvidence>,
    /// The session id the query named, when no agent stores it
    unresolved_session_id: Option<String>,
    /// The load filters that narrowed this list, named for the user
    active_filters: Vec<FilterTerm>,
    /// The clipboard that copy keys and clipboard export write to.
    clipboard_writer: ClipboardWriter,
}

type ClipboardWriter = fn(&str) -> Result<ClipboardDestination, String>;

struct AppParts {
    conversations: Vec<Conversation>,
    conversations_snapshot: Arc<Vec<Conversation>>,
    semantic_conversations_snapshot: Arc<Vec<Arc<Conversation>>>,
    searchable: Vec<SearchableConversation>,
    filtered: Vec<usize>,
    selected: Option<usize>,
    loading_state: LoadingState,
    app_mode: AppMode,
    tool_display: ToolDisplayMode,
    show_thinking: bool,
    single_file_mode: bool,
    keys: KeyBindings,
    workspace_filter: bool,
    workspace: Option<Workspace>,
    excluded_projects: HashSet<String>,
    search_tx: mpsc::Sender<SearchCommand>,
    search_rx: mpsc::Receiver<SearchResponse>,
    list_search_mode: ListSearchMode,
    semantic_search: SemanticSearchState,
}

impl App {
    fn from_parts(parts: AppParts) -> Self {
        Self {
            conversations_snapshot: parts.conversations_snapshot,
            semantic_conversations_snapshot: parts.semantic_conversations_snapshot,
            semantic_corpus_version: 1,
            semantic_scope_version: 0,
            semantic_sent_corpus_version: 0,
            semantic_sent_scope_signature: None,
            conversations: parts.conversations,
            searchable: parts.searchable,
            filtered: parts.filtered,
            selected: parts.selected,
            query: String::new(),
            cursor_pos: 0,
            loading_state: parts.loading_state,
            dialog_mode: DialogMode::None,
            app_mode: parts.app_mode,
            status_message: None,
            tool_display: parts.tool_display,
            show_thinking: parts.show_thinking,
            show_timing: false,
            single_file_mode: parts.single_file_mode,
            keys: parts.keys,
            workspace_filter: parts.workspace_filter,
            workspace: parts.workspace,
            excluded_projects: parts.excluded_projects,
            search_tx: parts.search_tx,
            search_rx: parts.search_rx,
            search_generation: 0,
            search_in_flight: false,
            list_search_mode: parts.list_search_mode,
            semantic_search: parts.semantic_search,
            lexical_evidence: HashMap::new(),
            unresolved_session_id: None,
            active_filters: Vec::new(),
            clipboard_writer: copy_to_system_clipboard,
        }
    }

    /// Copy `text`, reporting the outcome as a status message that names
    /// `subject` — "Path copied to clipboard", and so on.
    fn copy_to_clipboard(&mut self, subject: &str, text: &str) {
        let outcome = match (self.clipboard_writer)(text) {
            Ok(ClipboardDestination::System) => format!("{subject} copied to clipboard"),
            Ok(ClipboardDestination::Terminal) => format!("{subject} sent to terminal clipboard"),
            Err(error) => error,
        };
        self.set_status(outcome);
    }

    fn set_status(&mut self, message: String) {
        self.status_message = Some((message, std::time::Instant::now()));
    }

    fn conversation_snapshot(conversations: &[Conversation]) -> Arc<Vec<Conversation>> {
        Arc::new(conversations.to_vec())
    }

    fn semantic_snapshot(conversations: &[Conversation]) -> Arc<Vec<Arc<Conversation>>> {
        Arc::new(conversations.iter().cloned().map(Arc::new).collect())
    }

    fn send_initial_search_data(
        search_tx: &mpsc::Sender<SearchCommand>,
        conversations: Arc<Vec<Conversation>>,
        searchable: &[SearchableConversation],
    ) {
        let _ = search_tx.send(SearchCommand::UpdateData {
            conversations,
            searchable: Arc::new(searchable.to_vec()),
        });
    }

    fn list_search_mode_from_options(search_options: TuiSearchOptions) -> ListSearchMode {
        search_options.default_mode
    }

    fn semantic_search_state(available: bool) -> SemanticSearchState {
        SemanticSearchState {
            available,
            ..Default::default()
        }
    }

    /// Create a new app with all conversations pre-loaded
    #[allow(dead_code)]
    pub fn new(
        conversations: Vec<Conversation>,
        tool_display: ToolDisplayMode,
        show_thinking: bool,
        keys: KeyBindings,
        exclude_projects: Vec<String>,
    ) -> Self {
        Self::new_with_options(
            conversations,
            tool_display,
            show_thinking,
            keys,
            exclude_projects,
            TuiSearchOptions::default(),
        )
    }

    #[allow(dead_code)]
    pub fn new_with_options(
        conversations: Vec<Conversation>,
        tool_display: ToolDisplayMode,
        show_thinking: bool,
        keys: KeyBindings,
        exclude_projects: Vec<String>,
        search_options: TuiSearchOptions,
    ) -> Self {
        let searchable = search::precompute_search_text(&conversations);
        let excluded_projects = exclude_projects.into_iter().collect();
        let filtered = list_state::filter_conversation_indices(
            0..conversations.len(),
            &conversations,
            &excluded_projects,
            false,
            None,
        );
        let selected = if filtered.is_empty() { None } else { Some(0) };
        let (search_tx, search_rx) = spawn_search_worker();
        let conversations_snapshot = Self::conversation_snapshot(&conversations);

        Self::send_initial_search_data(&search_tx, conversations_snapshot.clone(), &searchable);

        Self::from_parts(AppParts {
            conversations_snapshot,
            semantic_conversations_snapshot: Self::semantic_snapshot(&conversations),
            conversations,
            searchable,
            filtered,
            selected,
            loading_state: LoadingState::Ready,
            app_mode: AppMode::List,
            tool_display,
            show_thinking,
            single_file_mode: false,
            keys,
            workspace_filter: false,
            workspace: None,
            excluded_projects,
            search_tx,
            search_rx,
            list_search_mode: Self::list_search_mode_from_options(search_options),
            semantic_search: Self::semantic_search_state(true),
        })
    }

    /// Create a new app in loading state
    pub fn new_loading_with_options(
        tool_display: ToolDisplayMode,
        show_thinking: bool,
        keys: KeyBindings,
        workspace_filter: bool,
        workspace: Option<Workspace>,
        exclude_projects: Vec<String>,
        search_options: TuiSearchOptions,
    ) -> Self {
        let conversations = Vec::new();
        let (search_tx, search_rx) = spawn_search_worker();

        Self::from_parts(AppParts {
            conversations_snapshot: Self::conversation_snapshot(&conversations),
            semantic_conversations_snapshot: Self::semantic_snapshot(&conversations),
            conversations,
            searchable: Vec::new(),
            filtered: Vec::new(),
            selected: None,
            loading_state: LoadingState::Loading {
                loaded: 0,
                progress: None,
            },
            app_mode: AppMode::List,
            tool_display,
            show_thinking,
            single_file_mode: false,
            keys,
            workspace_filter,
            workspace,
            excluded_projects: exclude_projects.into_iter().collect(),
            search_tx,
            search_rx,
            list_search_mode: Self::list_search_mode_from_options(search_options),
            semantic_search: Self::semantic_search_state(true),
        })
    }

    /// Create a new app for viewing a single file directly
    pub fn new_single_file(
        path: PathBuf,
        tool_display: ToolDisplayMode,
        show_thinking: bool,
        keys: KeyBindings,
    ) -> Self {
        let (search_tx, search_rx) = spawn_search_worker();

        let parsed = parse_single_file(&path);

        // The registry parse above already attributed the file; a file it did
        // not claim is read as a raw Claude transcript, as everywhere else.
        let source = parsed
            .as_ref()
            .map(|conversation| conversation.source)
            .unwrap_or(crate::history::Source::Claude);

        // A file that parsed into nothing has no session ID to report.
        let session_id = parsed
            .as_ref()
            .map(|conversation| conversation.session_id.clone());

        let selected = parsed.is_some().then_some(0);
        let filtered = Vec::from_iter(selected);
        let conversations = Vec::from_iter(parsed);

        Self::from_parts(AppParts {
            conversations_snapshot: Self::conversation_snapshot(&conversations),
            semantic_conversations_snapshot: Self::semantic_snapshot(&conversations),
            conversations,
            searchable: Vec::new(),
            filtered,
            selected,
            loading_state: LoadingState::Ready,
            app_mode: AppMode::View(ViewState::initial(
                path,
                source,
                session_id,
                tool_display,
                show_thinking,
                false,
                0,
            )),
            tool_display,
            show_thinking,
            single_file_mode: true,
            keys,
            workspace_filter: false,
            workspace: None,
            excluded_projects: HashSet::new(),
            search_tx,
            search_rx,
            list_search_mode: ListSearchMode::Lexical,
            semantic_search: Self::semantic_search_state(false),
        })
    }

    pub fn keys(&self) -> &KeyBindings {
        &self.keys
    }

    /// Append a batch of conversations during loading
    /// Note: Does NOT precompute search text - that's deferred to finish_loading
    pub fn append_conversations(&mut self, new_convs: Vec<Conversation>) {
        let start_idx = self.conversations.len();
        self.conversations.extend(new_convs);
        let end_idx = self.conversations.len();

        let new_filtered = self.filter_indices(start_idx..end_idx);
        self.filtered.extend(new_filtered);

        // Select first item if nothing selected yet
        if self.selected.is_none() && !self.filtered.is_empty() {
            self.selected = Some(0);
        }

        if let LoadingState::Loading { loaded, .. } = &mut self.loading_state {
            *loaded = self.conversations.len();
        }
    }

    /// Record how far the loader has got through the source it is on.
    pub fn report_load_progress(&mut self, progress: LoadProgress) {
        if let LoadingState::Loading {
            progress: current, ..
        } = &mut self.loading_state
        {
            *current = Some(progress);
        }
    }

    /// Mark loading as complete: sort, precompute search, and transition to Ready
    pub fn finish_loading(&mut self) {
        // Sort all conversations by timestamp (newest first)
        self.conversations
            .sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        // Reindex after sorting
        for (idx, conv) in self.conversations.iter_mut().enumerate() {
            conv.index = idx;
        }

        self.conversations_snapshot = Arc::new(self.conversations.clone());
        self.rebuild_semantic_conversations_snapshot();

        // Now precompute search text (only once, at the end)
        self.searchable = search::precompute_search_text(&self.conversations);

        // Send data snapshot to the background search worker
        let _ = self.search_tx.send(SearchCommand::UpdateData {
            conversations: self.conversations_snapshot.clone(),
            searchable: Arc::new(self.searchable.clone()),
        });

        self.loading_state = LoadingState::Ready;

        self.invalidate_search_generation();

        // Apply filter (handles query, exclusions, and workspace filter)
        self.update_filter();
        if self.list_search_mode == ListSearchMode::Semantic && !self.query.trim().is_empty() {
            self.dispatch_search();
        } else {
            self.prewarm_semantic_cache();
        }
    }

    /// Consume the app and return its conversations
    pub fn into_conversations(self) -> Vec<Conversation> {
        self.conversations
    }

    pub fn loading_state(&self) -> &LoadingState {
        &self.loading_state
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.loading_state, LoadingState::Loading { .. })
    }

    // Getters for UI access
    pub fn filtered(&self) -> &[usize] {
        &self.filtered
    }

    pub fn conversations(&self) -> &[Conversation] {
        &self.conversations
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn dialog_mode(&self) -> &DialogMode {
        &self.dialog_mode
    }

    #[cfg(test)]
    pub fn set_dialog_mode_for_test(&mut self, dialog_mode: DialogMode) {
        self.dialog_mode = dialog_mode;
    }

    #[cfg(test)]
    pub fn set_clipboard_writer_for_test(&mut self, clipboard_writer: ClipboardWriter) {
        self.clipboard_writer = clipboard_writer;
    }

    pub fn app_mode(&self) -> &AppMode {
        &self.app_mode
    }

    pub fn status_message(&self) -> Option<&(String, std::time::Instant)> {
        self.status_message.as_ref()
    }

    pub fn unresolved_session_id(&self) -> Option<&str> {
        self.unresolved_session_id.as_deref()
    }

    /// Name the filters the load ran under, so a list that holds less than the
    /// user expects can say why.
    pub fn set_active_filters(&mut self, filters: Vec<FilterTerm>) {
        self.active_filters = filters;
    }

    /// A term the load reports — sessions a provider ignores — listed after
    /// the filters it started under.
    pub fn add_active_filter(&mut self, filter: FilterTerm) {
        self.active_filters.push(filter);
    }

    pub fn active_filters(&self) -> &[FilterTerm] {
        &self.active_filters
    }

    /// Returns how long until the active status message expires, if any
    pub fn status_message_remaining(&self) -> Option<Duration> {
        const STATUS_TTL: Duration = Duration::from_secs(3);
        self.status_message
            .as_ref()
            .and_then(|(_, instant)| STATUS_TTL.checked_sub(instant.elapsed()))
    }

    pub fn cursor_pos(&self) -> usize {
        self.cursor_pos
    }

    pub fn is_single_file_mode(&self) -> bool {
        self.single_file_mode
    }

    pub fn workspace_filter(&self) -> bool {
        self.workspace_filter
    }

    pub fn list_search_mode(&self) -> ListSearchMode {
        self.list_search_mode
    }

    pub fn semantic_search_available(&self) -> bool {
        self.semantic_search.available
    }

    pub fn has_project_context(&self) -> bool {
        self.workspace.is_some()
    }

    pub fn semantic_toggle_available(&self) -> bool {
        self.semantic_search.available
            && !self
                .keys
                .rename
                .matches(KeyCode::Char('t'), KeyModifiers::CONTROL)
            && !self
                .keys
                .delete
                .matches(KeyCode::Char('t'), KeyModifiers::CONTROL)
            && !self
                .keys
                .resume
                .matches(KeyCode::Char('t'), KeyModifiers::CONTROL)
            && !self
                .keys
                .fork
                .matches(KeyCode::Char('t'), KeyModifiers::CONTROL)
    }

    /// Handle a left-click in list mode: select the conversation under the cursor.
    /// Returns true if the click landed on a list item — the caller is expected to
    /// then transition into view mode (matching the Enter-key behavior).
    pub fn handle_list_click(&mut self, row: u16, frame_area: Rect) -> bool {
        if !matches!(self.app_mode, AppMode::List)
            || self.dialog_mode != DialogMode::None
            || self.is_loading()
        {
            return false;
        }

        // Mirror the layout in render_list_mode: outer 1px border, then split
        // [search bar (2), list (Min 1), bottom bar (1)] — or omit the bottom
        // bar when the inner area is < 4 lines tall.
        let inner_height = frame_area.height.saturating_sub(2);
        let list_y = frame_area.y.saturating_add(1).saturating_add(2);
        let list_height = if inner_height < 4 {
            inner_height.saturating_sub(2)
        } else {
            inner_height.saturating_sub(3)
        };

        if list_height == 0 || row < list_y || row >= list_y.saturating_add(list_height) {
            return false;
        }

        let lines_per_item = list_lines_per_item(self.list_search_mode, &self.query);
        let items_per_page = (list_height as usize) / lines_per_item;
        if items_per_page == 0 {
            return false;
        }

        let offset = match self.selected {
            Some(sel) => (sel / items_per_page) * items_per_page,
            None => 0,
        };
        let relative_row = (row - list_y) as usize;
        let relative_idx = relative_row / lines_per_item;
        let new_idx = offset + relative_idx;
        if new_idx < self.filtered.len() {
            self.selected = Some(new_idx);
            true
        } else {
            false
        }
    }
}

/// Parse the one conversation a directly opened file holds, through the same
/// parser the list uses. `None` for a file that holds none.
fn parse_single_file(path: &Path) -> Option<Conversation> {
    let modified = std::fs::metadata(path)
        .and_then(|file| file.modified())
        .ok();
    let mut conversation = process_conversation_file(path.to_path_buf(), modified, None)
        .ok()
        .flatten()?;

    // Set project_name the same way as the loader does
    let project_path = conversation
        .cwd
        .clone()
        .unwrap_or_else(|| path.to_path_buf());
    conversation.project_name = Some(format_short_name_from_path(&project_path));

    Some(conversation)
}

#[cfg(test)]
mod semantic_test_helpers;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod interaction_tests;
