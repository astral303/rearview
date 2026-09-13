use crate::config::KeyBindings;
use crate::history::{Conversation, LoadProgress, LoadUnit};
use crate::search::LexicalEvidence;
#[cfg(test)]
use crate::search::preview::find_normalized_match_ranges;
use crate::search::preview::{
    HighlightQuery, build_context_segments_from_ranges, build_literal_context_segments,
    build_match_segments, build_match_segments_for_query, merge_match_ranges, sanitize_preview,
    simple_truncate,
};
use crate::tui::app::{
    App, AppMode, DialogMode, ListSearchMode, LoadingState, SemanticResultMetadata, ViewSearchMode,
    ViewState, list_lines_per_item,
};
use crate::tui::theme::{self, Theme};
use crate::tui::viewer::{LineStyle, RenderedLine, format_coarse_duration};
use chrono::{DateTime, Local};
use ratatui::layout::Position;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Get the current theme
fn th() -> &'static Theme {
    theme::detect_theme()
}

/// Convert theme RGB tuple to ratatui Color
fn rgb(c: (u8, u8, u8)) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// The search bar's cue to the key that lists the filters in force. One string
/// so the width it is budgeted and the spans it renders as cannot disagree.
const FILTER_CUE: &str = " · ^L filters";

/// Duration before status messages auto-clear
const STATUS_TTL: std::time::Duration = std::time::Duration::from_secs(3);

/// Format model name for display (e.g., "claude-opus-4-5-20251101" → "opus-4.5")
fn format_model_name(model: &str) -> String {
    // Handle claude-opus-4-5-YYYYMMDD format
    if let Some(rest) = model.strip_prefix("claude-opus-4-5-")
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return "opus-4.5".to_string();
    }

    // Handle claude-sonnet-4-YYYYMMDD format
    if let Some(rest) = model.strip_prefix("claude-sonnet-4-")
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return "sonnet-4".to_string();
    }

    // Handle claude-3-5-sonnet-YYYYMMDD format
    if let Some(rest) = model.strip_prefix("claude-3-5-sonnet-")
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return "sonnet-3.5".to_string();
    }

    // Handle claude-3-5-haiku-YYYYMMDD format
    if let Some(rest) = model.strip_prefix("claude-3-5-haiku-")
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return "haiku-3.5".to_string();
    }

    // Handle claude-3-opus-YYYYMMDD format
    if let Some(rest) = model.strip_prefix("claude-3-opus-")
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return "opus-3".to_string();
    }

    // Handle claude-3-sonnet-YYYYMMDD format
    if let Some(rest) = model.strip_prefix("claude-3-sonnet-")
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return "sonnet-3".to_string();
    }

    // Handle claude-3-haiku-YYYYMMDD format
    if let Some(rest) = model.strip_prefix("claude-3-haiku-")
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return "haiku-3".to_string();
    }

    // Unknown format - truncate if too long
    if model.len() > 20 {
        format!("{}…", &model[..19])
    } else {
        model.to_string()
    }
}

/// Format token count with K/M suffix (short form, e.g., "926k")
pub(crate) fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

/// Format token count with K/M suffix and "tokens" label (long form, e.g., "926k tokens")
fn format_tokens_long(tokens: u64) -> String {
    format!("{} tokens", format_tokens(tokens))
}

/// Render the TUI
pub fn render(frame: &mut Frame, app: &App) {
    match app.app_mode() {
        AppMode::List => render_list_mode(frame, app),
        AppMode::View(state) => render_view_mode(frame, app, state),
    }
}

/// Render the list mode (conversation browser)
fn render_list_mode(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Outer border wrapping the entire app
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(rgb(th().border)));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    // Graceful degradation for tiny terminals - skip bottom bar if too small
    if inner_area.height < 4 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(inner_area);
        render_search_bar(frame, app, chunks[0]);
        render_list(frame, app, chunks[1]);
        return;
    }

    // Always reserve space for bottom bar (status, dialog, or hotkeys)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner_area);

    render_search_bar(frame, app, chunks[0]);
    render_list(frame, app, chunks[1]);

    // Render bottom bar: confirm dialog > status message > hotkeys
    if *app.dialog_mode() == DialogMode::ConfirmDelete {
        render_confirm_dialog(frame, chunks[2]);
    } else if let Some((msg, instant)) = app.status_message()
        && instant.elapsed() < STATUS_TTL
    {
        render_status_message(frame, msg, chunks[2]);
    } else {
        render_list_status_bar(frame, app, chunks[2]);
    }

    match app.dialog_mode() {
        DialogMode::Help { scroll } => render_help_overlay(
            frame,
            false,
            false,
            app.semantic_toggle_available(),
            !app.active_filters().is_empty(),
            app.keys(),
            *scroll,
        ),
        DialogMode::SemanticDebug => render_semantic_debug_popup(frame, app),
        DialogMode::ActiveFilters => render_active_filters_popup(frame, app),
        DialogMode::Rename { input, cursor } => render_rename_dialog(frame, input, *cursor),
        _ => {}
    }
}

fn render_status_message(frame: &mut Frame, msg: &str, area: Rect) {
    let status_line = Line::from(vec![
        Span::raw("  "),
        Span::styled(msg, Style::default().fg(Color::Yellow)),
    ]);
    let status = Paragraph::new(status_line).style(Style::default().bg(rgb(th().status_bar_bg)));
    frame.render_widget(status, area);
}

fn render_activity_status(frame: &mut Frame, msg: &str, area: Rect) {
    let status_line = Line::from(vec![
        Span::raw("  "),
        Span::styled(msg, Style::default().fg(rgb(th().accent)).bold()),
    ]);
    let status = Paragraph::new(status_line).style(Style::default().bg(rgb(th().status_bar_bg)));
    frame.render_widget(status, area);
}

fn render_list_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let is_loading = app.is_loading();

    let key_style = Style::default().fg(rgb(th().accent));
    let label_style = Style::default().fg(rgb(th().text_muted));
    // Dimmed styles for unavailable shortcuts during loading
    let dim_key_style = Style::default().fg(rgb(th().dim_key));
    let dim_label_style = Style::default().fg(rgb(th().dim_label));

    if let Some(status) = app.semantic_activity_status_text() {
        render_activity_status(frame, &status, area);
        return;
    }

    let (action_key, action_label) = if is_loading {
        (dim_key_style, dim_label_style)
    } else {
        (key_style, label_style)
    };

    let keys = app.keys();
    let mut spans = vec![
        Span::raw("  "),
        Span::styled("Enter", action_key),
        Span::styled(" open  ", action_label),
        Span::styled(keys.resume.short_label(), action_key),
        Span::styled(" resume  ", action_label),
        Span::styled(keys.fork.short_label(), action_key),
        Span::styled(" fork  ", action_label),
        Span::styled(keys.rename.short_label(), action_key),
        Span::styled(" rename  ", action_label),
        Span::styled(keys.delete.short_label(), action_key),
        Span::styled(" delete  ", action_label),
    ];

    // Scope toggle (only when project context exists)
    if app.has_project_context() {
        let scope_label = if app.workspace_filter() { "Prj" } else { "All" };
        let scope_val_style = if app.workspace_filter() {
            Style::default().fg(rgb(th().accent)).bold()
        } else {
            label_style
        };
        spans.extend([
            Span::styled("Tab", key_style),
            Span::styled("\u{b7}", label_style),
            Span::styled(scope_label, scope_val_style),
            Span::raw("  "),
        ]);
    }

    if app.semantic_toggle_available() {
        let mode_style = if app.list_search_mode() == ListSearchMode::Semantic {
            Style::default().fg(rgb(th().accent)).bold()
        } else {
            label_style
        };
        spans.extend([
            Span::styled("Ctrl+T", key_style),
            Span::styled(" semantic·", label_style),
            Span::styled(app.list_search_mode().label(), mode_style),
            Span::raw("  "),
        ]);
    }

    spans.extend([
        Span::styled("?", key_style),
        Span::styled("help  ", label_style),
        Span::styled("Esc", key_style),
        Span::styled(" quit", label_style),
    ]);

    let status_line = Line::from(spans);
    let status = Paragraph::new(status_line).style(Style::default().bg(rgb(th().status_bar_bg)));
    frame.render_widget(status, area);
}

fn semantic_rationale_label(metadata: &SemanticResultMetadata) -> &'static str {
    match metadata.explanation.rationale_kind {
        crate::semantic::types::SemanticRationaleKind::SemanticOnly => "semantic",
        crate::semantic::types::SemanticRationaleKind::LexicalBoosted => "lex boost",
        crate::semantic::types::SemanticRationaleKind::WeakMatch => "weak",
    }
}

fn semantic_row_metadata(metadata: &SemanticResultMetadata) -> String {
    format!("{:.2}", metadata.score_breakdown.hybrid)
}

