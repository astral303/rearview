use super::{App, AppMode, CountHighlight, DialogMode, Focus, ViewSearchMode, ViewState};
use crate::history::Conversation;
use crate::tui::ui;
use crate::tui::viewer::{
    CallArea, CallRange, EntryRelocation, MessageRange, RenderOptions, RenderedLine, ToolOutputId,
};
use ratatui::prelude::*;
use std::collections::BTreeSet;
use std::sync::Arc;

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
        self.open_conversation(conv_idx, frame_width);
    }

    pub(super) fn open_conversation(&mut self, conv_idx: usize, frame_width: usize) {
        use crate::tui::viewer::{
            content_width, parse_conversation_file, render_parsed_conversation,
        };

        let path = self.conversations[conv_idx].path.clone();
        let source = self.conversations[conv_idx].source;
        let session_id = Some(self.conversations[conv_idx].session_id.clone());
        let subagents = self.conversations[conv_idx].subagents.clone();

        let options = RenderOptions {
            tool_display: self.tool_display,
            show_thinking: self.show_thinking,
            show_timing: self.show_timing,
            content_width: content_width(frame_width, self.show_timing),
            expanded_tool_outputs: BTreeSet::new(),
            can_expand: true,
        };

        match parse_conversation_file(source, &path, &subagents) {
            Ok(conversation) => {
                let conversation = Arc::new(conversation);
                let rendered = render_parsed_conversation(&conversation, &options);
                let total_lines = rendered.lines.len();
                let first_msg = (!rendered.messages.is_empty()).then_some(Focus {
                    message_index: 0,
                    call_index: None,
                });
                self.app_mode = AppMode::View(ViewState {
                    conversation_path: path,
                    conversation_source: source,
                    session_id,
                    subagents,
                    parsed_conversation: Some(conversation),
                    scroll_offset: 0,
                    rendered_lines: rendered.lines,
                    total_lines,
                    tool_display: self.tool_display,
                    show_thinking: self.show_thinking,
                    show_timing: self.show_timing,
                    frame_width,
                    search_mode: ViewSearchMode::Off,
                    search_query: String::new(),
                    search_matches: Vec::new(),
                    current_match_index: 0,
                    message_ranges: rendered.messages,
                    call_ranges: rendered.calls,
                    focus: first_msg,
                    message_nav_active: false,
                    expanded_tool_outputs: BTreeSet::new(),
                    hovered_tool_output: None,
                    count_highlight: None,
                });
            }
            Err(e) => {
                self.status_message =
                    Some((format!("Failed to open: {}", e), std::time::Instant::now()));
            }
        }
    }

    pub fn exit_view_mode(&mut self) {
        self.app_mode = AppMode::List;
        self.pending_view_refresh = false;
    }

    pub(super) fn start_view_search(&mut self) {
        if let AppMode::View(ref mut state) = self.app_mode {
            state.search_mode = ViewSearchMode::Typing;
            state.search_query.clear();
            state.search_matches.clear();
            state.current_match_index = 0;
        }
    }

    pub(super) fn clear_view_search(&mut self) {
        if let AppMode::View(ref mut state) = self.app_mode {
            state.search_mode = ViewSearchMode::Off;
            state.search_query.clear();
            state.search_matches.clear();
        }
    }

    pub(super) fn clear_view_search_query(&mut self) -> bool {
        if let AppMode::View(ref mut state) = self.app_mode
            && !state.search_query.is_empty()
        {
            state.search_query.clear();
            self.update_search_results();
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
        self.update_search_results();
    }

    pub(super) fn push_view_search_char(&mut self, c: char) {
        if let AppMode::View(ref mut state) = self.app_mode {
            state.search_query.push(c);
        }
        self.update_search_results();
    }

    pub(super) fn backspace_view_search(&mut self) {
        if let AppMode::View(ref mut state) = self.app_mode {
            state.search_query.pop();
        }
        self.update_search_results();
    }

    pub(super) fn commit_view_search(&mut self) {
        if let AppMode::View(ref mut state) = self.app_mode {
            if !state.search_matches.is_empty() {
                state.search_mode = ViewSearchMode::Active;
            } else {
                state.search_mode = ViewSearchMode::Off;
            }
        }
    }

    pub(super) fn update_search_results(&mut self) {
        if let AppMode::View(ref mut state) = self.app_mode {
            let query_lower = state.search_query.to_lowercase();
            if query_lower.is_empty() {
                state.search_matches.clear();
                return;
            }

            state.search_matches = state
                .rendered_lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line_matches_query(line, &query_lower))
                .map(|(i, _)| i)
                .collect();

            if !state.search_matches.is_empty() {
                state.current_match_index = 0;
                let match_line = state.search_matches[0];
                state.scroll_offset = match_line;
                Self::focus_message_at_line(state, match_line);
            }
        }
    }

    pub(super) fn next_search_match(&mut self, viewport_height: usize) {
        if let AppMode::View(ref mut state) = self.app_mode {
            if state.search_matches.is_empty() {
                return;
            }
            state.current_match_index =
                (state.current_match_index + 1) % state.search_matches.len();
            let match_line = state.search_matches[state.current_match_index];
            if match_line < state.scroll_offset
                || match_line >= state.scroll_offset + viewport_height
            {
                state.scroll_offset = match_line;
            }
            Self::focus_message_at_line(state, match_line);
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
            let match_line = state.search_matches[state.current_match_index];
            if match_line < state.scroll_offset
                || match_line >= state.scroll_offset + viewport_height
            {
                state.scroll_offset = match_line;
            }
            Self::focus_message_at_line(state, match_line);
        }
    }

    pub(super) fn toggle_view_tools(&mut self, viewport_height: usize) {
        if let AppMode::View(ref mut state) = self.app_mode {
            state.tool_display = state.tool_display.next();
            self.tool_display = state.tool_display;
            self.re_render_view(viewport_height);
        }
    }

    pub(super) fn toggle_view_thinking(&mut self, viewport_height: usize) {
        if let AppMode::View(ref mut state) = self.app_mode {
            state.show_thinking = !state.show_thinking;
            self.show_thinking = state.show_thinking;
            self.re_render_view(viewport_height);
        }
    }

    pub(super) fn toggle_view_timing(&mut self, viewport_height: usize) {
        if let AppMode::View(ref mut state) = self.app_mode {
            state.show_timing = !state.show_timing;
            self.show_timing = state.show_timing;
            self.re_render_view(viewport_height);
        }
    }

    pub(super) fn re_render_view(&mut self, viewport_height: usize) {
        self.render_view(viewport_height, None, false);
    }

    /// Read the open session again (`Ctrl+R`): new messages appear, its row
    /// in the list updates, and the view keeps the reader's place. A changed
    /// message count is marked in the view; a refresh reports nothing else
    /// unless it failed. A session no longer found, or now empty, returns to
    /// the list. False when no session is open, so nothing was read.
    pub(super) fn refresh_open_session(&mut self, viewport_height: usize) -> bool {
        use crate::history::format::same_file;
        use crate::history::provider::SessionRead;
        use crate::tui::viewer::parse_conversation_file;

        let AppMode::View(state) = &self.app_mode else {
            return false;
        };
        let path = state.conversation_path.clone();
        let source = state.conversation_source;
        self.note_changed_during_refresh(&path);
        let row_index = self
            .conversations
            .iter()
            .position(|row| same_file(&row.path, &path));
        match self.read_open_session(&path, source, row_index) {
            Some(SessionRead::Listed(row)) => {
                let subagents = row.subagents.clone();
                match parse_conversation_file(source, &path, &subagents) {
                    Ok(conversation) => {
                        let session_id = Some(row.session_id.clone());
                        self.store_open_session_row(row_index, Some(*row));
                        self.show_reread_conversation(
                            viewport_height,
                            Arc::new(conversation),
                            subagents,
                            session_id,
                        );
                    }
                    Err(error) => self.set_status(format!("Refresh failed: {error}")),
                }
            }
            Some(SessionRead::Failed(_)) => {
                self.set_status("Refresh failed: the session could not be read".to_owned());
            }
            None => self.leave_session_gone_or_empty(row_index, "Session not found"),
            Some(SessionRead::Empty) => {
                self.leave_session_gone_or_empty(row_index, "Session is empty");
            }
        }
        true
    }

    /// Return to the list from an open session that is gone or now empty,
    /// removing its row, and show `message`. A file opened directly keeps the
    /// view.
    fn leave_session_gone_or_empty(&mut self, row_index: Option<usize>, message: &str) {
        if !self.single_file_mode {
            self.store_open_session_row(row_index, None);
            self.exit_view_mode();
        }
        self.set_status(message.to_owned());
    }

    /// The open session read again as the list builds its row. A file opened
    /// directly is read by the format it was opened with, since no agent's
    /// discovery lists it.
    fn read_open_session(
        &self,
        path: &std::path::Path,
        source: crate::history::Source,
        row_index: Option<usize>,
    ) -> Option<crate::history::provider::SessionRead> {
        use crate::history::provider::{ReadError, SessionRead};

        if self.single_file_mode {
            if !path.exists() {
                return None;
            }
            return Some(match super::read_single_file(path, source) {
                Ok(Some(row)) => SessionRead::Listed(Box::new(row)),
                Ok(None) => SessionRead::Empty,
                Err(error) => SessionRead::Failed(ReadError::of(&error)),
            });
        }
        let row = &self.conversations[row_index?];
        (self.session_reader)(&[row], self.show_last)
            .pop()
            .flatten()
    }

    /// Replace the open session's row with `row`, or remove it when `row` is
    /// `None`.
    fn store_open_session_row(&mut self, row_index: Option<usize>, row: Option<Conversation>) {
        let Some(index) = row_index else {
            return;
        };
        if self.single_file_mode {
            if let Some(row) = row {
                self.conversations[index] = row;
            }
            return;
        }
        self.update_listed_row(index, row);
    }

    /// Show `conversation`, the open session read again, where the reader
    /// was: at the bottom, the view stays at the bottom so new messages show;
    /// otherwise the same message stays at the top. Expanded rows stay
    /// expanded where their entry is still there. A changed message count,
    /// counted as message navigation steps through them, sets the view's
    /// `CountHighlight`.
    fn show_reread_conversation(
        &mut self,
        viewport_height: usize,
        conversation: Arc<crate::tui::viewer::ParsedConversation>,
        subagents: Vec<std::path::PathBuf>,
        session_id: Option<String>,
    ) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let is_at_bottom = state.is_at_bottom(viewport_height);
        let messages_before = state.message_ranges.len();
        let relocation = state
            .parsed_conversation
            .as_ref()
            .map(|earlier| EntryRelocation::between(earlier, &conversation));
        if let Some(relocation) = &relocation {
            state.expanded_tool_outputs = state
                .expanded_tool_outputs
                .iter()
                .filter_map(|id| relocation.tool_output_id(id))
                .collect();
        }
        state.hovered_tool_output = None;
        state.subagents = subagents;
        state.session_id = session_id;
        state.parsed_conversation = Some(conversation);
        self.render_view(viewport_height, relocation.as_ref(), is_at_bottom);
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let messages_after = state.message_ranges.len();
        if messages_after != messages_before {
            state.count_highlight = Some(CountHighlight {
                until: std::time::Instant::now() + COUNT_HIGHLIGHT,
                arrived_below: (is_at_bottom && messages_after > messages_before)
                    .then(|| messages_after - messages_before),
            });
        }
    }

    /// Render the open conversation with the view's current options, keeping
    /// the anchor message on its row and the focused call focused.
    /// `relocation` carries both into a conversation read again; `None` when
    /// the conversation is the one last rendered. With `stick_to_bottom`, the
    /// view ends on the last row instead.
    fn render_view(
        &mut self,
        viewport_height: usize,
        relocation: Option<&EntryRelocation>,
        stick_to_bottom: bool,
    ) {
        use crate::tui::viewer::{parse_conversation_file, render_parsed_conversation};

        if let AppMode::View(ref mut state) = self.app_mode {
            let options = RenderOptions {
                tool_display: state.tool_display,
                show_thinking: state.show_thinking,
                show_timing: state.show_timing,
                content_width: state.content_width(),
                expanded_tool_outputs: state.expanded_tool_outputs.clone(),
                can_expand: true,
            };

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
            let focused_call_id = Self::focused_call_range(state)
                .map(|call| call.input.id.clone())
                .and_then(|id| match relocation {
                    Some(relocation) => relocation.tool_output_id(&id),
                    None => Some(id),
                });
            let old_scroll = state.scroll_offset;

            let conversation = match state.parsed_conversation.clone() {
                Some(conversation) => conversation,
                None => match parse_conversation_file(
                    state.conversation_source,
                    &state.conversation_path,
                    &state.subagents,
                ) {
                    Ok(conversation) => {
                        let conversation = Arc::new(conversation);
                        state.parsed_conversation = Some(conversation.clone());
                        conversation
                    }
                    Err(_) => return,
                },
            };
            let rendered = render_parsed_conversation(&conversation, &options);
            state.total_lines = rendered.lines.len();
            state.rendered_lines = rendered.lines;
            state.message_ranges = rendered.messages;
            state.call_ranges = rendered.calls;

            let max_scroll = state.total_lines.saturating_sub(viewport_height);

            let resolved_idx = anchor
                .and_then(|a| find_message_idx_or_prev(&state.message_ranges, a.entry_index))
                .or_else(|| (!state.message_ranges.is_empty()).then_some(0));
            // The call is found again by its id, not its index, so rows opening
            // above it do not move the focus.
            let resolved_call = focused_call_id.and_then(|id| {
                state
                    .call_ranges
                    .iter()
                    .position(|call| call.input.id == id)
            });
            state.focus = resolved_idx.map(|message_index| Focus {
                message_index,
                call_index: resolved_call,
            });

            state.scroll_offset = match (anchor, resolved_idx) {
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
                _ => old_scroll.min(max_scroll),
            };
            if stick_to_bottom {
                state.scroll_offset = max_scroll;
                if state.message_nav_active {
                    Self::sync_focus_to_scroll(state, viewport_height);
                }
            }

            if state.search_mode == ViewSearchMode::Active && !state.search_query.is_empty() {
                let query_lower = state.search_query.to_lowercase();
                state.search_matches = state
                    .rendered_lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| line_matches_query(line, &query_lower))
                    .map(|(i, _)| i)
                    .collect();

                if state.search_matches.is_empty() {
                    state.current_match_index = 0;
                } else {
                    state.current_match_index = state
                        .current_match_index
                        .min(state.search_matches.len() - 1);
                }
            }
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
        let is_at_message_stop = state.focused_call().is_none();
        if Self::expand_all(state, ids) {
            self.re_render_view(viewport_height);
        }
        if is_at_message_stop {
            self.focus_first_call(viewport_height);
        }
    }

    pub(super) fn collapse_focused(&mut self, viewport_height: usize) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let ids = Self::focused_tool_output_ids(state);
        if Self::collapse_all(state, ids) {
            self.re_render_view(viewport_height);
            return;
        }
        if let Some(focus) = &mut state.focus
            && focus.call_index.is_some()
        {
            focus.call_index = None;
        }
    }

    pub(super) fn toggle_focused(&mut self, viewport_height: usize) {
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
            Self::collapse_all(state, ids);
        } else {
            Self::expand_all(state, ids);
        }
        self.re_render_view(viewport_height);
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
    /// otherwise the focused message's own rows.
    fn focused_tool_output_ids(state: &ViewState) -> BTreeSet<ToolOutputId> {
        match Self::focused_call_range(state) {
            Some(call) => call
                .areas()
                .filter(|area| Self::is_expandable(state, area))
                .map(|area| area.id.clone())
                .collect(),
            None => Self::focused_message_tool_output_ids(state),
        }
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
            AppMode::List => self.scroll_list(delta.signum()),
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

    pub fn handle_view_click(
        &mut self,
        row: u16,
        frame_area: Rect,
        viewport_height: usize,
    ) -> bool {
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
        if let Some(id) = tool_output {
            Self::toggle_expanded_tool_output(state, id.clone());
            state.hovered_tool_output = Some(id);
            changed = true;
        }
        if changed {
            self.re_render_view(viewport_height);
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
            .as_ref()
            .and_then(|conversation| conversation.entry(entry_index))
            .map(|entry| crate::tui::export::format_entry_for_clipboard(entry, options));

        match text {
            Some(text) if text.is_empty() => {
                self.set_status("No text content in this message".to_owned());
            }
            Some(text) => self.copy_to_clipboard("Message", &text),
            None => self.set_status("Message not found".to_owned()),
        }
    }

    pub(super) fn copy_focused(&mut self, viewport_height: usize) {
        if self.focused_call_is_active() {
            self.copy_focused_call();
        } else {
            self.copy_focused_message(viewport_height);
        }
    }

    fn copy_focused_call(&mut self) {
        match self.focused_call_text() {
            Ok(text) => self.copy_to_clipboard("Call", &text),
            Err(e) => {
                self.status_message = Some((e, std::time::Instant::now()));
            }
        }
    }

    fn focused_call_text(&self) -> Result<String, String> {
        let AppMode::View(state) = &self.app_mode else {
            return Err("Not viewing a conversation".to_string());
        };
        let call = Self::focused_call_range(state).ok_or_else(|| "No call focused".to_string())?;
        let conversation = state
            .parsed_conversation
            .as_ref()
            .ok_or_else(|| "Call not found".to_string())?;
        crate::tui::export::extract_call_text(
            |entry_index| conversation.entry(entry_index),
            call.input.location,
            call.result.as_ref().map(|result| result.location),
        )
    }

    pub fn check_view_resize(&mut self, frame_width: usize, viewport_height: usize) {
        if let AppMode::View(ref mut state) = self.app_mode
            && state.frame_width != frame_width
        {
            state.frame_width = frame_width;
            self.re_render_view(viewport_height);
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

pub fn line_matches_query(line: &RenderedLine, query_lower: &str) -> bool {
    let full_text: String = line.spans.iter().map(|(text, _)| text.as_str()).collect();
    full_text.to_lowercase().contains(query_lower)
}
