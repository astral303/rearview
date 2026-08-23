use super::{App, AppMode, DialogMode, ViewSearchMode, ViewState};
use crate::tui::ui;
use crate::tui::viewer::{
    CallArea, CallRange, MessageRange, RenderOptions, RenderedLine, ToolOutputId,
};
use ratatui::prelude::*;
use std::collections::BTreeSet;
use std::sync::Arc;

impl App {
    pub fn enter_view_mode(&mut self, content_width: usize) {
        use crate::tui::viewer::{parse_conversation_file, render_parsed_conversation};

        let Some(selected) = self.selected else {
            return;
        };
        let Some(&conv_idx) = self.filtered.get(selected) else {
            return;
        };
        let path = self.conversations[conv_idx].path.clone();
        let source = self.conversations[conv_idx].source;
        let session_id = Some(self.conversations[conv_idx].session_id.clone());

        let options = RenderOptions {
            tool_display: self.tool_display,
            show_thinking: self.show_thinking,
            show_timing: self.show_timing,
            content_width,
            expanded_tool_outputs: BTreeSet::new(),
        };

        match parse_conversation_file(source, &path) {
            Ok(entries) => {
                let entries = Arc::new(entries);
                let rendered = render_parsed_conversation(&entries, &options);
                let total_lines = rendered.lines.len();
                let first_msg = if rendered.messages.is_empty() {
                    None
                } else {
                    Some(0)
                };
                self.app_mode = AppMode::View(ViewState {
                    conversation_path: path,
                    conversation_source: source,
                    session_id,
                    parsed_entries: Some(entries),
                    scroll_offset: 0,
                    rendered_lines: rendered.lines,
                    total_lines,
                    tool_display: self.tool_display,
                    show_thinking: self.show_thinking,
                    show_timing: self.show_timing,
                    content_width,
                    search_mode: ViewSearchMode::Off,
                    search_query: String::new(),
                    search_matches: Vec::new(),
                    current_match: 0,
                    message_ranges: rendered.messages,
                    call_ranges: rendered.calls,
                    focused_message: first_msg,
                    focused_call: None,
                    message_nav_active: false,
                    expanded_tool_outputs: BTreeSet::new(),
                    hovered_tool_output: None,
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
    }

    pub(super) fn start_view_search(&mut self) {
        if let AppMode::View(ref mut state) = self.app_mode {
            state.search_mode = ViewSearchMode::Typing;
            state.search_query.clear();
            state.search_matches.clear();
            state.current_match = 0;
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
                state.current_match = 0;
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
            state.current_match = (state.current_match + 1) % state.search_matches.len();
            let match_line = state.search_matches[state.current_match];
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
            state.current_match = if state.current_match == 0 {
                state.search_matches.len() - 1
            } else {
                state.current_match - 1
            };
            let match_line = state.search_matches[state.current_match];
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
        use crate::tui::viewer::{parse_conversation_file, render_parsed_conversation};

        if let AppMode::View(ref mut state) = self.app_mode {
            let options = RenderOptions {
                tool_display: state.tool_display,
                show_thinking: state.show_thinking,
                show_timing: state.show_timing,
                content_width: state.content_width,
                expanded_tool_outputs: state.expanded_tool_outputs.clone(),
            };

            let anchor = capture_anchor(
                &state.message_ranges,
                state.scroll_offset,
                state.focused_message,
                state.message_nav_active,
            );
            let focused_call_id = Self::focused_call_range(state).map(|call| call.input.id.clone());
            let old_scroll = state.scroll_offset;

            let entries = match state.parsed_entries.clone() {
                Some(entries) => entries,
                None => match parse_conversation_file(
                    state.conversation_source,
                    &state.conversation_path,
                ) {
                    Ok(entries) => {
                        let entries = Arc::new(entries);
                        state.parsed_entries = Some(entries.clone());
                        entries
                    }
                    Err(_) => return,
                },
            };
            let rendered = render_parsed_conversation(&entries, &options);
            state.total_lines = rendered.lines.len();
            state.rendered_lines = rendered.lines;
            state.message_ranges = rendered.messages;
            state.call_ranges = rendered.calls;
            state.focused_call = focused_call_id.and_then(|id| {
                state
                    .call_ranges
                    .iter()
                    .position(|call| call.input.id == id)
            });

            let max_scroll = state.total_lines.saturating_sub(viewport_height);

            let resolved_idx = anchor
                .and_then(|a| find_message_idx_or_prev(&state.message_ranges, a.entry_index))
                .or_else(|| (!state.message_ranges.is_empty()).then_some(0));
            state.focused_message = resolved_idx;

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
                    state.current_match = 0;
                } else {
                    state.current_match = state.current_match.min(state.search_matches.len() - 1);
                }
            }
        }
    }

    pub(super) fn focus_next_message(&mut self, viewport_height: usize) {
        if let AppMode::View(ref mut state) = self.app_mode {
            if state.message_ranges.is_empty() {
                return;
            }
            if !state.message_nav_active {
                state.message_nav_active = true;
                Self::sync_focus_to_scroll(state, viewport_height);
            }
            let next = match state.focused_message {
                Some(i) if i + 1 < state.message_ranges.len() => i + 1,
                Some(i) => i,
                None => 0,
            };
            Self::set_focused_message(state, next);
            Self::ensure_message_visible(state, viewport_height);
        }
    }

    pub(super) fn focus_prev_message(&mut self, viewport_height: usize) {
        if let AppMode::View(ref mut state) = self.app_mode {
            if state.message_ranges.is_empty() {
                return;
            }
            if !state.message_nav_active {
                state.message_nav_active = true;
                Self::sync_focus_to_scroll(state, viewport_height);
            }
            let prev = match state.focused_message {
                Some(i) if i > 0 => i - 1,
                Some(i) => i,
                None => 0,
            };
            Self::set_focused_message(state, prev);
            Self::ensure_message_visible(state, viewport_height);
        }
    }

    fn set_focused_message(state: &mut ViewState, idx: usize) {
        if state.focused_message != Some(idx) {
            state.focused_message = Some(idx);
            state.focused_call = None;
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
        if self.focused_call_is_active() {
            self.focus_next_call(viewport_height);
        } else {
            self.focus_next_message(viewport_height);
        }
    }

    pub(super) fn focus_prev(&mut self, viewport_height: usize) {
        if self.focused_call_is_active() {
            self.focus_prev_call(viewport_height);
        } else {
            self.focus_prev_message(viewport_height);
        }
    }

    fn focused_call_is_active(&self) -> bool {
        matches!(&self.app_mode, AppMode::View(state) if state.focused_call.is_some())
    }

    fn focus_next_call(&mut self, viewport_height: usize) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let Some(next) = state.focused_call.map(|call| call + 1) else {
            return;
        };
        if Self::focused_message_calls(state).contains(&next) {
            Self::focus_call(state, next, viewport_height);
        }
    }

    fn focus_prev_call(&mut self, viewport_height: usize) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let Some(prev) = state.focused_call.and_then(|call| call.checked_sub(1)) else {
            return;
        };
        if Self::focused_message_calls(state).contains(&prev) {
            Self::focus_call(state, prev, viewport_height);
        }
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
        state.focused_call = Some(call);
        let first_row = state.call_ranges[call].input.start_line;
        Self::ensure_line_visible(state, first_row, viewport_height);
    }

    fn focused_message_calls(state: &ViewState) -> std::ops::Range<usize> {
        let Some(message) = state
            .focused_message
            .and_then(|idx| state.message_ranges.get(idx))
        else {
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
            .focused_call
            .and_then(|call| state.call_ranges.get(call))
    }

    pub(super) fn expand_focused(&mut self, viewport_height: usize) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let Some(call) = Self::focused_call_range(state) else {
            self.enter_focused_run(viewport_height);
            return;
        };
        let collapsed = call
            .areas()
            .find(|area| Self::is_expandable(state, area) && !Self::is_expanded(state, area))
            .map(|area| area.id.clone());
        let Some(id) = collapsed else {
            return;
        };
        state.expanded_tool_outputs.insert(id);
        self.re_render_view(viewport_height);
    }

    pub(super) fn collapse_focused(&mut self, viewport_height: usize) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let Some(call) = Self::focused_call_range(state) else {
            self.collapse_focused_run(viewport_height);
            return;
        };
        let expanded = call
            .areas()
            .rev()
            .find(|area| Self::is_expanded(state, area))
            .map(|area| area.id.clone());
        let Some(id) = expanded else {
            state.focused_call = None;
            return;
        };
        state.expanded_tool_outputs.remove(&id);
        self.re_render_view(viewport_height);
    }

    pub(super) fn toggle_focused(&mut self, viewport_height: usize) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let Some(call) = Self::focused_call_range(state) else {
            self.toggle_focused_tool_run(viewport_height);
            return;
        };
        let expandable = call
            .areas()
            .rev()
            .find(|area| Self::is_expandable(state, area))
            .map(|area| area.id.clone());
        let Some(id) = expandable else {
            return;
        };
        Self::toggle_expanded_tool_output(state, id);
        self.re_render_view(viewport_height);
    }