fn render_active_filters_popup(frame: &mut Frame, app: &App) {
    let filters = app.active_filters();
    if filters.is_empty() {
        return;
    }
    // Laid out as the shortcuts overlay lays out keys and actions, so two
    // filters read as a column of terms rather than one run-on line.
    let label_width = filters
        .iter()
        .map(|filter| filter.label.chars().count())
        .max()
        .unwrap_or(0);
    let value_width = filters
        .iter()
        .map(|filter| filter.value.chars().count())
        .max()
        .unwrap_or(0);
    let popup = centered_modal_area(
        frame.area(),
        (label_width + value_width + 11) as u16,
        filters.len() as u16 + 4,
    );

    frame.render_widget(Clear, popup);
    let background = Block::default().style(Style::default().bg(rgb(th().overlay_bg)));
    frame.render_widget(background, popup);
    let block = Block::default()
        .title(" Active search filters ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(rgb(th().accent)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.is_empty() {
        return;
    }

    let mut lines = vec![Line::from("")];
    lines.extend(filters.iter().map(|filter| {
        let padding = label_width - filter.label.chars().count();
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{}{}", filter.label, " ".repeat(padding)),
                Style::default().fg(rgb(th().accent)),
            ),
            Span::styled(" │ ", Style::default().fg(rgb(th().border))),
            Span::styled(
                filter.value.clone(),
                Style::default().fg(rgb(th().text_primary)),
            ),
        ])
    }));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_semantic_debug_popup(frame: &mut Frame, app: &App) {
    let Some(metadata) = app.semantic_result_metadata_for_selection() else {
        return;
    };
    let area = frame.area();
    let popup = centered_modal_area(area, 68, 10);
    frame.render_widget(Clear, popup);
    let background = Block::default().style(Style::default().bg(rgb(th().overlay_bg)));
    frame.render_widget(background, popup);
    let block = Block::default()
        .title(" Semantic result ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(rgb(th().accent)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.is_empty() {
        return;
    }

    let score = &metadata.score_breakdown;
    let explanation = &metadata.explanation;
    let lines = vec![
        Line::from(vec![
            Span::styled(" hybrid ", Style::default().fg(rgb(th().text_muted))),
            Span::styled(
                format!("{:.2}", score.hybrid),
                Style::default().fg(rgb(th().accent)).bold(),
            ),
            Span::styled("  semantic ", Style::default().fg(rgb(th().text_muted))),
            Span::styled(
                format!("{:.2}", score.semantic),
                Style::default().fg(rgb(th().text_primary)),
            ),
            Span::styled("  lexical ", Style::default().fg(rgb(th().text_muted))),
            Span::styled(
                format!("{:.2}", score.lexical),
                Style::default().fg(rgb(th().text_primary)),
            ),
        ]),
        Line::from(vec![
            Span::styled(" rationale ", Style::default().fg(rgb(th().text_muted))),
            Span::styled(
                semantic_rationale_label(metadata),
                Style::default().fg(rgb(th().text_primary)),
            ),
            Span::styled("  quality ", Style::default().fg(rgb(th().text_muted))),
            Span::styled(
                explanation.quality_label,
                Style::default().fg(rgb(th().text_primary)),
            ),
        ]),
        Line::from(vec![
            Span::styled(" chunk ", Style::default().fg(rgb(th().text_muted))),
            Span::styled(
                format!(
                    "{} #{}",
                    explanation.chunk.session, explanation.chunk.chunk_index
                ),
                Style::default().fg(rgb(th().text_primary)),
            ),
        ]),
        Line::from(vec![
            Span::styled(" terms ", Style::default().fg(rgb(th().text_muted))),
            Span::styled(
                if explanation.matched_terms.is_empty() {
                    "(none)".to_string()
                } else {
                    explanation.matched_terms.join(", ")
                },
                Style::default().fg(rgb(th().text_primary)),
            ),
        ]),
        Line::from(vec![
            Span::styled(" preview ", Style::default().fg(rgb(th().text_muted))),
            Span::styled(
                simple_truncate(
                    &sanitize_preview(&explanation.evidence_preview),
                    inner.width.saturating_sub(10) as usize,
                ),
                Style::default().fg(rgb(th().preview)),
            ),
        ]),
        Line::from(""),
        Line::styled(" Esc close", Style::default().fg(rgb(th().text_muted))),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Check if the header (with summary) fits on a single line given terminal width
fn header_fits_single_line(conv: &crate::history::Conversation, terminal_width: u16) -> bool {
    let summary = match &conv.summary {
        Some(s) => s,
        None => return true, // No summary means it's already single line
    };

    let project = conv.project_name.as_deref().unwrap_or("Unknown");

    // Calculate custom title length if present
    let custom_title_len = conv
        .custom_title
        .as_ref()
        .map(|t| t.chars().count() + 3) // + " · "
        .unwrap_or(0);

    // Calculate model length if present
    let model_len = conv
        .model
        .as_ref()
        .map(|m| format_model_name(m).len() + 3) // + " · "
        .unwrap_or(0);

    let msg_count_len = if conv.message_count == 1 {
        "1 message".len()
    } else {
        format!("{} messages", conv.message_count).len()
    };

    // Calculate tokens length if present (use long form for single-line check)
    let tokens_len = if conv.total_tokens > 0 {
        format_tokens_long(conv.total_tokens).len() + 3 // + " · "
    } else {
        0
    };

    // timestamp is "YYYY-MM-DD HH:MM" = 16 chars
    let timestamp_len = 16;

    // Duration length (if present): " · Xm" or " · Xh Ym" etc.
    let duration_len = conv.duration_minutes.map_or(0, |minutes| {
        3 + format_coarse_duration(minutes * 60).len() // " · " + duration
    });

    // Format: "  project · custom_title · model · msg_count · duration · tokens · timestamp · summary"
    let total_len = 2
        + project.len()
        + 3
        + custom_title_len
        + model_len
        + msg_count_len
        + duration_len
        + 3
        + tokens_len
        + timestamp_len
        + 3
        + summary.len();

    total_len <= terminal_width as usize
}

#[derive(Clone, Copy, Debug)]
pub struct ViewLayoutRects {
    pub header: Rect,
    pub content: Rect,
    pub status: Rect,
}

pub fn view_layout_rects(area: Rect, app: &App, state: &ViewState) -> ViewLayoutRects {
    let status_height = if state.search_mode == ViewSearchMode::Typing {
        2
    } else {
        1
    };
    let conv = app
        .conversations()
        .iter()
        .find(|c| c.path == state.conversation_path);
    let has_summary = conv.is_some_and(|c| c.summary.is_some());
    let fits_single_line = conv.is_some_and(|c| header_fits_single_line(c, area.width));
    let header_height = if has_summary && !fits_single_line {
        3
    } else {
        2
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(1),
            Constraint::Length(status_height),
        ])
        .split(area);

    ViewLayoutRects {
        header: chunks[0],
        content: chunks[1],
        status: chunks[2],
    }
}

/// Render the view mode (conversation viewer)
fn render_view_mode(frame: &mut Frame, app: &App, state: &ViewState) {
    let layout = view_layout_rects(frame.area(), app, state);

    render_view_header(frame, app, state, layout.header);
    render_view_content(frame, state, layout.content);

    if state.search_mode == ViewSearchMode::Typing {
        render_search_input(frame, state, layout.status);
    } else {
        render_view_status_bar(frame, app, state, layout.status);
    }

    // Render dialog overlay if active
    match app.dialog_mode() {
        DialogMode::ConfirmDelete => render_confirm_dialog(frame, layout.status),
        DialogMode::ExportMenu { selected } => render_export_menu(frame, *selected, false),
        DialogMode::YankMenu { selected } => render_export_menu(frame, *selected, true),
        DialogMode::Help { scroll } => {
            render_help_overlay(
                frame,
                true,
                app.is_single_file_mode(),
                false,
                false,
                app.keys(),
                *scroll,
            );
        }
        DialogMode::SemanticDebug => render_semantic_debug_popup(frame, app),
        DialogMode::ActiveFilters => render_active_filters_popup(frame, app),
        DialogMode::Rename { input, cursor } => render_rename_dialog(frame, input, *cursor),
        DialogMode::None => {}
    }
}

fn render_view_header(frame: &mut Frame, app: &App, state: &ViewState, area: Rect) {
    // Find the conversation by path (works for both list and single file mode)
    let conv = app
        .conversations()
        .iter()
        .find(|c| c.path == state.conversation_path);

    let (
        project,
        custom_title,
        model,
        msg_count,
        duration,
        tokens,
        timestamp,
        summary,
        fits_single,
    ) = if let Some(conv) = conv {
        let project = conv.project_name.as_deref().unwrap_or("Unknown");
        let custom_title = conv.custom_title.clone();
        let model = conv.model.as_ref().map(|m| format_model_name(m));
        let msg_count = if conv.message_count == 1 {
            "1 message".to_string()
        } else {
            format!("{} messages", conv.message_count)
        };
        let duration = conv
            .duration_minutes
            .map(|minutes| format_coarse_duration(minutes * 60));

        // Calculate header length to determine if long token format fits
        let custom_title_len = custom_title
            .as_ref()
            .map(|t| t.chars().count() + 3)
            .unwrap_or(0); // + " · "
        let model_len = model.as_ref().map(|m| m.len() + 3).unwrap_or(0); // + " · "
        let duration_len = duration.as_ref().map(|d| d.len() + 3).unwrap_or(0); // + " · "
        let base_len = 2
            + project.len()
            + 3
            + custom_title_len
            + model_len
            + msg_count.len()
            + duration_len
            + 3
            + 16; // 16 = timestamp

        let tokens = if conv.total_tokens > 0 {
            let long_form = format_tokens_long(conv.total_tokens);
            let short_form = format_tokens(conv.total_tokens);
            // Use long form if it fits (base + " · " + tokens <= width)
            if base_len + 3 + long_form.len() <= area.width as usize {
                Some(long_form)
            } else {
                Some(short_form)
            }
        } else {
            None
        };

        let timestamp = conv.timestamp.format("%Y-%m-%d %H:%M").to_string();
        let fits = header_fits_single_line(conv, area.width);
        (
            project.to_string(),
            custom_title,
            model,
            msg_count,
            duration,
            tokens,
            timestamp,
            conv.summary.clone(),
            fits,
        )
    } else {
        // Fallback if parsing failed
        let project = state
            .conversation_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string();
        (
            project,
            None,
            None,
            "".to_string(),
            None,
            None,
            "".to_string(),
            None,
            true,
        )
    };

    // Build header spans for metadata line
    let build_metadata_spans = |include_summary: bool| {
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(
                project.clone(),
                Style::default().fg(rgb(th().accent)).bold(),
            ),
        ];

        // Add custom title if present
        if let Some(ref t) = custom_title {
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                t.clone(),
                Style::default().fg(rgb(th().custom_title)), // Warm gold
            ));
        }

        // Add model if present
        if let Some(ref m) = model {
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                m.clone(),
                Style::default().fg(rgb(th().model_color)),
            ));
        }

        spans.push(Span::raw(" · "));
        spans.push(Span::styled(
            msg_count.clone(),
            Style::default().fg(rgb(th().text_secondary)),
        ));

        // Add conversation duration if present
        if let Some(ref d) = duration {
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                d.clone(),
                Style::default().fg(rgb(th().duration_color)),
            ));
        }

        // Add tokens if present
        if let Some(ref t) = tokens {
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                t.clone(),
                Style::default().fg(rgb(th().text_secondary)),
            ));
        }

        spans.push(Span::raw(" · "));
        spans.push(Span::styled(
            timestamp.clone(),
            Style::default().fg(rgb(th().text_secondary)),
        ));

        // Add summary if requested
        if include_summary && let Some(ref s) = summary {
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                s.clone(),
                Style::default().fg(rgb(th().header_summary)),
            ));
        }

        spans
    };

    // Build header lines
    let lines = if fits_single && summary.is_some() {
        // Single line with summary
        vec![Line::from(build_metadata_spans(true))]
    } else {
        // Two lines (or single line without summary)
        let mut lines = vec![Line::from(build_metadata_spans(false))];

        // Add summary on second line if available
        if let Some(summary_text) = summary {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(summary_text, Style::default().fg(rgb(th().header_summary))),
            ]));
        }
        lines
    };

    let header = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(rgb(th().border))),
    );

    frame.render_widget(header, area);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GutterMark {
    /// Message navigation is off: no gutter column at all.
    Hidden,
    Clear,
    Focused,
    /// A row between the focused call's input and its result.
    FocusedGap,
    InsideRun,
}

fn gutter_mark(state: &ViewState, line_idx: usize) -> GutterMark {
    if !state.message_nav_active {
        return GutterMark::Hidden;
    }
    let Some(focus) = state.focus else {
        return GutterMark::Clear;
    };
    let in_focused_message = state
        .message_ranges
        .get(focus.message_index)
        .is_some_and(|message| (message.start_line..message.end_line).contains(&line_idx));
    if !in_focused_message {
        return GutterMark::Clear;
    }
    match focus.call_index.and_then(|idx| state.call_ranges.get(idx)) {
        None => GutterMark::Focused,
        Some(call) if call.contains_line(line_idx) => GutterMark::Focused,
        Some(call) if call.input_to_result_gap().contains(&line_idx) => GutterMark::FocusedGap,
        Some(_) => GutterMark::InsideRun,
    }
}

fn gutter_span(mark: GutterMark) -> Span<'static> {
    let (glyph, color) = match mark {
        GutterMark::Hidden => return Span::raw(""),
        GutterMark::Clear => return Span::raw("  "),
        GutterMark::Focused => ("▌ ", th().accent),
        GutterMark::FocusedGap => ("▏ ", th().accent),
        GutterMark::InsideRun => ("▏ ", th().text_muted),
    };
    Span::styled(glyph, Style::default().fg(rgb(color)))
}

fn render_view_content(frame: &mut Frame, state: &ViewState, area: Rect) {
    let visible_height = area.height as usize;
    let query_lower = state.search_query.to_lowercase();

    let visible_lines: Vec<Line> = state
        .rendered_lines
        .iter()
        .enumerate()
        .skip(state.scroll_offset)
        .take(visible_height)
        .map(|(line_idx, rendered)| {
            let is_current_match =
                state.search_matches.get(state.current_match_index) == Some(&line_idx);
            let has_match = !query_lower.is_empty() && state.search_matches.contains(&line_idx);

            let mut spans: Vec<Span> = vec![gutter_span(gutter_mark(state, line_idx))];

            if has_match && !query_lower.is_empty() {
                spans.extend(highlight_line_matches(
                    rendered,
                    &query_lower,
                    is_current_match,
                ));
            } else {
                spans.extend(
                    rendered
                        .spans
                        .iter()
                        .map(|(text, style)| styled_span(text, style)),
                );
            }

            let is_hovered = rendered
                .tool_output_id
                .as_ref()
                .is_some_and(|id| state.hovered_tool_output.as_ref() == Some(id));
            if is_hovered {
                let used_width: usize = spans
                    .iter()
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                    .sum();
                let padding = (area.width as usize).saturating_sub(used_width);
                if padding > 0 {
                    spans.push(Span::styled(
                        " ".repeat(padding),
                        Style::default().bg(rgb(th().selection_bg)),
                    ));
                }
            }

            let mut line = Line::from(spans);
            if is_hovered {
                line = line.style(Style::default().bg(rgb(th().selection_bg)));
            }

            line
        })
        .collect();

    let content = Paragraph::new(visible_lines);
    frame.render_widget(content, area);
}

fn render_view_status_bar(frame: &mut Frame, app: &App, state: &ViewState, area: Rect) {
    // Check for status message first
    if let Some((msg, instant)) = app.status_message()
        && instant.elapsed() < STATUS_TTL
    {
        let status_line = Line::from(vec![
            Span::raw("  "),
            Span::styled(msg, Style::default().fg(Color::Green)),
        ]);
        let status =
            Paragraph::new(status_line).style(Style::default().bg(rgb(th().status_bar_bg)));
        frame.render_widget(status, area);
        return;
    }

    // Fixed-width scroll position to prevent bar from jumping
    // Use minimum width of 4 for both numbers to handle most conversations
    let total = state.total_lines.max(1);
    let width = total.to_string().len().max(4);
    let scroll_pos = format!("[{:>width$}/{:<width$}]", state.scroll_offset + 1, total);

    let key_style = Style::default().fg(rgb(th().accent));
    let label_style = Style::default().fg(rgb(th().text_muted));

    // Fixed-width status labels to prevent jumping when toggling
    let tools_status = state.tool_display.status_label();
    let thinking_status = if state.show_thinking { "on " } else { "off" };
    let timing_status = if state.show_timing { "on " } else { "off" };

    let mut spans = vec![
        Span::raw("  "),
        Span::styled(scroll_pos, Style::default().fg(rgb(th().text_secondary))),
        Span::raw("  "),
        Span::styled("t", key_style),
        Span::styled(format!("ools·{} ", tools_status), label_style),
        Span::styled("T", key_style),
        Span::styled(format!("hink·{} ", thinking_status), label_style),
        Span::styled("i", key_style),
        Span::styled(format!("nfo·{}", timing_status), label_style),
        Span::raw("  "),
        Span::styled("│", label_style),
        Span::raw("  "),
    ];

    if state.search_mode == ViewSearchMode::Active && !state.search_matches.is_empty() {
        spans.extend([
            Span::styled("n", key_style),
            Span::styled("ext  ", label_style),
            Span::styled("N", key_style),
            Span::styled("prev  ", label_style),
            Span::styled(
                format!(
                    "{}/{}  ",
                    state.current_match_index + 1,
                    state.search_matches.len()
                ),
                Style::default().fg(rgb(th().text_secondary)),
            ),
            Span::styled("Esc", key_style),
            Span::styled(" clear", label_style),
        ]);
    } else {
        spans.extend([
            Span::styled("?", key_style),
            Span::styled("help  ", label_style),
            Span::styled("/", key_style),
            Span::styled("search  ", label_style),
            Span::styled("e", key_style),
            Span::styled("xport  ", label_style),
            Span::styled("y", key_style),
            Span::styled("ank  ", label_style),
            Span::styled(app.keys().resume.short_label(), key_style),
            Span::styled(" resume  ", label_style),
            Span::styled(app.keys().fork.short_label(), key_style),
            Span::styled(" fork  ", label_style),
            Span::styled(app.keys().delete.short_label(), key_style),
            Span::styled(" del  ", label_style),
            Span::styled("q", key_style),
            Span::styled("uit", label_style),
        ]);
    }

    let status_line = Line::from(spans);
    let status = Paragraph::new(status_line).style(Style::default().bg(rgb(th().status_bar_bg)));
    frame.render_widget(status, area);
}

fn render_search_input(frame: &mut Frame, state: &ViewState, area: Rect) {
    let match_info = if state.search_matches.is_empty() {
        if state.search_query.is_empty() {
            String::new()
        } else {
            " (no matches)".to_string()
        }
    } else {
        format!(
            " ({}/{})",
            state.current_match_index + 1,
            state.search_matches.len()
        )
    };

    let input_line = Line::from(vec![
        Span::raw("  /"),
        Span::styled(
            &state.search_query,
            Style::default().fg(rgb(th().text_primary)),
        ),
        Span::styled(match_info, Style::default().fg(rgb(th().text_secondary))),
    ]);

    let input = Paragraph::new(input_line).style(Style::default().bg(rgb(th().status_bar_bg)));
    frame.render_widget(input, area);

    // Position cursor (account for "  /" prefix = 3 columns)
    let query_width: usize = state
        .search_query
        .chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();
    let max_x = area.x + area.width.saturating_sub(1);
    let cursor_x = (area.x + 3 + query_width.min(u16::MAX as usize) as u16).min(max_x);
    frame.set_cursor_position(Position::new(cursor_x, area.y));
}

/// Highlight search matches across the full line text, handling matches that span
/// across multiple styled spans. Works by finding match positions in the concatenated
/// line text, then rebuilding spans with highlights applied at the correct positions.
fn highlight_line_matches(
    rendered: &RenderedLine,
    query: &str,
    is_current_match: bool,
) -> Vec<Span<'static>> {
    // Concatenate all span texts to get the full line
    let full_text: String = rendered
        .spans
        .iter()
        .map(|(text, _)| text.as_str())
        .collect();
    let full_lower = full_text.to_lowercase();

    // Find match positions using char indices to safely handle Unicode
    // (lowercasing can change byte lengths for some characters)
    let orig_chars: Vec<(usize, char)> = full_text.char_indices().collect();
    let lower_chars: Vec<char> = full_lower.chars().collect();
    let query_chars: Vec<char> = query.chars().collect();

    let mut match_byte_ranges: Vec<(usize, usize)> = Vec::new();
    if !query_chars.is_empty() {
        let mut i = 0;
        while i + query_chars.len() <= lower_chars.len() {
            if lower_chars[i..i + query_chars.len()] == query_chars[..] {
                // Guard against Unicode casing expansion (e.g. ß → ss) where
                // lower_chars may be longer than orig_chars
                if i >= orig_chars.len() {
                    break;
                }
                let start_byte = orig_chars[i].0;
                let end_byte = if i + query_chars.len() < orig_chars.len() {
                    orig_chars[i + query_chars.len()].0
                } else {
                    full_text.len()
                };
                match_byte_ranges.push((start_byte, end_byte));
                i += query_chars.len();
            } else {
                i += 1;
            }
        }
    }

    if match_byte_ranges.is_empty() {
        return rendered
            .spans
            .iter()
            .map(|(t, s)| styled_span(t, s))
            .collect();
    }

    let match_style = if is_current_match {
        Style::default().bg(Color::Yellow).fg(Color::Black)
    } else {
        Style::default()
            .bg(rgb(th().search_match_bg))
            .fg(Color::Black)
    };

    // Build output spans by walking through original spans and splitting at match boundaries
    let mut result: Vec<Span<'static>> = Vec::new();
    let mut match_idx = 0;
    let mut global_offset: usize = 0;

    for (text, style) in &rendered.spans {
        let span_start = global_offset;
        let span_end = global_offset + text.len();
        let base_style = build_style(style);
        let mut pos = span_start;

        while pos < span_end {
            // Skip past matches that are entirely before our position
            while match_idx < match_byte_ranges.len() && match_byte_ranges[match_idx].1 <= pos {
                match_idx += 1;
            }

            if match_idx < match_byte_ranges.len() {
                let (ms, me) = match_byte_ranges[match_idx];
                if pos >= ms && pos < me {
                    // Inside a match
                    let end = me.min(span_end);
                    result.push(Span::styled(full_text[pos..end].to_string(), match_style));
                    pos = end;
                } else if ms < span_end {
                    // There's a match starting within this span, emit text before it
                    let end = ms.min(span_end);
                    if end > pos {
                        result.push(Span::styled(full_text[pos..end].to_string(), base_style));
                    }
                    pos = end;
                } else {
                    // No more matches in this span
                    result.push(Span::styled(
                        full_text[pos..span_end].to_string(),
                        base_style,
                    ));
                    pos = span_end;
                }
            } else {
                // No more matches at all
                result.push(Span::styled(
                    full_text[pos..span_end].to_string(),
                    base_style,
                ));
                pos = span_end;
            }
        }

        global_offset = span_end;
    }

    result
}

fn build_style(style: &LineStyle) -> Style {
    let mut s = Style::default();
    if let Some((r, g, b)) = style.fg {
        s = s.fg(Color::Rgb(r, g, b));
    }
    if style.bold {
        s = s.bold();
    }
    if style.italic {
        s = s.italic();
    }
    if style.dimmed {
        s = s.fg(rgb(th().text_muted));
    }
    s
}

fn styled_span(text: &str, style: &LineStyle) -> Span<'static> {
    Span::styled(text.to_string(), build_style(style))
}

fn loading_status(loaded: usize, progress: Option<&LoadProgress>) -> String {
    match progress {
        Some(progress) => {
            let unit = match progress.unit {
                LoadUnit::Sessions => "sessions",
                LoadUnit::Projects => "projects",
            };
            format!(
                "Loading {} {}/{} {unit} · {loaded} loaded",
                progress.source.display_label(),
                progress.done,
                progress.total
            )
        }
        None => format!("Loading... {loaded}"),
    }
}

fn render_search_bar(frame: &mut Frame, app: &App, area: Rect) {
    let count_text = match app.loading_state() {
        LoadingState::Loading { loaded, progress } => loading_status(*loaded, progress.as_ref()),
        LoadingState::Ready => match app.selected() {
            Some(selected) => format!("{}/{}", selected + 1, app.filtered().len()),
            None => format!("0/{}", app.filtered().len()),
        },
    };
    let status_text = if app.semantic_search_available() {
        app.semantic_status_text()
            .map(|status| {
                format!(
                    "{} {} {}",
                    app.list_search_mode().label(),
                    count_text,
                    status
                )
            })
            .unwrap_or_else(|| format!("{} {}", app.list_search_mode().label(), count_text))
    } else {
        count_text
    };

    let prompt_style = Style::default().fg(rgb(th().accent));
    let (prompt_spans, prefix_width) = if app.workspace_filter() {
        (
            vec![
                Span::raw(" "),
                Span::styled("Project", Style::default().fg(rgb(th().text_muted))),
                Span::raw(" "),
                Span::styled("\u{276F} ", prompt_style),
            ],
            11,
        )
    } else {
        (
            vec![Span::raw(" "), Span::styled("\u{276F} ", prompt_style)],
            3,
        )
    };

    let status_style = if app.is_loading() {
        Style::default().fg(rgb(th().accent))
    } else {
        Style::default().fg(rgb(th().text_muted))
    };
    let available = area.width as usize;
    let min_gap = usize::from(available > prefix_width);
    let right_budget = available.saturating_sub(prefix_width + min_gap);
    let rendered_status = simple_truncate(&status_text, right_budget);
    let status_width = UnicodeWidthStr::width(rendered_status.as_str());
    // The count alone cannot say whether it counts the whole history, and the
    // filters are too wide to sit here, so this cues the key that lists them.
    // Dropped rather than cut where the bar is narrow, as the transient
    // semantic status is.
    let cue_fits = !app.active_filters().is_empty()
        && right_budget >= status_width + UnicodeWidthStr::width(FILTER_CUE);
    let right_width = status_width
        + if cue_fits {
            UnicodeWidthStr::width(FILTER_CUE)
        } else {
            0
        }
        + usize::from(!rendered_status.is_empty());
    let query_budget = available.saturating_sub(prefix_width + right_width + min_gap);
    let rendered_query = simple_truncate(app.query(), query_budget);
    let query_width = UnicodeWidthStr::width(rendered_query.as_str());
    let padding = available.saturating_sub(prefix_width + query_width + right_width);

    let query_style = if app.is_session_id_query() {
        Style::default().fg(rgb(th().session_id))
    } else {
        Style::default()
    };
    let mut spans = prompt_spans;
    spans.extend([
        Span::styled(rendered_query, query_style),
        Span::raw(" ".repeat(padding)),
        Span::styled(rendered_status, status_style),
    ]);
    if cue_fits {
        spans.extend([
            Span::styled(" · ", Style::default().fg(rgb(th().text_muted))),
            Span::styled("^L", Style::default().fg(rgb(th().accent))),
            Span::styled(" filters", Style::default().fg(rgb(th().text_muted))),
        ]);
    }
    spans.push(Span::raw(" "));
    let search_line = Line::from(spans);

    let input = Paragraph::new(search_line).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(rgb(th().border))),
    );

    frame.render_widget(input, area);

    if area.width > prefix_width as u16 {
        let cursor_offset: u16 = app
            .query()
            .chars()
            .take(app.cursor_pos())
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum::<usize>()
            .min(query_budget)
            .min(u16::MAX as usize) as u16;
        let max_x = area
            .x
            .saturating_add(prefix_width as u16)
            .saturating_add(query_budget.min(u16::MAX as usize) as u16);
        let cursor_x = (area.x + prefix_width as u16)
            .saturating_add(cursor_offset)
            .min(max_x)
            .min(area.x + area.width.saturating_sub(1));
        frame.set_cursor_position(Position::new(cursor_x, area.y));
    }
}

fn centered_modal_area(area: Rect, preferred_width: u16, preferred_height: u16) -> Rect {
    let width = preferred_width.min(area.width);
    let height = preferred_height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn render_confirm_dialog(frame: &mut Frame, area: Rect) {
    let prompt = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "Delete this conversation? ",
            Style::default().fg(Color::Yellow),
        ),
        Span::styled("(y/n)", Style::default().fg(rgb(th().text_secondary))),
    ]);
    let paragraph = Paragraph::new(prompt);
    frame.render_widget(paragraph, area);
}

fn render_rename_dialog(frame: &mut Frame, input: &str, cursor: usize) {
    let area = frame.area();
    let menu_width = area.width.saturating_sub(4).clamp(30, 70);
    let menu_height = 4;
    let menu_area = Rect {
        x: (area.width.saturating_sub(menu_width)) / 2,
        y: (area.height.saturating_sub(menu_height)) / 2,
        width: menu_width,
        height: menu_height,
    };

    frame.render_widget(Clear, menu_area);
    let background = Block::default().style(Style::default().bg(rgb(th().overlay_bg)));
    frame.render_widget(background, menu_area);

    let block = Block::default()
        .title(" Rename session ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(rgb(th().accent)));
    let inner = block.inner(menu_area);
    frame.render_widget(block, menu_area);

    let input_width = inner.width.saturating_sub(2) as usize;
    let display = simple_truncate(input, input_width);
    let lines = vec![
        Line::from(vec![
            Span::raw(" "),
            Span::styled(display, Style::default().fg(rgb(th().text_primary))),
        ]),
        Line::styled(
            " Enter save · Esc cancel",
            Style::default().fg(rgb(th().text_muted)),
        ),
    ];
    frame.render_widget(Paragraph::new(lines), inner);

    let cursor_offset: u16 = input
        .chars()
        .take(cursor)
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum::<usize>()
        .min(input_width) as u16;
    frame.set_cursor_position(Position::new(
        inner.x.saturating_add(1).saturating_add(cursor_offset),
        inner.y,
    ));
}

fn render_export_menu(frame: &mut Frame, selected: usize, is_yank: bool) {
    let title = if is_yank {
        "Copy to clipboard"
    } else {
        "Export to file"
    };
    let options = [
        "[1] Ledger (formatted)",
        "[2] Plain text",
        "[3] Markdown",
        "[4] JSONL (raw)",
    ];

    let area = frame.area();
    let menu_width = 35;
    let menu_height = options.len() as u16 + 4; // options + title + border + cancel hint

    let menu_area = centered_modal_area(area, menu_width, menu_height);

    // Clear the area behind the modal first
    frame.render_widget(Clear, menu_area);

    // Render background
    let background = Block::default().style(Style::default().bg(rgb(th().overlay_bg)));
    frame.render_widget(background, menu_area);

    // Render border
    let block = Block::default()
        .title(format!(" {} ", title))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(rgb(th().accent)));

    let inner = block.inner(menu_area);
    frame.render_widget(block, menu_area);

    // Render options
    let mut lines = Vec::new();
    for (i, opt) in options.iter().enumerate() {
        let style = if i == selected {
            Style::default().fg(rgb(th().accent)).bold()
        } else {
            Style::default().fg(rgb(th().text_primary))
        };
        let prefix = if i == selected { "▶ " } else { "  " };
        lines.push(Line::styled(format!("{}{}", prefix, opt), style));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "  [Esc] Cancel",
        Style::default().fg(rgb(th().text_muted)),
    ));

    if inner.is_empty() {
        return;
    }

    let menu_content = Paragraph::new(lines);
    frame.render_widget(menu_content, inner);
}