    fn enter_focused_run(&mut self, viewport_height: usize) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let Some(run_id) = Self::focused_tool_run_id(state) else {
            return;
        };
        let was_collapsed = state.expanded_tool_outputs.insert(run_id);
        if was_collapsed {
            self.re_render_view(viewport_height);
        }
        self.focus_first_call(viewport_height);
    }

    fn collapse_focused_run(&mut self, viewport_height: usize) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let Some(run_id) = Self::focused_tool_run_id(state) else {
            return;
        };
        let was_expanded = state.expanded_tool_outputs.remove(&run_id);
        if was_expanded {
            self.re_render_view(viewport_height);
        }
    }

    /// The renderer marks a row clickable only where a click toggles it: a
    /// truncated body, its `(N more lines...)` row, or an expanded body.
    fn is_expandable(state: &ViewState, area: &CallArea) -> bool {
        state.rendered_lines[area.start_line..area.end_line]
            .iter()
            .any(|line| line.clickable)
    }

    fn is_expanded(state: &ViewState, area: &CallArea) -> bool {
        state.expanded_tool_outputs.contains(&area.id)
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
        if state.focused_call.is_some() {
            state.focused_call = None;
            changed = true;
        }
        if let Some(idx) = message_idx
            && (!state.message_nav_active || state.focused_message != Some(idx))
        {
            state.message_nav_active = true;
            state.focused_message = Some(idx);
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

    pub(super) fn toggle_focused_tool_run(&mut self, viewport_height: usize) {
        let AppMode::View(state) = &mut self.app_mode else {
            return;
        };
        let Some(run_id) = Self::focused_tool_run_id(state) else {
            return;
        };
        Self::toggle_expanded_tool_output(state, run_id);
        self.re_render_view(viewport_height);
    }

    fn focused_tool_run_id(state: &ViewState) -> Option<ToolOutputId> {
        if !state.message_nav_active || !state.tool_display.is_summary() {
            return None;
        }
        let message = state.message_ranges.get(state.focused_message?)?;
        let run_row = state.rendered_lines.get(message.start_line)?;
        run_row.tool_output_id.clone().filter(|_| run_row.clickable)
    }

    fn toggle_expanded_tool_output(state: &mut ViewState, id: ToolOutputId) {
        if !state.expanded_tool_outputs.remove(&id) {
            state.expanded_tool_outputs.insert(id);
        }
    }

    /// Point focus at what the viewport shows:
    ///
    /// - message: the first one on screen
    /// - call: the first one on screen inside that message's expanded run
    ///
    /// Never scrolls. The scroll keys call this after moving the offset.
    fn sync_focus_to_scroll(state: &mut ViewState, viewport_height: usize) {
        if state.message_ranges.is_empty() {
            return;
        }
        let viewport = state.scroll_offset..state.scroll_offset + viewport_height;
        let found = state
            .message_ranges
            .iter()
            .position(|m| m.end_line > viewport.start && m.start_line < viewport.end);
        if let Some(idx) = found {
            state.focused_message = Some(idx);
        }
        // Recomputing subsumes the clearing `set_focused_message` does: no
        // calls, or none on screen, both leave `None`.
        state.focused_call = Self::focused_message_calls(state)
            .find(|&idx| state.call_ranges[idx].overlaps(&viewport));
    }

    fn ensure_message_visible(state: &mut ViewState, viewport_height: usize) {
        if let Some(idx) = state.focused_message
            && let Some(msg) = state.message_ranges.get(idx)
        {
            Self::ensure_line_visible(state, msg.start_line, viewport_height);
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

        let (source, path, entry_index) = if let AppMode::View(ref state) = self.app_mode {
            if let Some(idx) = state.focused_message {
                if let Some(msg) = state.message_ranges.get(idx) {
                    (
                        state.conversation_source,
                        state.conversation_path.clone(),
                        msg.entry_index,
                    )
                } else {
                    return;
                }
            } else {
                return;
            }
        } else {
            return;
        };

        let options = if let AppMode::View(ref state) = self.app_mode {
            crate::tui::export::ExportOptions {
                show_tools: state.tool_display.is_visible(),
                show_thinking: state.show_thinking,
            }
        } else {
            return;
        };

        match crate::tui::export::extract_message_text(source, &path, entry_index, options) {
            Ok(text) if text.is_empty() => {
                self.status_message = Some((
                    "No text content in this message".to_string(),
                    std::time::Instant::now(),
                ));
            }
            Ok(text) => self.copy_to_clipboard("Message", &text),
            Err(e) => {
                self.status_message = Some((e, std::time::Instant::now()));
            }
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
        crate::tui::export::extract_call_text(
            state.conversation_source,
            &state.conversation_path,
            call.input.location,
            call.result.as_ref().map(|result| result.location),
        )
    }

    pub fn check_view_resize(&mut self, new_content_width: usize, viewport_height: usize) {
        if let AppMode::View(ref mut state) = self.app_mode
            && state.content_width != new_content_width
        {
            state.content_width = new_content_width;
            self.re_render_view(viewport_height);
        }
    }
}

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