fn render_help_overlay(
    frame: &mut Frame,
    is_view_mode: bool,
    is_single_file_mode: bool,
    semantic_available: bool,
    has_active_filters: bool,
    keys: &KeyBindings,
    scroll: usize,
) {
    let exit_text = if is_single_file_mode {
        "Quit"
    } else {
        "Back to list"
    };

    let shortcuts: Vec<(String, &str)> = if is_view_mode {
        vec![
            ("j / ↓".into(), "Scroll down"),
            ("k / ↑".into(), "Scroll up"),
            ("Wheel".into(), "Scroll"),
            ("Click".into(), "Expand / collapse tool row"),
            ("J / ]".into(), "Next message / call"),
            ("K / [".into(), "Previous message / call"),
            ("Enter".into(), "Expand / collapse run or call"),
            ("→".into(), "Expand run or call"),
            ("←".into(), "Collapse / leave call or run"),
            ("d / Ctrl+D".into(), "Half page down"),
            ("u / Ctrl+U".into(), "Half page up"),
            ("g / Home".into(), "Jump to top"),
            ("G / End".into(), "Jump to bottom"),
            ("/".into(), "Search"),
            ("n / N".into(), "Next / prev match"),
            ("t".into(), "Cycle tools: off/trunc/full"),
            ("T".into(), "Toggle thinking"),
            ("i".into(), "Toggle timing"),
            ("e".into(), "Export to file"),
            ("y".into(), "Copy message / call, or menu"),
            ("p".into(), "Show file path"),
            ("Y".into(), "Copy path"),
            ("I".into(), "Copy session ID"),
            (keys.resume.help_label(), "Resume"),
            (keys.fork.help_label(), "Fork resume"),
            (keys.delete.help_label(), "Delete"),
            ("q / Esc".into(), exit_text),
            ("Ctrl+C".into(), "Quit"),
        ]
    } else {
        let mut shortcuts = vec![
            ("↑ / ↓".into(), "Move selection"),
            ("← / →".into(), "Move cursor"),
            ("Ctrl+P / N".into(), "Move selection"),
            ("Wheel".into(), "Scroll the list"),
            ("Click".into(), "Open conversation"),
            ("Ctrl+D".into(), "Half page down"),
            ("Ctrl+U".into(), "Kill to start of line"),
            ("Ctrl+K".into(), "Kill to end of line"),
            ("PgUp / PgDn".into(), "Jump by page"),
            ("Home / End".into(), "Jump to first/last"),
            ("Tab".into(), "Toggle scope (All/Project)"),
        ];
        if semantic_available {
            shortcuts.push(("Ctrl+T".into(), "Toggle semantic search"));
            shortcuts.push(("Ctrl+S".into(), "Semantic details"));
        }
        if has_active_filters {
            shortcuts.push(("Ctrl+L".into(), "List active filters"));
        }
        shortcuts.extend([
            ("Enter".into(), "Open viewer"),
            ("Ctrl+O".into(), "Select and exit"),
            ("Ctrl+W".into(), "Delete word"),
            (keys.resume.help_label(), "Resume"),
            (keys.fork.help_label(), "Fork resume"),
            (keys.rename.help_label(), "Rename"),
            (keys.delete.help_label(), "Delete"),
            ("Esc".into(), "Clear search, or quit"),
            ("Ctrl+C".into(), "Quit"),
        ]);
        shortcuts
    };

    let title = " Shortcuts ";

    let area = frame.area();
    // Calculate dimensions based on content (use chars().count() for Unicode)
    let max_key_len = shortcuts
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    let max_action_len = shortcuts
        .iter()
        .map(|(_, a)| a.chars().count())
        .max()
        .unwrap_or(0);
    // Padding: 2 chars left + key + " │ " (3) + action + 2 chars right
    let menu_width = (max_key_len + max_action_len + 11) as u16;
    // Height: 1 top padding + shortcuts + 1 bottom padding + 2 border
    let menu_height = shortcuts.len() as u16 + 4;

    let menu_area = centered_modal_area(area, menu_width, menu_height);

    // Clear the area behind the modal
    frame.render_widget(Clear, menu_area);

    // Render background
    let background = Block::default().style(Style::default().bg(rgb(th().overlay_bg)));
    frame.render_widget(background, menu_area);

    // Render border
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(rgb(th().accent)));

    let inner = block.inner(menu_area);
    frame.render_widget(block, menu_area);

    if inner.is_empty() {
        return;
    }

    let content_height = inner.height as usize;
    let indicator_needed = shortcuts.len() > content_height;
    let shortcut_rows = if indicator_needed {
        content_height.saturating_sub(1)
    } else {
        content_height
    };
    let max_scroll = shortcuts.len().saturating_sub(shortcut_rows);
    let scroll = scroll.min(max_scroll);

    let mut lines = Vec::new();
    if !indicator_needed {
        lines.extend(
            (0..content_height.saturating_sub(shortcuts.len()) / 2).map(|_| Line::from("")),
        );
    }
    for (key, action) in shortcuts.iter().skip(scroll).take(shortcut_rows) {
        let key_padding = max_key_len - key.chars().count();
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{}{}", key, " ".repeat(key_padding)),
                Style::default().fg(rgb(th().accent)),
            ),
            Span::styled(" │ ", Style::default().fg(rgb(th().border))),
            Span::styled(
                action.to_string(),
                Style::default().fg(rgb(th().text_primary)),
            ),
        ]));
    }

    if indicator_needed && content_height > 0 {
        let start = scroll + 1;
        let end = (scroll + shortcut_rows).min(shortcuts.len());
        let indicator = match (scroll > 0, scroll < max_scroll) {
            (true, true) => format!("  ↑↓ more  {start}-{end}/{}", shortcuts.len()),
            (true, false) => format!("  ↑ more  {start}-{end}/{}", shortcuts.len()),
            (false, true) => format!("  ↓ more  {start}-{end}/{}", shortcuts.len()),
            (false, false) => format!("  {start}-{end}/{}", shortcuts.len()),
        };
        lines.push(Line::styled(
            indicator,
            Style::default().fg(rgb(th().text_muted)),
        ));
    }

    let content = Paragraph::new(lines);
    frame.render_widget(content, inner);
}

fn render_unresolved_session_id(frame: &mut Frame, session_id: &str, area: Rect) {
    let muted = Style::default().fg(rgb(th().text_muted));
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("No session with ID ", muted),
            Span::styled(session_id, Style::default().fg(rgb(th().session_id))),
            Span::styled(" found", muted),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("To search transcripts for it as text, quote it: ", muted),
            Span::styled(
                format!("\"{session_id}\""),
                Style::default().fg(rgb(th().accent)),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_list(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(session_id) = app.unresolved_session_id() {
        render_unresolved_session_id(frame, session_id, area);
        return;
    }

    let width = area.width as usize;
    let highlight_query = HighlightQuery::parse(app.query());
    let frame_inputs = ListFrameInputs {
        width,
        query: &highlight_query,
        search_mode: app.list_search_mode(),
        source_label_column: app
            .has_multiple_sources()
            .then(crate::history::provider::list_label_column_width),
        now: Local::now(),
    };

    let lines_per_item = list_lines_per_item(app.list_search_mode(), app.query());
    let items_per_page = (area.height as usize) / lines_per_item;
    let offset = match (app.selected(), items_per_page) {
        (Some(sel), n) if n > 0 => (sel / n) * n,
        _ => 0,
    };
    let visible_count = items_per_page.max(1);
    let separator = "─".repeat(width);

    let visible_items: Vec<ListItem> = app
        .filtered()
        .iter()
        .skip(offset)
        .take(visible_count)
        .enumerate()
        .map(|(relative_idx, &conv_idx)| {
            let conv = &app.conversations()[conv_idx];
            let row = build_list_row(
                conv,
                &frame_inputs,
                app.semantic_result_metadata(conv_idx),
                app.lexical_evidence(conv_idx),
            );
            let is_selected = app.selected() == Some(offset + relative_idx);
            style_list_row(row, &highlight_query, is_selected, &separator)
        })
        .collect();

    let list = List::new(visible_items);
    frame.render_widget(list, area);
}

/// Opens every line of a row. One string, so the width it is budgeted and
/// the cells it renders as cannot disagree.
const ROW_INDICATOR: &str = " ▌ ";

/// The inputs every row of one frame shares.
struct ListFrameInputs<'a> {
    width: usize,
    query: &'a HighlightQuery,
    search_mode: ListSearchMode,
    /// The source label column's width when more than one source is listed;
    /// `None` leaves the project name unlabelled.
    source_label_column: Option<usize>,
    now: DateTime<Local>,
}

/// What one row shows at its width, each part truncated to its budget,
/// before any style is applied.
struct ListRow {
    project: String,
    /// `None` when the title does not fit.
    custom_title_with_separator: Option<String>,
    /// `None` when the summary does not fit.
    summary_with_separator: Option<String>,
    /// Blank columns between the left parts and the right-aligned metadata.
    padding: usize,
    message_count: String,
    semantic_metadata: Option<String>,
    duration: Option<String>,
    timestamp: String,
    recency: Recency,
    preview: String,
    /// The surroundings of a literal match the preview hides, on a line of
    /// its own.
    context: Option<String>,
}

fn build_list_row(
    conv: &Conversation,
    frame_inputs: &ListFrameInputs,
    semantic_metadata: Option<&SemanticResultMetadata>,
    lexical_evidence: Option<&LexicalEvidence>,
) -> ListRow {
    let width = frame_inputs.width;
    let semantic_mode = frame_inputs.search_mode == ListSearchMode::Semantic;

    let (timestamp, recency) = format_timestamp(conv.timestamp, frame_inputs.now);

    let message_count = if conv.message_count == 1 {
        "1 msg".to_string()
    } else {
        format!("{} msgs", conv.message_count)
    };

    let duration = conv
        .duration_minutes
        .map(|minutes| format_coarse_duration(minutes * 60));

    let semantic_metadata_part = (semantic_mode && width >= 70)
        .then(|| semantic_metadata.map(semantic_row_metadata))
        .flatten();
    let semantic_meta_len = semantic_metadata_part
        .as_ref()
        .map(|s| UnicodeWidthStr::width(s.as_str()) + 3)
        .unwrap_or(0);

    let duration_len = duration
        .as_ref()
        .map(|d| UnicodeWidthStr::width(d.as_str()) + 3)
        .unwrap_or(0);
    let right_len = UnicodeWidthStr::width(message_count.as_str())
        + duration_len
        + semantic_meta_len
        + 3
        + UnicodeWidthStr::width(timestamp.as_str());
    let indicator_len = UnicodeWidthStr::width(ROW_INDICATOR);
    let min_padding = 3;
    let left_budget = width.saturating_sub(indicator_len + right_len + min_padding);

    let raw_project_part = conv
        .project_name
        .as_ref()
        .map(|name| match frame_inputs.source_label_column {
            Some(column) => format!(
                "{:<width$} · {name}",
                conv.source.list_label(),
                width = column
            ),
            None => name.to_string(),
        })
        .unwrap_or_default();
    let has_title_or_summary = conv.custom_title.as_ref().is_some_and(|s| !s.is_empty())
        || conv.summary.as_ref().is_some_and(|s| !s.is_empty());
    let raw_project_width = UnicodeWidthStr::width(raw_project_part.as_str());
    let reserved_left_detail = if width < 90 && has_title_or_summary {
        (left_budget / 3).clamp(10, 24)
    } else {
        0
    };
    let project_budget = raw_project_width.min(left_budget.saturating_sub(reserved_left_detail));
    let project = simple_truncate(&raw_project_part, project_budget);
    let project_len = UnicodeWidthStr::width(project.as_str());

    let title_budget = left_budget.saturating_sub(project_len + 3);
    let custom_title_with_separator = conv
        .custom_title
        .as_ref()
        .filter(|s| !s.is_empty() && title_budget > 4)
        .map(|s| format!(" · {}", simple_truncate(s, title_budget)));
    let custom_title_len = custom_title_with_separator
        .as_ref()
        .map(|s| UnicodeWidthStr::width(s.as_str()))
        .unwrap_or(0);

    let available_for_summary = width.saturating_sub(
        indicator_len + project_len + custom_title_len + right_len + min_padding + 4,
    );
    let summary_with_separator = conv
        .summary
        .as_ref()
        .filter(|s| !s.is_empty() && available_for_summary > 5)
        .map(|s| {
            if UnicodeWidthStr::width(s.as_str()) > available_for_summary {
                format!(" · {}", simple_truncate(s, available_for_summary))
            } else {
                format!(" · {}", s)
            }
        });

    let left_len = indicator_len
        + project_len
        + custom_title_len
        + summary_with_separator
            .as_ref()
            .map(|s| UnicodeWidthStr::width(s.as_str()))
            .unwrap_or(0);
    let padding = width.saturating_sub(left_len + right_len + 1);

    let (preview, context) =
        preview_and_context(conv, frame_inputs, semantic_metadata, lexical_evidence);

    ListRow {
        project,
        custom_title_with_separator,
        summary_with_separator,
        padding,
        message_count,
        semantic_metadata: semantic_metadata_part,
        duration,
        timestamp,
        recency,
        preview,
        context,
    }
}

/// The preview line and, when a literal match hides outside it, the context
/// line, each truncated to the row's width.
fn preview_and_context(
    conv: &Conversation,
    frame_inputs: &ListFrameInputs,
    semantic_metadata: Option<&SemanticResultMetadata>,
    lexical_evidence: Option<&LexicalEvidence>,
) -> (String, Option<String>) {
    let width = frame_inputs.width;
    let semantic_mode = frame_inputs.search_mode == ListSearchMode::Semantic;
    let query_normalized = frame_inputs.query.context_text();

    let max_preview_len = width.saturating_sub(4);
    let lexical_evidence =
        lexical_evidence.filter(|_| !semantic_mode || semantic_metadata.is_none());
    let lexical_context = lexical_evidence.and_then(|evidence| {
        build_context_segments_from_ranges(
            &conv.full_text,
            &evidence.context_ranges,
            max_preview_len,
        )
    });
    let semantic_preview = semantic_metadata
        .filter(|_| semantic_mode && !query_normalized.is_empty())
        .map(|metadata| sanitize_preview(&metadata.explanation.evidence_preview));
    let preview_text = if let Some(preview) = semantic_preview {
        preview
    } else if let Some(context) = lexical_context.as_ref() {
        context.clone()
    } else {
        sanitize_preview(&conv.preview)
    };
    let preview = if query_normalized.is_empty() {
        simple_truncate(&preview_text, max_preview_len)
    } else if semantic_mode && frame_inputs.query.has_match(&preview_text) {
        build_match_segments_for_query(&preview_text, frame_inputs.query, max_preview_len)
    } else if semantic_mode || lexical_context.is_some() {
        simple_truncate(&preview_text, max_preview_len)
    } else {
        build_match_segments(&preview_text, &query_normalized, max_preview_len)
    };

    let context = if lexical_context.is_none() && frame_inputs.query.needs_literal_context(&preview)
    {
        build_literal_context_segments(
            &conv.full_text,
            &preview,
            frame_inputs.query,
            width.saturating_sub(4),
        )
    } else {
        None
    };

    (preview, context)
}

/// The row's three lines, or four with a context line, with the theme's
/// styles applied and the query's matches highlighted.
fn style_list_row<'a>(
    row: ListRow,
    query: &HighlightQuery,
    is_selected: bool,
    separator: &'a str,
) -> ListItem<'a> {
    let indicator_style = if is_selected {
        Style::default().fg(rgb(th().accent))
    } else {
        Style::default().fg(rgb(th().border))
    };
    let project_style = if is_selected {
        Style::default().fg(rgb(th().text_primary)).bold()
    } else {
        Style::default().fg(rgb(th().text_primary))
    };
    let summary_style = Style::default().fg(rgb(th().summary));
    let summary_highlight_style = Style::default().fg(rgb(th().summary_highlight));
    let highlight_style = if is_selected {
        Style::default().fg(rgb(th().accent)).bold()
    } else {
        Style::default().fg(rgb(th().accent))
    };
    let selection_bg = if is_selected {
        Style::default().bg(rgb(th().selection_bg))
    } else {
        Style::default()
    };
    let custom_title_style = Style::default().fg(rgb(th().custom_title));
    let custom_title_highlight_style = Style::default().fg(rgb(th().custom_title_highlight));
    let dot_separator_style = Style::default().fg(rgb(th().dot_separator));

    let mut header_spans = vec![Span::styled(ROW_INDICATOR, indicator_style)];
    header_spans.extend(highlight(
        query,
        &row.project,
        project_style,
        highlight_style,
    ));
    if let Some(ref title) = row.custom_title_with_separator {
        header_spans.extend(highlight(
            query,
            title,
            custom_title_style,
            custom_title_highlight_style,
        ));
    }
    if let Some(ref summary) = row.summary_with_separator {
        header_spans.extend(highlight(
            query,
            summary,
            summary_style,
            summary_highlight_style,
        ));
    }
    header_spans.push(Span::raw(" ".repeat(row.padding)));
    header_spans.push(Span::styled(
        row.message_count,
        Style::default().fg(rgb(th().msg_count)),
    ));
    if let Some(metadata_text) = row.semantic_metadata {
        header_spans.push(Span::styled(" · ", dot_separator_style));
        header_spans.push(Span::styled(
            metadata_text,
            Style::default().fg(rgb(th().accent)),
        ));
    }
    if let Some(duration) = row.duration {
        header_spans.push(Span::styled(" · ", dot_separator_style));
        header_spans.push(Span::styled(
            duration,
            Style::default().fg(rgb(th().duration_color)),
        ));
    }
    header_spans.push(Span::styled(" · ", dot_separator_style));
    let timestamp_color = match row.recency {
        Recency::Now => th().timestamp_now,
        Recency::Minutes => th().timestamp_minutes,
        Recency::Hours => th().timestamp_hours,
        Recency::Days => th().timestamp_days,
        Recency::Old => th().text_secondary,
    };
    header_spans.push(Span::styled(
        row.timestamp,
        Style::default().fg(rgb(timestamp_color)),
    ));
    let header = Line::from(header_spans).style(selection_bg);

    let mut preview_spans = vec![Span::styled(ROW_INDICATOR, indicator_style)];
    preview_spans.extend(highlight(
        query,
        &row.preview,
        Style::default().fg(rgb(th().preview)),
        highlight_style,
    ));
    let preview = Line::from(preview_spans).style(selection_bg);

    let context_line = row.context.map(|context_text| {
        let mut context_spans = vec![Span::styled(ROW_INDICATOR, indicator_style)];
        context_spans.extend(highlight(
            query,
            &context_text,
            Style::default().fg(rgb(th().context_base)),
            Style::default().fg(rgb(th().context_highlight)),
        ));
        Line::from(context_spans).style(selection_bg)
    });

    let separator = Line::from(Span::styled(
        separator,
        Style::default().fg(rgb(th().separator)),
    ));

    let lines = if let Some(context) = context_line {
        vec![header, preview, context, separator]
    } else {
        vec![header, preview, separator]
    };
    ListItem::new(lines)
}

/// Recency level for timestamp color grading
enum Recency {
    Now,
    Minutes,
    Hours,
    Days,
    Old,
}

/// Format a timestamp as relative time for recent entries, absolute for older ones.
/// Returns (formatted_string, recency) for color grading.
fn format_timestamp(timestamp: DateTime<Local>, now: DateTime<Local>) -> (String, Recency) {
    let age = now.signed_duration_since(timestamp);

    // Future timestamps (clock skew): show absolute
    if age.num_seconds() < 0 {
        return (timestamp.format("%b %d, %H:%M").to_string(), Recency::Old);
    }

    let seconds = age.num_seconds();
    let minutes = age.num_minutes();
    let hours = age.num_hours();

    if seconds < 60 {
        return ("just now".to_string(), Recency::Now);
    }
    if minutes < 60 {
        return (format!("{minutes} min ago"), Recency::Minutes);
    }
    if hours < 24 {
        return (
            format!("{hours} hour{} ago", if hours == 1 { "" } else { "s" }),
            Recency::Hours,
        );
    }

    // Use calendar day difference for "yesterday" accuracy
    let day_diff = now
        .date_naive()
        .signed_duration_since(timestamp.date_naive())
        .num_days();
    if day_diff == 1 {
        return ("yesterday".to_string(), Recency::Days);
    }
    if day_diff < 7 {
        return (format!("{day_diff} days ago"), Recency::Days);
    }

    (timestamp.format("%b %d, %H:%M").to_string(), Recency::Old)
}

/// The text with the query's matches in `highlight_style` and the rest in
/// `base_style`.
fn highlight(
    query: &HighlightQuery,
    text: &str,
    base_style: Style,
    highlight_style: Style,
) -> Vec<Span<'static>> {
    highlight_ranges(text, query.match_ranges(text), base_style, highlight_style)
}

#[cfg(test)]
fn highlight_text(
    text: &str,
    query: &str,
    base_style: Style,
    highlight_style: Style,
) -> Vec<Span<'static>> {
    highlight_ranges(
        text,
        find_normalized_match_ranges(text, query),
        base_style,
        highlight_style,
    )
}

fn highlight_ranges(
    text: &str,
    ranges: Vec<(usize, usize)>,
    base_style: Style,
    highlight_style: Style,
) -> Vec<Span<'static>> {
    if ranges.is_empty() {
        return vec![Span::styled(text.to_string(), base_style)];
    }

    let merged = merge_match_ranges(ranges)
        .into_iter()
        .filter(|(_, end)| *end <= text.len())
        .collect::<Vec<_>>();

    if merged.is_empty() {
        return vec![Span::styled(text.to_string(), base_style)];
    }

    let mut spans = Vec::new();
    let mut pos = 0;

    for (start, end) in &merged {
        if *start > pos {
            spans.push(Span::styled(text[pos..*start].to_string(), base_style));
        }
        spans.push(Span::styled(
            text[*start..*end].to_string(),
            highlight_style,
        ));
        pos = *end;
    }

    if pos < text.len() {
        spans.push(Span::styled(text[pos..].to_string(), base_style));
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{Conversation, Source};
    use crate::search::query::ParsedQuery;
    use crate::semantic::types::{
        SemanticChunkIdentity, SemanticExplanation, SemanticQuality, SemanticRationaleKind,
        SemanticScoreBreakdown,
    };
    use crate::tui::app::{SemanticProgress, SemanticResultMetadata, TuiSearchOptions};
    use crate::tui::semantic_worker::{SemanticSearchMessage, SemanticSearchResponse};
    use crate::tui::viewer::ToolDisplayMode;
    use chrono::TimeZone;
    use ratatui::Terminal;
    use ratatui::backend::{Backend, TestBackend};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::mpsc;

    #[test]
    fn loading_status_names_the_source_being_loaded_and_the_rows_so_far() {
        let codex = LoadProgress {
            source: Source::Codex,
            done: 120,
            total: 3994,
            unit: LoadUnit::Sessions,
        };
        let claude = LoadProgress {
            source: Source::Claude,
            done: 12,
            total: 80,
            unit: LoadUnit::Projects,
        };

        assert_eq!(
            loading_status(1240, Some(&codex)),
            "Loading Codex 120/3994 sessions · 1240 loaded"
        );
        assert_eq!(
            loading_status(0, Some(&claude)),
            "Loading Claude 12/80 projects · 0 loaded"
        );
        assert_eq!(loading_status(0, None), "Loading... 0");
    }

    #[test]
    fn view_help_overlay_handles_tiny_terminal() {
        for (width, height) in [(20, 8), (10, 3), (2, 2), (1, 1)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    render_help_overlay(
                        frame,
                        true,
                        false,
                        false,
                        false,
                        &KeyBindings::default(),
                        0,
                    )
                })
                .unwrap();
        }
    }

    #[test]
    fn list_help_overlay_handles_tiny_terminal() {
        for (width, height) in [(20, 8), (10, 3), (2, 2), (1, 1)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    render_help_overlay(
                        frame,
                        false,
                        false,
                        false,
                        false,
                        &KeyBindings::default(),
                        0,
                    )
                })
                .unwrap();
        }
    }

    #[test]
    fn gutter_marks_the_focused_call_its_gap_and_the_run_around_it() {
        use crate::tui::app::AppMode;
        use crossterm::event::{KeyCode, KeyModifiers};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"one\ntwo\nthree\nfour\nfive"}},{"type":"tool_use","id":"toolu_2","name":"Read","input":{"file_path":"src/lib.rs"}}]}}"#,
                "\n",
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut app =
            App::new_single_file(path, ToolDisplayMode::Hidden, false, KeyBindings::default());
        app.check_view_resize(80, 17);
        app.handle_key(KeyCode::Char('J'), KeyModifiers::empty(), 17);
        app.handle_key(KeyCode::Right, KeyModifiers::empty(), 17);
        let AppMode::View(state) = app.app_mode() else {
            unreachable!()
        };
        assert_eq!(state.focused_call(), Some(0));

        let backend = TestBackend::new(80, 17);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_view_content(frame, state, frame.area()))
            .unwrap();

        let run = &state.message_ranges[0];
        let [bash, read] = state.call_ranges.as_slice() else {
            panic!("two calls expected, got {}", state.call_ranges.len());
        };
        // The run's rows: its heading, the Bash input, the Read's rows with a
        // blank on either side, the Bash result.
        let bash_result = bash.result.as_ref().unwrap();
        assert!(!bash.contains_line(run.start_line));
        assert!(bash.input.end_line < read.input.start_line);
        assert!(read.input.end_line < bash_result.start_line);
        assert_eq!(bash_result.end_line, run.end_line);

        let accent = rgb(th().accent);
        for line in run.start_line..run.end_line {
            let expected = if line == run.start_line {
                ('▏', rgb(th().text_muted))
            } else if bash.contains_line(line) {
                ('▌', accent)
            } else {
                ('▏', accent)
            };
            let row = row_text(&terminal, line as u16);
            let mark = (
                row.chars().next().unwrap(),
                cell_fg(&terminal, 0, line as u16),
            );
            assert_eq!(mark, expected, "line {line}: {row:?}");
        }
    }

    fn terminal_contents(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn row_text(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    fn cell_fg(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> Color {
        terminal.backend().buffer()[(x, y)].fg
    }

    /// The column where `text` starts on row `y`; the row must be ASCII up to
    /// that point for the byte offset to be the column.
    fn column_of(terminal: &Terminal<TestBackend>, y: u16, text: &str) -> u16 {
        let row = row_text(terminal, y);
        let offset = row
            .find(text)
            .unwrap_or_else(|| panic!("no {text:?} in row {y}: {row:?}"));
        assert!(row[..offset].is_ascii(), "{row:?}");
        offset as u16
    }

    fn assert_cursor_inside(terminal: &mut Terminal<TestBackend>, width: u16) {
        let cursor = terminal.backend_mut().get_cursor_position().unwrap();
        assert_eq!(cursor.y, 0);
        assert!(cursor.x < width, "cursor {cursor:?} outside width {width}");
    }

    fn test_conversation() -> Conversation {
        Conversation {
            source: crate::history::Source::Claude,
            subagents: Vec::new(),
            session_id: "session".to_owned(),
            path: PathBuf::from("/tmp/session.jsonl"),
            index: 0,
            timestamp: Local.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            preview: "lexical preview sentinel".to_string(),
            preview_first: "lexical preview sentinel".to_string(),
            preview_last: "lexical preview sentinel".to_string(),
            full_text: "tool output sentinel summary sentinel cwd sentinel".to_string(),
            agent_search_text: String::new(),
            semantic_route_text: String::new(),
            semantic_turns: vec!["semantic visible text".to_string()],
            semantic_turn_ranges: vec![crate::agent::refs::MessageRange::single(1)],
            search_text_lower: "lexical preview sentinel".to_string(),
            project_name: Some("project sentinel".to_string()),
            project_path: None,
            cwd: Some(PathBuf::from("/cwd/sentinel")),
            message_count: 1,
            assistant_messages: 1,
            parse_errors: Vec::new(),
            summary: Some("summary sentinel".to_string()),
            custom_title: Some("title sentinel".to_string()),
            model: None,
            total_tokens: 0,
            duration_minutes: None,
        }
    }

    /// A month past the test conversation, so a row dates it absolutely as
    /// `Jan 01, 00:00`.
    fn a_month_later() -> DateTime<Local> {
        Local.with_ymd_and_hms(2024, 2, 1, 0, 0, 0).unwrap()
    }

    fn lexical_row(conversation: &Conversation, width: usize) -> ListRow {
        let query = HighlightQuery::parse("");
        let frame_inputs = ListFrameInputs {
            width,
            query: &query,
            search_mode: ListSearchMode::Lexical,
            source_label_column: None,
            now: a_month_later(),
        };
        build_list_row(conversation, &frame_inputs, None, None)
    }

    fn semantic_row(width: usize, query: &str, metadata: &SemanticResultMetadata) -> ListRow {
        let query = HighlightQuery::parse(query);
        let frame_inputs = ListFrameInputs {
            width,
            query: &query,
            search_mode: ListSearchMode::Semantic,
            source_label_column: None,
            now: a_month_later(),
        };
        build_list_row(&test_conversation(), &frame_inputs, Some(metadata), None)
    }

    /// The columns the header fills: the indicator, the left parts, the
    /// padding and the right-aligned metadata. The row's last column stays
    /// blank, so a header fills its width less one.
    fn header_width(row: &ListRow) -> usize {
        let width = |text: &str| UnicodeWidthStr::width(text);
        let part = |text: &Option<String>| text.as_deref().map(width).unwrap_or(0);
        let separated_part =
            |text: &Option<String>| text.as_deref().map(|s| width(s) + 3).unwrap_or(0);
        width(ROW_INDICATOR)
            + width(&row.project)
            + part(&row.custom_title_with_separator)
            + part(&row.summary_with_separator)
            + row.padding
            + width(&row.message_count)
            + separated_part(&row.semantic_metadata)
            + separated_part(&row.duration)
            + 3
            + width(&row.timestamp)
    }

    fn semantic_app() -> App {
        App::new_with_options(
            vec![test_conversation()],
            ToolDisplayMode::Truncated,
            false,
            KeyBindings::default(),
            vec![],
            TuiSearchOptions {
                default_mode: ListSearchMode::Semantic,
            },
        )
    }

    fn semantic_searching_app(query: &str, progress: SemanticProgress) -> App {
        let mut app = semantic_app();
        let (response_tx, response_rx) = mpsc::channel();
        app.set_query_for_test(query);
        app.set_semantic_receiver_for_test(7, response_rx);
        app.set_semantic_prewarm_generation_for_test(7);
        response_tx
            .send(SemanticSearchMessage::Progress {
                generation: 7,
                progress,
            })
            .unwrap();
        app.receive_search_results();
        app
    }

    fn test_semantic_metadata(evidence_preview: &str) -> SemanticResultMetadata {
        test_semantic_metadata_with_scores(
            evidence_preview,
            SemanticScoreBreakdown {
                hybrid: 1.0,
                semantic: 1.0,
                lexical: 0.0,
            },
            SemanticRationaleKind::SemanticOnly,
        )
    }

    fn test_semantic_metadata_with_scores(
        evidence_preview: &str,
        score_breakdown: SemanticScoreBreakdown,
        rationale_kind: SemanticRationaleKind,
    ) -> SemanticResultMetadata {
        SemanticResultMetadata {
            score_breakdown,
            explanation: SemanticExplanation {
                quality: SemanticQuality::Strong,
                quality_label: "strong",
                matched_terms: Vec::new(),
                evidence_preview: evidence_preview.to_string(),
                rationale_kind,
                chunk: SemanticChunkIdentity {
                    conversation_index: 0,
                    source: crate::semantic::types::SemanticChunkSource::VisibleDialogue,
                    session: "test-session".to_string(),
                    chunk_index: 0,
                    message_range: crate::agent::refs::MessageRange::single(1),
                },
            },
        }
    }

    fn app_with_active_filters(filters: &[(&str, &str)]) -> App {
        let mut app = semantic_searching_app("query", SemanticProgress::Complete);
        app.set_active_filters(
            filters
                .iter()
                .map(|(label, value)| crate::history::FilterTerm::new(*label, *value))
                .collect(),
        );
        app
    }

    /// The filters themselves are too wide to sit beside the count, so the bar
    /// carries the cue to the key that lists them.
    #[test]
    fn the_search_bar_points_at_the_key_that_lists_active_filters() {
        let app = app_with_active_filters(&[("since", "2026-08-17 13:45")]);
        let backend = TestBackend::new(90, 4);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_search_bar(frame, &app, frame.area()))
            .unwrap();

        let line = row_text(&terminal, 0);
        assert!(line.contains("· ^L filters"), "{line:?}");
        assert!(!line.contains("2026-08-17"), "{line:?}");
    }

    /// The bar drops the cue rather than cutting it, as it drops the transient
    /// semantic status.
    #[test]
    fn a_narrow_search_bar_drops_the_filter_cue_rather_than_cutting_it() {
        let app = app_with_active_filters(&[("since", "2026-08-17 13:45")]);
        let width = 20;
        let backend = TestBackend::new(width, 4);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_search_bar(frame, &app, frame.area()))
            .unwrap();

        let line = row_text(&terminal, 0);
        assert_eq!(line.chars().count(), width as usize);
        assert!(!line.contains("^L"), "{line:?}");
        assert!(!line.contains("filte"), "{line:?}");
    }

    #[test]
    fn an_unfiltered_search_bar_points_at_no_such_key() {
        let app = semantic_searching_app("query", SemanticProgress::Complete);
        let backend = TestBackend::new(90, 4);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_search_bar(frame, &app, frame.area()))
            .unwrap();

        assert!(!row_text(&terminal, 0).contains("filters"));
    }

    #[test]
    fn the_active_filters_popup_names_every_filter_in_force() {
        let app = app_with_active_filters(&[
            ("since", "2026-08-17 13:45"),
            ("before", "2026-08-24 09:00"),
        ]);
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_active_filters_popup(frame, &app))
            .unwrap();

        let screen = terminal_contents(&terminal);
        assert!(screen.contains("Active search filters"), "{screen}");
        // Label and value sit in separate columns, aligned on the separator,
        // so each row is asserted as the reader sees it.
        assert!(screen.contains("since  │ 2026-08-17 13:45"), "{screen}");
        assert!(screen.contains("before │ 2026-08-24 09:00"), "{screen}");
    }

    #[test]
    fn search_bar_hides_transient_semantic_status_at_narrow_width() {
        let app = semantic_searching_app("你好世界widequery", SemanticProgress::Ranking);
        let width = 24;
        let backend = TestBackend::new(width, 4);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_search_bar(frame, &app, frame.area()))
            .unwrap();

        let line = row_text(&terminal, 0);
        assert_eq!(line.chars().count(), width as usize);
        assert!(!line.contains("sem ranking"), "{line:?}");
        assert!(!line.contains("sem model"), "{line:?}");
        assert!(!line.contains("sem cache"), "{line:?}");
        assert!(line.contains("1/1"), "{line:?}");
        assert_cursor_inside(&mut terminal, width);
    }

    #[test]
    fn lexical_search_bar_omits_semantic_status_at_normal_width() {
        let mut app = App::new(
            vec![test_conversation()],
            ToolDisplayMode::Truncated,
            false,
            KeyBindings::default(),
            vec![],
        );
        app.set_query_for_test("lexical query");
        let width = 80;
        let backend = TestBackend::new(width, 4);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_search_bar(frame, &app, frame.area()))
            .unwrap();

        let line = row_text(&terminal, 0);
        assert_eq!(line.chars().count(), width as usize);
        assert!(line.contains("lexical query"), "{line:?}");
        assert!(line.contains("1/1"), "{line:?}");
        assert!(!line.contains("semantic"), "{line:?}");
        assert!(!line.contains("sem "), "{line:?}");
        assert_cursor_inside(&mut terminal, width);
    }

    fn lexical_app_with_query(query: &str) -> App {
        let mut app = App::new(
            vec![test_conversation()],
            ToolDisplayMode::Truncated,
            false,
            KeyBindings::default(),
            vec![],
        );
        app.set_query_for_test(query);
        app.update_filter_for_test();
        app
    }

    fn draw_search_bar(app: &App) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(80, 4)).unwrap();
        terminal
            .draw(|frame| render_search_bar(frame, app, frame.area()))
            .unwrap();
        terminal
    }

    /// The prompt is `" ❯ "`, so the query starts at column 3; `column_of`
    /// cannot find it past the non-ASCII prompt.
    const QUERY_COLUMN: u16 = 3;

    /// A listed id and a missing one color the query alike; the listed case
    /// stands for both.
    #[test]
    fn a_recognized_session_id_is_gold_in_the_search_bar() {
        let session_id = test_conversation().session_id;
        let app = lexical_app_with_query(&session_id);
        assert!(app.is_session_id_query());

        let terminal = draw_search_bar(&app);

        let line = row_text(&terminal, 0);
        assert!(line.contains(session_id.as_str()), "{line:?}");
        for x in QUERY_COLUMN..QUERY_COLUMN + session_id.len() as u16 {
            assert_eq!(cell_fg(&terminal, x, 0), rgb(th().session_id), "{line:?}");
        }
    }

    #[test]
    fn a_text_query_keeps_the_default_color_in_the_search_bar() {
        let app = lexical_app_with_query("lexical query");
        assert!(!app.is_session_id_query());

        let terminal = draw_search_bar(&app);

        let line = row_text(&terminal, 0);
        assert!(line.contains("lexical query"), "{line:?}");
        assert_eq!(
            cell_fg(&terminal, QUERY_COLUMN, 0),
            Color::Reset,
            "{line:?}"
        );
    }

    #[test]
    fn semantic_search_bar_keeps_query_mode_count_status_and_cursor_at_normal_width() {
        let app = semantic_searching_app(
            "vector query with enough words",
            SemanticProgress::Embedding {
                completed: 21,
                total: 42,
            },
        );
        let width = 80;
        let backend = TestBackend::new(width, 4);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_search_bar(frame, &app, frame.area()))
            .unwrap();

        let line = row_text(&terminal, 0);
        assert_eq!(line.chars().count(), width as usize);
        assert!(line.contains("vector query with enough words"), "{line:?}");
        assert!(line.contains("sem 1/1"), "{line:?}");
        assert!(line.contains("1/1"), "{line:?}");
        assert!(!line.contains("sem embedding"), "{line:?}");
        assert_cursor_inside(&mut terminal, width);
    }

    fn complete_semantic_search(app: &mut App, metadata: SemanticResultMetadata) {
        let (response_tx, response_rx) = mpsc::channel();
        app.set_semantic_receiver_for_test(7, response_rx);
        response_tx
            .send(SemanticSearchMessage::Complete(SemanticSearchResponse {
                generation: 7,
                filtered: vec![0],
                metadata: HashMap::from([(0, metadata)]),
                error: None,
                progress: SemanticProgress::Complete,
                prewarm: false,
            }))
            .unwrap();
        app.receive_search_results();
    }

    fn render_semantic_list_contents(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_list(frame, app, frame.area()))
            .unwrap();

        terminal_contents(&terminal)
    }

    /// Mixed-source rows align on a label column sized from the registry. Pinning
    /// the text keeps a new provider from silently reflowing every row, and
    /// keeps the column from drifting back to a hardcoded width.
    #[test]
    fn mixed_source_rows_align_on_the_source_label() {
        let query = HighlightQuery::parse("");
        let frame_inputs = ListFrameInputs {
            width: 120,
            query: &query,
            search_mode: ListSearchMode::Lexical,
            source_label_column: Some(crate::history::provider::list_label_column_width()),
            now: a_month_later(),
        };

        for (source, project, expected) in [
            (crate::history::Source::Claude, "alpha", "CC   · alpha"),
            (crate::history::Source::Pi, "beta", "Pi   · beta"),
            (crate::history::Source::Kimi, "delta", "KIMI · delta"),
        ] {
            let mut conversation = test_conversation();
            conversation.source = source;
            conversation.project_name = Some(project.to_string());
            conversation.custom_title = None;
            conversation.summary = None;

            let row = build_list_row(&conversation, &frame_inputs, None, None);

            assert_eq!(row.project, expected);
        }
    }

    #[test]
    fn list_truncates_long_project_names_on_narrow_rows() {
        let mut conversation = test_conversation();
        conversation.project_name = Some("claude-history/drop-semantic-feature-gate".to_string());

        let row = lexical_row(&conversation, 70);

        assert_eq!(row.project, "claude-history/drop-semantic…");
    }

    #[test]
    fn list_uses_available_width_for_custom_titles() {
        let mut conversation = test_conversation();
        conversation.project_name = Some("aven".to_string());
        conversation.custom_title = Some(
            "fork lineage alpha beta gamma delta epsilon zeta eta theta iota kappa lambda"
                .to_string(),
        );
        conversation.summary = Some("generated summary remains visible".to_string());

        let row = lexical_row(&conversation, 160);

        assert_eq!(
            row.custom_title_with_separator.as_deref(),
            Some(" · fork lineage alpha beta gamma delta epsilon zeta eta theta iota kappa lambda")
        );
        assert_eq!(
            row.summary_with_separator.as_deref(),
            Some(" · generated summary remains visible")
        );
        assert_eq!(row.message_count, "1 msg");
        assert_eq!(row.timestamp, "Jan 01, 00:00");
    }

    #[test]
    fn list_truncates_custom_titles_to_preserve_metadata() {
        let mut conversation = test_conversation();
        conversation.project_name = Some("aven".to_string());
        conversation.custom_title = Some(
            "fork lineage alpha beta gamma delta epsilon zeta eta theta iota kappa lambda"
                .to_string(),
        );
        conversation.summary = None;
        let width = 72;

        let row = lexical_row(&conversation, width);

        assert_eq!(
            row.custom_title_with_separator.as_deref(),
            Some(" · fork lineage alpha beta gamma delta e…")
        );
        assert_eq!(row.message_count, "1 msg");
        assert_eq!(row.timestamp, "Jan 01, 00:00");
        assert_eq!(header_width(&row), width - 1);
    }

    #[test]
    fn semantic_list_uses_conversation_preview_without_query() {
        let app = semantic_app();
        let backend = TestBackend::new(80, 8);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_list(frame, &app, frame.area()))
            .unwrap();

        let contents = terminal_contents(&terminal);
        assert!(
            contents.contains("lexical preview sentinel"),
            "{contents:?}"
        );
        assert!(!contents.contains("semantic visible text"), "{contents:?}");
    }

    #[test]
    fn semantic_list_uses_conversation_preview_while_query_has_no_metadata() {
        let mut app = semantic_app();
        app.set_query_for_test("sentinel");
        let backend = TestBackend::new(80, 8);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_list(frame, &app, frame.area()))
            .unwrap();

        let contents = terminal_contents(&terminal);
        assert!(
            contents.contains("lexical preview sentinel"),
            "{contents:?}"
        );
    }

    fn scored_semantic_metadata() -> SemanticResultMetadata {
        test_semantic_metadata_with_scores(
            "semantic evidence only",
            SemanticScoreBreakdown {
                hybrid: 1.23,
                semantic: 1.0,
                lexical: 0.23,
            },
            SemanticRationaleKind::LexicalBoosted,
        )
    }

    /// The compact metadata is the hybrid score alone; the quality label stays
    /// off the row.
    #[test]
    fn semantic_list_shows_compact_score_metadata_on_wide_rows() {
        let row = semantic_row(70, "sentinel", &scored_semantic_metadata());

        assert_eq!(row.semantic_metadata.as_deref(), Some("1.23"));
    }

    #[test]
    fn semantic_list_hides_score_metadata_on_narrow_rows() {
        let row = semantic_row(69, "sentinel", &scored_semantic_metadata());

        assert_eq!(row.semantic_metadata, None);
    }

    #[test]
    fn semantic_status_bar_keeps_hotkeys_when_result_metadata_exists() {
        let mut app = App::new_with_options(
            vec![test_conversation(), test_conversation()],
            ToolDisplayMode::Truncated,
            false,
            KeyBindings::default(),
            vec![],
            TuiSearchOptions {
                default_mode: ListSearchMode::Semantic,
            },
        );
        app.set_query_for_test("sentinel");
        let (response_tx, response_rx) = mpsc::channel();
        app.set_semantic_receiver_for_test(7, response_rx);
        response_tx
            .send(SemanticSearchMessage::Complete(SemanticSearchResponse {
                generation: 7,
                filtered: vec![1, 0],
                metadata: HashMap::from([(
                    1,
                    test_semantic_metadata_with_scores(
                        "semantic evidence only",
                        SemanticScoreBreakdown {
                            hybrid: 1.23,
                            semantic: 0.98,
                            lexical: 0.25,
                        },
                        SemanticRationaleKind::LexicalBoosted,
                    ),
                )]),
                error: None,
                progress: SemanticProgress::Complete,
                prewarm: false,
            }))
            .unwrap();
        app.receive_search_results();
        let backend = TestBackend::new(80, 2);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_list_status_bar(frame, &app, frame.area()))
            .unwrap();

        let line = row_text(&terminal, 0);
        assert!(line.contains("Enter"), "{line:?}");
        assert!(line.contains("semantic·sem"), "{line:?}");
        assert!(!line.contains("sem 0.98"), "{line:?}");
        assert!(!line.contains("lex 0.25"), "{line:?}");
        assert!(!line.contains("lex boost"), "{line:?}");
    }

    #[test]
    fn semantic_status_bar_shows_embedding_progress_before_results() {
        let app = semantic_searching_app(
            "sentinel",
            SemanticProgress::Embedding {
                completed: 21,
                total: 42,
            },
        );
        let backend = TestBackend::new(80, 2);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_list_status_bar(frame, &app, frame.area()))
            .unwrap();

        let line = row_text(&terminal, 0);
        assert!(line.contains("sem embedding 50%"), "{line:?}");
        assert!(line.contains("21/42 chunks"), "{line:?}");
    }

    #[test]
    fn semantic_debug_popup_renders_score_details() {
        let mut app = semantic_app();
        app.set_query_for_test("sentinel");
        complete_semantic_search(
            &mut app,
            test_semantic_metadata_with_scores(
                "semantic evidence only",
                SemanticScoreBreakdown {
                    hybrid: 1.23,
                    semantic: 0.9,
                    lexical: 0.1,
                },
                SemanticRationaleKind::LexicalBoosted,
            ),
        );
        app.set_dialog_mode_for_test(DialogMode::SemanticDebug);
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_list_mode(frame, &app))
            .unwrap();

        let contents = terminal_contents(&terminal);
        assert!(contents.contains("Semantic result"), "{contents:?}");
        assert!(contents.contains("1.23"), "{contents:?}");
        assert!(contents.contains("0.90"), "{contents:?}");
        assert!(contents.contains("lex boost"), "{contents:?}");
        assert!(contents.contains("semantic evidence only"), "{contents:?}");
    }

    #[test]
    fn highlight_query_uses_unquoted_terms_and_literals_for_context() {
        assert_eq!(
            HighlightQuery::parse("alpha \"DEPLOYMENT_TOKEN\" beta").context_text(),
            "alpha beta deployment token"
        );
    }

    #[test]
    fn highlight_query_preserves_unquoted_identifier_punctuation() {
        let highlight_style = Style::default().fg(Color::Yellow);
        let query = HighlightQuery::parse("deployment_token");
        let spans = highlight(
            &query,
            "prefix deployment token suffix DEPLOYMENT_TOKEN",
            Style::default(),
            highlight_style,
        );
        let highlighted: Vec<_> = span_info(&spans, highlight_style)
            .into_iter()
            .filter(|(_, highlighted)| *highlighted)
            .collect();

        assert_eq!(highlighted, vec![("DEPLOYMENT_TOKEN", true)]);
    }

    #[test]
    fn quoted_list_highlighting_matches_literal_text() {
        let highlight_style = Style::default().fg(Color::Yellow);
        let query = HighlightQuery::parse("\"DEPLOYMENT_TOKEN\"");
        let spans = highlight(
            &query,
            "prefix DEPLOYMENT_TOKEN suffix",
            Style::default(),
            highlight_style,
        );
        let highlighted: Vec<_> = span_info(&spans, highlight_style)
            .into_iter()
            .filter(|(_, highlighted)| *highlighted)
            .collect();

        assert_eq!(highlighted, vec![("DEPLOYMENT_TOKEN", true)]);
    }

    #[test]
    fn quoted_list_highlighting_matches_multiword_literal_phrase() {
        let highlight_style = Style::default().fg(Color::Yellow);
        let query = HighlightQuery::parse("alpha \"beta gamma\"");
        let spans = highlight(
            &query,
            "alpha prefix beta gamma suffix beta-only",
            Style::default(),
            highlight_style,
        );
        let highlighted: Vec<_> = span_info(&spans, highlight_style)
            .into_iter()
            .filter(|(_, highlighted)| *highlighted)
            .collect();

        assert_eq!(highlighted, vec![("alpha", true), ("beta gamma", true)]);
    }

    #[test]
    fn quoted_list_highlighting_respects_smart_case() {
        let highlight_style = Style::default().fg(Color::Yellow);
        let query = HighlightQuery::parse("\"Beta Gamma\"");
        let spans = highlight(
            &query,
            "beta gamma then Beta Gamma",
            Style::default(),
            highlight_style,
        );
        let highlighted: Vec<_> = span_info(&spans, highlight_style)
            .into_iter()
            .filter(|(_, highlighted)| *highlighted)
            .collect();

        assert_eq!(highlighted, vec![("Beta Gamma", true)]);
    }

    #[test]
    fn semantic_evidence_preview_highlights_query_terms() {
        let metadata = test_semantic_metadata(
            "prefix text before the important semantic needle appears near the end",
        );
        let spans = highlight_text(
            &build_match_segments(&metadata.explanation.evidence_preview, "needle", 40),
            "needle",
            Style::default(),
            Style::default().fg(Color::Yellow),
        );
        let highlighted: Vec<_> = span_info(&spans, Style::default().fg(Color::Yellow))
            .into_iter()
            .filter(|(_, highlighted)| *highlighted)
            .collect();
        assert_eq!(highlighted.len(), 1);
        assert_eq!(highlighted[0].0, "needle");
    }

    /// A preview of wide characters stays inside the preview budget, the width
    /// less the indicator and a trailing column, and the header keeps its width.
    #[test]
    fn semantic_list_truncates_cleanly_at_narrow_width() {
        let evidence_preview = format!("{} needle{}", "宽字符前缀".repeat(8), "x".repeat(120));
        let metadata = test_semantic_metadata_with_scores(
            &evidence_preview,
            SemanticScoreBreakdown {
                hybrid: 123.45,
                semantic: 67.89,
                lexical: 55.56,
            },
            SemanticRationaleKind::WeakMatch,
        );
        let width = 28;

        let row = semantic_row(width, "needle", &metadata);

        assert!(
            UnicodeWidthStr::width(row.preview.as_str()) <= width - 4,
            "{:?}",
            row.preview
        );
        assert_eq!(header_width(&row), width - 1);
    }

    #[test]
    fn lexical_unquoted_render_shows_cached_hidden_full_text_context() {
        let mut conversation = test_conversation();
        conversation.preview = "visible lexical preview".to_string();
        conversation.full_text =
            format!("visible lexical preview {} hiddenneedle", "x ".repeat(200));
        let evidence = crate::search::build_lexical_evidence(
            &conversation,
            &ParsedQuery::parse("hiddenneedle"),
        )
        .unwrap();
        let mut app = App::new(
            vec![conversation],
            ToolDisplayMode::Truncated,
            false,
            KeyBindings::default(),
            vec![],
        );
        app.set_query_for_test("hiddenneedle");
        app.set_lexical_evidence_for_test(0, evidence);
        let backend = TestBackend::new(80, 8);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_list(frame, &app, frame.area()))
            .unwrap();

        let contents = terminal_contents(&terminal);
        assert!(contents.contains("hiddenneedle"), "{contents:?}");
    }

    #[test]
    fn lexical_quoted_render_shows_hidden_literal_context() {
        let mut conversation = test_conversation();
        conversation.preview = "visible lexical preview".to_string();
        conversation.full_text =
            format!("visible lexical preview {} hidden_literal", "x ".repeat(80));
        let mut app = App::new(
            vec![conversation],
            ToolDisplayMode::Truncated,
            false,
            KeyBindings::default(),
            vec![],
        );
        app.set_query_for_test("\"hidden_literal\"");
        let backend = TestBackend::new(80, 8);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_list(frame, &app, frame.area()))
            .unwrap();

        let contents = terminal_contents(&terminal);
        assert!(contents.contains("hidden_literal"), "{contents:?}");
    }

    #[test]
    fn lexical_mixed_query_cached_context_uses_unquoted_and_literals() {
        let mut conversation = test_conversation();
        conversation.preview = "visible preview".to_string();
        conversation.full_text = format!("hidden_unquoted {} exact_literal", "x ".repeat(120));
        let evidence = crate::search::build_lexical_evidence(
            &conversation,
            &ParsedQuery::parse("hidden_unquoted \"exact_literal\""),
        )
        .unwrap();
        let ctx = build_context_segments_from_ranges(
            &conversation.full_text,
            &evidence.context_ranges,
            120,
        )
        .unwrap();

        assert!(ctx.contains("exact_literal"), "{ctx:?}");
        assert!(ctx.contains("hidden_unquoted"), "{ctx:?}");
    }

    #[test]
    fn semantic_list_uses_semantic_evidence_preview_without_full_text_context() {
        let mut app = semantic_app();
        app.set_query_for_test("sentinel");
        complete_semantic_search(&mut app, test_semantic_metadata("semantic evidence only"));
        let contents = render_semantic_list_contents(&mut app, 80, 8);
        assert!(contents.contains("semantic evidence only"), "{contents:?}");
        assert!(
            !contents.contains("lexical preview sentinel"),
            "{contents:?}"
        );
        assert!(!contents.contains("tool output sentinel"), "{contents:?}");
    }

    #[test]
    fn semantic_list_shows_literal_context_when_evidence_lacks_literal() {
        let mut conversation = test_conversation();
        conversation.full_text =
            "tool output sentinel includes audio_generation literal".to_string();
        let mut app = App::new_with_options(
            vec![conversation],
            ToolDisplayMode::Truncated,
            false,
            KeyBindings::default(),
            vec![],
            TuiSearchOptions {
                default_mode: ListSearchMode::Semantic,
            },
        );
        app.set_query_for_test("semantic \"audio_generation\"");
        let (response_tx, response_rx) = mpsc::channel();
        app.set_semantic_receiver_for_test(7, response_rx);
        response_tx
            .send(SemanticSearchMessage::Complete(SemanticSearchResponse {
                generation: 7,
                filtered: vec![0],
                metadata: HashMap::new(),
                error: None,
                progress: SemanticProgress::Complete,
                prewarm: false,
            }))
            .unwrap();
        app.receive_search_results();
        let contents = render_semantic_list_contents(&mut app, 80, 8);
        assert!(
            contents.contains("lexical preview sentinel"),
            "{contents:?}"
        );
        assert!(contents.contains("audio_generation"), "{contents:?}");
    }

    #[test]
    fn semantic_literal_preview_uses_literal_ranges() {
        let mut app = semantic_app();
        app.set_query_for_test("\"audio_generation\"");
        complete_semantic_search(
            &mut app,
            test_semantic_metadata(
                "normalized audio generation appears early before exact audio_generation literal",
            ),
        );
        let backend = TestBackend::new(54, 8);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_list(frame, &app, frame.area()))
            .unwrap();

        let contents = terminal_contents(&terminal);
        assert!(contents.contains("audio_generation"), "{contents:?}");
        assert!(!contents.contains("audio generation"), "{contents:?}");
    }

    #[test]
    fn semantic_literal_preview_merges_overlapping_ranges() {
        let mut app = semantic_app();
        app.set_query_for_test("audio \"audio_generation\"");
        complete_semantic_search(
            &mut app,
            test_semantic_metadata("prefix audio_generation literal near the front"),
        );
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_list(frame, &app, frame.area()))
            .unwrap();

        let contents = terminal_contents(&terminal);
        assert!(contents.contains("audio_generation"), "{contents:?}");
    }

    #[test]
    fn semantic_literal_context_requires_all_literals_visible() {
        let mut conversation = test_conversation();
        conversation.full_text = "alpha_exact near preview. beta_exact hidden deeper.".to_string();
        let mut app = App::new_with_options(
            vec![conversation],
            ToolDisplayMode::Truncated,
            false,
            KeyBindings::default(),
            vec![],
            TuiSearchOptions {
                default_mode: ListSearchMode::Semantic,
            },
        );
        app.set_query_for_test("semantic \"alpha_exact\" \"beta_exact\"");
        complete_semantic_search(
            &mut app,
            test_semantic_metadata("semantic alpha_exact only"),
        );
        let backend = TestBackend::new(80, 8);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_list(frame, &app, frame.area()))
            .unwrap();

        let contents = terminal_contents(&terminal);
        assert!(contents.contains("alpha_exact"), "{contents:?}");
        assert!(contents.contains("beta_exact"), "{contents:?}");
    }

    #[test]
    fn semantic_shortcut_appears_only_when_available() {
        let backend = TestBackend::new(70, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_help_overlay(
                    frame,
                    false,
                    false,
                    false,
                    false,
                    &KeyBindings::default(),
                    0,
                )
            })
            .unwrap();
        let unavailable = terminal_contents(&terminal);

        let backend = TestBackend::new(70, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_help_overlay(frame, false, false, true, false, &KeyBindings::default(), 0)
            })
            .unwrap();
        let available = terminal_contents(&terminal);

        assert!(
            !unavailable.contains("Toggle semantic search"),
            "{unavailable:?}"
        );
        assert!(
            available.contains("Toggle semantic search"),
            "{available:?}"
        );
    }

    #[test]
    fn help_overlay_lists_the_mouse_in_both_modes() {
        for (is_view_mode, action) in [
            (true, "Expand / collapse tool row"),
            (false, "Open conversation"),
        ] {
            let backend = TestBackend::new(80, 40);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    render_help_overlay(
                        frame,
                        is_view_mode,
                        false,
                        false,
                        false,
                        &KeyBindings::default(),
                        0,
                    )
                })
                .unwrap();

            let contents = terminal_contents(&terminal);
            assert!(contents.contains("Wheel"), "{contents:?}");
            assert!(contents.contains(action), "{contents:?}");
            assert!(contents.contains("Ctrl+C"), "{contents:?}");
        }
    }

    #[test]
    fn help_overlay_indicates_hidden_rows() {
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_help_overlay(frame, true, false, false, false, &KeyBindings::default(), 0)
            })
            .unwrap();

        let contents = terminal_contents(&terminal);
        assert!(contents.contains("↓ more"), "{contents:?}");
        assert!(contents.contains("1-"), "{contents:?}");
    }

    #[test]
    fn help_overlay_scrolls_to_later_rows() {
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_help_overlay(
                    frame,
                    true,
                    false,
                    false,
                    false,
                    &KeyBindings::default(),
                    10,
                )
            })
            .unwrap();

        let contents = terminal_contents(&terminal);
        assert!(
            contents.contains("↑↓ more") || contents.contains("↑ more"),
            "{contents:?}"
        );
        assert!(contents.contains("11-"), "{contents:?}");
    }

    #[test]
    fn export_menus_handle_tiny_terminal() {
        for is_yank in [false, true] {
            for (width, height) in [(20, 8), (10, 3), (2, 2), (1, 1)] {
                let backend = TestBackend::new(width, height);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal
                    .draw(|frame| render_export_menu(frame, 0, is_yank))
                    .unwrap();
            }
        }
    }

    #[test]
    fn centered_modal_area_preserves_fitting_size() {
        let area = centered_modal_area(Rect::new(0, 0, 80, 24), 35, 8);
        assert_eq!(area, Rect::new(22, 8, 35, 8));
    }

    #[test]
    fn centered_modal_area_clamps_to_frame() {
        assert_eq!(
            centered_modal_area(Rect::new(0, 0, 20, 24), 35, 8),
            Rect::new(0, 8, 20, 8)
        );
        assert_eq!(
            centered_modal_area(Rect::new(0, 0, 80, 3), 35, 8),
            Rect::new(22, 0, 35, 3)
        );
        assert_eq!(
            centered_modal_area(Rect::new(0, 0, 10, 3), 35, 8),
            Rect::new(0, 0, 10, 3)
        );
    }

    #[test]
    fn test_format_model_name_opus_45() {
        assert_eq!(format_model_name("claude-opus-4-5-20251101"), "opus-4.5");
    }

    #[test]
    fn test_format_model_name_sonnet_4() {
        assert_eq!(format_model_name("claude-sonnet-4-20250514"), "sonnet-4");
    }

    #[test]
    fn test_format_model_name_sonnet_35() {
        assert_eq!(
            format_model_name("claude-3-5-sonnet-20241022"),
            "sonnet-3.5"
        );
    }

    #[test]
    fn test_format_model_name_haiku_35() {
        assert_eq!(format_model_name("claude-3-5-haiku-20241022"), "haiku-3.5");
    }

    #[test]
    fn test_format_model_name_opus_3() {
        assert_eq!(format_model_name("claude-3-opus-20240229"), "opus-3");
    }

    #[test]
    fn test_format_model_name_unknown() {
        assert_eq!(format_model_name("custom-model"), "custom-model");
    }

    #[test]
    fn test_format_model_name_truncates_long() {
        let long_name = "very-long-unknown-model-name-that-exceeds-limit";
        let formatted = format_model_name(long_name);
        // 19 chars + ellipsis (3 bytes in UTF-8)
        assert!(formatted.chars().count() <= 20);
        assert!(formatted.ends_with('…'));
    }

    #[test]
    fn test_format_tokens_small() {
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
    }

    #[test]
    fn test_format_tokens_thousands() {
        assert_eq!(format_tokens(1000), "1k");
        assert_eq!(format_tokens(417000), "417k");
        assert_eq!(format_tokens(999999), "999k");
    }

    #[test]
    fn test_format_tokens_millions() {
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(1_500_000), "1.5M");
        assert_eq!(format_tokens(12_345_678), "12.3M");
    }

    #[test]
    fn test_format_tokens_long() {
        assert_eq!(format_tokens_long(500), "500 tokens");
        assert_eq!(format_tokens_long(1000), "1k tokens");
        assert_eq!(format_tokens_long(926000), "926k tokens");
        assert_eq!(format_tokens_long(1_500_000), "1.5M tokens");
    }

    // --- highlight_text / find_normalized_match_ranges tests ---

    /// Helper: extract (text, is_highlighted) from spans
    fn span_info<'a>(spans: &'a [Span<'a>], highlight_style: Style) -> Vec<(&'a str, bool)> {
        spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style == highlight_style))
            .collect()
    }

    #[test]
    fn highlight_word_boundary_prefix() {
        let base = Style::default();
        let hl = Style::default().fg(Color::Yellow);
        // "red" matches at start of "redaction" (prefix), but not mid-word
        let spans = highlight_text("Extend log redaction to cover", "red team", base, hl);
        let info = span_info(&spans, hl);
        let highlighted: Vec<_> = info.iter().filter(|(_, h)| *h).collect();
        assert_eq!(highlighted.len(), 1);
        assert_eq!(highlighted[0].0, "red");
    }

    #[test]
    fn highlight_phrase_exact_match() {
        let base = Style::default();
        let hl = Style::default().fg(Color::Yellow);
        let spans = highlight_text(
            "You are being tested as a security red team exercise.",
            "red team",
            base,
            hl,
        );
        let info = span_info(&spans, hl);
        let highlighted: Vec<_> = info.iter().filter(|(_, h)| *h).collect();
        // Adjacent words separated by space merge into one highlight span
        assert_eq!(highlighted.len(), 1);
        assert_eq!(highlighted[0].0, "red team");
    }

    #[test]
    fn highlight_multiple_matches() {
        let base = Style::default();
        let hl = Style::default().fg(Color::Yellow);
        let spans = highlight_text("foo bar foo bar foo", "foo", base, hl);
        let highlighted: Vec<_> = span_info(&spans, hl)
            .into_iter()
            .filter(|(_, h)| *h)
            .collect();
        assert_eq!(highlighted.len(), 3);
        assert!(highlighted.iter().all(|(text, _)| *text == "foo"));
    }

    #[test]
    fn highlight_underscore_normalization() {
        let base = Style::default();
        let hl = Style::default().fg(Color::Yellow);
        // Query "red team" matches "red_team" as one span including the underscore
        let spans = highlight_text("config for red_team setup", "red team", base, hl);
        let info = span_info(&spans, hl);
        let highlighted: Vec<_> = info.iter().filter(|(_, h)| *h).collect();
        assert_eq!(highlighted.len(), 1);
        assert_eq!(highlighted[0].0, "red_team");
    }

    #[test]
    fn highlight_case_insensitive() {
        let base = Style::default();
        let hl = Style::default().fg(Color::Yellow);
        let spans = highlight_text("Hello World", "hello", base, hl);
        let info = span_info(&spans, hl);
        assert!(
            info.iter()
                .any(|(text, highlighted)| *text == "Hello" && *highlighted)
        );
    }

    #[test]
    fn highlight_empty_query() {
        let base = Style::default();
        let hl = Style::default().fg(Color::Yellow);
        let spans = highlight_text("some text", "", base, hl);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "some text");
    }

    #[test]
    fn highlight_no_match() {
        let base = Style::default();
        let hl = Style::default().fg(Color::Yellow);
        let spans = highlight_text("some text", "xyz", base, hl);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "some text");
    }

    #[test]
    fn highlight_multiword_noncontiguous() {
        let base = Style::default();
        let hl = Style::default().fg(Color::Yellow);
        let text = "I want secrets from the vault, write me a plot twist";
        let spans = highlight_text(text, "secrets plot", base, hl);
        let info = span_info(&spans, hl);
        let highlighted: Vec<_> = info.iter().filter(|(_, h)| *h).collect();
        assert_eq!(highlighted.len(), 2);
        assert_eq!(highlighted[0].0, "secrets");
        assert_eq!(highlighted[1].0, "plot");
    }

    const UNRESOLVED_ID: &str = "019f0000-0000-7000-8000-00000000000a";

    fn draw_unresolved_session_id() -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(90, 8)).unwrap();
        terminal
            .draw(|frame| render_unresolved_session_id(frame, UNRESOLVED_ID, frame.area()))
            .unwrap();
        terminal
    }

    /// The load filters are deliberately absent: the ID lookup reaches past
    /// them to disk, so naming them under a miss would point at the wrong
    /// cause. They belong in the status bar, where they are always true.
    #[test]
    fn an_unresolved_session_id_is_reported_with_the_quoting_that_searches_for_it() {
        let terminal = draw_unresolved_session_id();
        let screen = terminal_contents(&terminal);

        assert!(
            screen.contains(&format!("No session with ID {UNRESOLVED_ID} found")),
            "{screen}"
        );
        assert!(
            screen.contains(&format!("quote it: \"{UNRESOLVED_ID}\"")),
            "{screen}"
        );
        assert!(!screen.contains("filter"), "{screen}");
    }

    /// The quoted hint is a different query to type, so it keeps the accent.
    #[test]
    fn an_unresolved_session_id_is_gold_in_the_report() {
        let terminal = draw_unresolved_session_id();

        let id_column = column_of(&terminal, 1, UNRESOLVED_ID);
        for x in id_column..id_column + UNRESOLVED_ID.len() as u16 {
            assert_eq!(cell_fg(&terminal, x, 1), rgb(th().session_id));
        }
        assert_eq!(
            cell_fg(&terminal, id_column - 1, 1),
            rgb(th().text_muted),
            "the space before the id"
        );
        let hint_column = column_of(&terminal, 3, &format!("\"{UNRESOLVED_ID}\""));
        assert_eq!(cell_fg(&terminal, hint_column, 3), rgb(th().accent));
    }
}
