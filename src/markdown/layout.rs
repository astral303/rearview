//! Shared markdown layout engine.
//!
//! Produces a backend-neutral LayoutDoc that can be rendered to TUI spans
//! or ANSI strings via thin sinks.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Semantic text attributes (backend-neutral).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Attrs {
    pub bold: bool,
    pub italic: bool,
    pub dimmed: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub code: bool,
    pub quote: bool,
    pub link: bool,
    pub heading: bool,
    pub code_block_lang: Option<String>,
    /// Explicit RGB foreground (used for syntax-highlighted code tokens).
    pub fg: Option<(u8, u8, u8)>,
    /// Marks a run that is the URL suffix after a link's text.
    /// Sinks that don't want to render URLs (e.g. TUI) can skip these.
    pub link_url: bool,
    /// Marks the `# ` prefix emitted before heading text.
    /// Sinks that don't want to show the literal marker (e.g. TUI) can skip it.
    pub heading_marker: bool,
    /// Marks the opening/closing ``` fence of a code block.
    /// Sinks that don't want to show fence markers (e.g. TUI) can skip them.
    pub code_fence: bool,
}

/// A run of text with uniform attributes.
#[derive(Clone, Debug)]
pub struct Run {
    pub text: String,
    pub attrs: Attrs,
}

/// A line of rendered output.
#[derive(Clone, Debug)]
pub struct Line {
    pub runs: Vec<Run>,
    pub wrap: LineWrap,
}

impl Line {
    fn new(runs: Vec<Run>) -> Self {
        Self {
            runs,
            wrap: LineWrap::default(),
        }
    }
}

/// A line's place in the source line it was wrapped from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineWrap {
    /// True when the line is a later row of a wrapped source line: the line
    /// above, then this line without its prefix, give the source text.
    pub continues_above: bool,
    /// Bytes of the continuation prefix opening the line: a list's indent,
    /// a quote's `> `.
    pub prefix_len: usize,
}

/// Complete rendered document.
#[derive(Clone, Debug)]
pub struct LayoutDoc {
    pub lines: Vec<Line>,
    /// The top-level blocks, in order; empty unless the render recorded
    /// them ([`LayoutEngine::render_with_blocks`]).
    pub blocks: Vec<LayoutBlock>,
}

/// A top-level block of the input: what it is, where its source is, and
/// the lines it laid out.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutBlock {
    pub kind: BlockKind,
    /// Bytes of the input.
    pub source: Range<usize>,
    /// Indices into [`LayoutDoc::lines`], without the blank line that
    /// separates the block from the one above.
    pub lines: Range<usize>,
    /// The items of a list, in order; empty for any other block. Each item
    /// holds its nested content.
    pub items: Vec<LayoutItem>,
}

/// One item of a top-level list.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutItem {
    /// Bytes of the input, from its bullet or number.
    pub source: Range<usize>,
    /// Indices into [`LayoutDoc::lines`].
    pub lines: Range<usize>,
}

/// The kinds of top-level block a render records. A thematic break and an
/// HTML block are not recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Paragraph,
    Heading,
    /// A fenced or indented code block; `code` is its text as the parser
    /// read it, without the fences or the indent.
    CodeBlock {
        code: String,
    },
    Quote,
    List,
    Table,
}

impl BlockKind {
    /// The kind a top-level block opened by `tag` records; `None` for a
    /// block that isn't recorded.
    fn opened_by(tag: &Tag) -> Option<Self> {
        match tag {
            Tag::Paragraph => Some(Self::Paragraph),
            Tag::Heading { .. } => Some(Self::Heading),
            Tag::CodeBlock(_) => Some(Self::CodeBlock {
                code: String::new(),
            }),
            Tag::BlockQuote(_) => Some(Self::Quote),
            Tag::List(_) => Some(Self::List),
            Tag::Table(_) => Some(Self::Table),
            _ => None,
        }
    }
}

/// The top-level blocks of a render, recorded as the parser's events open
/// and close them.
#[derive(Default)]
struct BlockRecorder {
    /// The tags open around the current event.
    depth: usize,
    /// The top-level block the current event is inside.
    open: Option<LayoutBlock>,
    blocks: Vec<LayoutBlock>,
}

impl BlockRecorder {
    /// Record `event`, whose source is `source`, before the engine lays it
    /// out. `first_line` is the index of the first line it can lay out,
    /// after any line still pending, such as an HTML block's, because a
    /// recorded block or item flushes that line as it starts.
    fn record_event(&mut self, event: &Event, source: Range<usize>, first_line: usize) {
        match event {
            Event::Start(tag) => {
                let first_lines = first_line..first_line;
                match self.depth {
                    0 => {
                        self.open = BlockKind::opened_by(tag).map(|kind| LayoutBlock {
                            kind,
                            source,
                            lines: first_lines,
                            items: Vec::new(),
                        });
                    }
                    1 if matches!(tag, Tag::Item) => {
                        if let Some(list) = &mut self.open {
                            list.items.push(LayoutItem {
                                source,
                                lines: first_lines,
                            });
                        }
                    }
                    _ => {}
                }
                self.depth += 1;
            }
            Event::Text(text) if self.depth == 1 => {
                if let Some(LayoutBlock {
                    kind: BlockKind::CodeBlock { code },
                    ..
                }) = &mut self.open
                {
                    code.push_str(text);
                }
            }
            _ => {}
        }
    }

    /// Record the end of `tag` once the engine laid it out into `lines`.
    fn record_end(&mut self, tag: TagEnd, lines: &[Line]) {
        self.depth -= 1;
        match self.depth {
            0 => {
                if let Some(mut block) = self.open.take() {
                    block.lines = laid_out_since(block.lines.start, lines);
                    self.blocks.push(block);
                }
            }
            1 if matches!(tag, TagEnd::Item) => {
                if let Some(item) = self.open.as_mut().and_then(|list| list.items.last_mut()) {
                    item.lines = laid_out_since(item.lines.start, lines);
                }
            }
            _ => {}
        }
    }
}

/// The lines from `first` to the end of `lines`, without the blank lines
/// that open them.
fn laid_out_since(first: usize, lines: &[Line]) -> Range<usize> {
    let start = (first..lines.len())
        .find(|&index| !lines[index].runs.is_empty())
        .unwrap_or(lines.len());
    start..lines.len()
}

/// Tracks list state and computes precise continuation prefixes.
#[derive(Clone, Debug)]
struct ListContext {
    index: Option<u64>,
    depth: usize,
}

/// Prefix emitted on every continuation line for enclosing block structures
/// (list items, block quotes). Each stack entry contributes a styled run.
#[derive(Clone, Debug)]
struct Prefix {
    cont_text: String,
    cont_attrs: Attrs,
}

/// Table state accumulator.
#[derive(Clone, Debug)]
struct TableState {
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
}

impl TableState {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            current_row: Vec::new(),
            current_cell: String::new(),
        }
    }
}

/// Shared markdown layout engine.
pub struct LayoutEngine {
    max_width: usize,
    lines: Vec<Line>,
    current_line: Vec<Run>,
    current_width: usize,
    current_wrap: LineWrap,
    attrs_stack: Vec<Attrs>,
    list_stack: Vec<ListContext>,
    prefix_stack: Vec<Prefix>,
    in_code_block: bool,
    code_block_content: String,
    code_block_lang: String,
    in_list_item_start: bool,
    in_block_quote: bool,
    heading_level: Option<u8>,
    link_url: Option<String>,
    table_state: Option<TableState>,
    /// `None` unless the render records the top-level blocks.
    block_recorder: Option<BlockRecorder>,
}

impl LayoutEngine {
    pub fn new(max_width: usize) -> Self {
        Self {
            max_width,
            lines: Vec::new(),
            current_line: Vec::new(),
            current_width: 0,
            current_wrap: LineWrap::default(),
            attrs_stack: Vec::new(),
            list_stack: Vec::new(),
            prefix_stack: Vec::new(),
            in_code_block: false,
            code_block_content: String::new(),
            code_block_lang: String::new(),
            in_list_item_start: false,
            in_block_quote: false,
            heading_level: None,
            link_url: None,
            table_state: None,
            block_recorder: None,
        }
    }

    pub fn render(input: &str, max_width: usize) -> LayoutDoc {
        let mut engine = Self::new(max_width);
        for event in Parser::new_ext(input, Self::parser_options()) {
            engine.handle_event(event);
        }
        engine.finish()
    }

    /// [`Self::render`], with [`LayoutDoc::blocks`] recorded. The lines are
    /// the same.
    pub fn render_with_blocks(input: &str, max_width: usize) -> LayoutDoc {
        let mut engine = Self::new(max_width);
        engine.block_recorder = Some(BlockRecorder::default());
        for (event, source) in Parser::new_ext(input, Self::parser_options()).into_offset_iter() {
            engine.record_event(&event, source);
            let end = match &event {
                Event::End(tag) => Some(*tag),
                _ => None,
            };
            engine.handle_event(event);
            if let Some(tag) = end {
                engine.record_end(tag);
            }
        }
        engine.finish()
    }

    fn parser_options() -> Options {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TABLES);
        options
    }

    /// Record `event` before it is laid out.
    fn record_event(&mut self, event: &Event, source: Range<usize>) {
        let first_line = self.lines.len() + usize::from(!self.current_line.is_empty());
        if let Some(recorder) = &mut self.block_recorder {
            recorder.record_event(event, source, first_line);
        }
    }

    /// Record the end of `tag` once it is laid out.
    fn record_end(&mut self, tag: TagEnd) {
        if let Some(recorder) = &mut self.block_recorder {
            recorder.record_end(tag, &self.lines);
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.inline_code(&code),
            Event::SoftBreak => self.soft_break(),
            Event::HardBreak => self.hard_break(),
            Event::Rule => self.rule(),
            Event::Html(html) | Event::InlineHtml(html) => self.text(&html),
            _ => {}
        }
    }

    fn current_attrs(&self) -> Attrs {
        let mut attrs = Attrs::default();
        for a in &self.attrs_stack {
            if a.bold {
                attrs.bold = true;
            }
            if a.italic {
                attrs.italic = true;
            }
            if a.dimmed {
                attrs.dimmed = true;
            }
            if a.underline {
                attrs.underline = true;
            }
            if a.strikethrough {
                attrs.strikethrough = true;
            }
            if a.code {
                attrs.code = true;
            }
            if a.quote {
                attrs.quote = true;
            }
            if a.link {
                attrs.link = true;
            }
            if a.heading {
                attrs.heading = true;
            }
        }
        attrs
    }

    fn push_run(&mut self, text: &str, attrs: Attrs) {
        if text.is_empty() {
            return;
        }
        let width = text.width();
        self.current_line.push(Run {
            text: text.to_string(),
            attrs,
        });
        self.current_width += width;
    }

    fn flush_line(&mut self) {
        if !self.current_line.is_empty() {
            self.lines.push(Line {
                runs: std::mem::take(&mut self.current_line),
                wrap: self.current_wrap,
            });
        }
        self.current_wrap = LineWrap::default();
        self.current_width = 0;
    }

    /// Flush a line inside a code block, preserving blank lines by emitting an
    /// empty placeholder run tagged with the language so sinks can render the
    /// code-block background.
    fn flush_code_line(&mut self, lang: &str) {
        let runs = if self.current_line.is_empty() {
            vec![Run {
                text: String::new(),
                attrs: Attrs {
                    code_block_lang: Some(lang.to_string()),
                    ..Attrs::default()
                },
            }]
        } else {
            std::mem::take(&mut self.current_line)
        };
        self.lines.push(Line {
            runs,
            wrap: self.current_wrap,
        });
        self.current_wrap = LineWrap::default();
        self.current_width = 0;
    }

    /// Start a new line of the block, after a line break in the source.
    fn break_line_with_indent(&mut self) {
        self.flush_line();
        self.emit_continuation_prefixes();
    }

    /// Start a new line of the block where the current line ran out of
    /// width. The new line continues the current one.
    fn wrap_line(&mut self) {
        self.break_line_with_indent();
        self.current_wrap = LineWrap {
            continues_above: true,
            prefix_len: self
                .prefix_stack
                .iter()
                .map(|prefix| prefix.cont_text.len())
                .sum(),
        };
    }

    /// Push one styled run per prefix stack entry, producing e.g. "> " or
    /// spaces matching a list's bullet width. Called at the start of every
    /// wrapped / soft-broken continuation line.
    fn emit_continuation_prefixes(&mut self) {
        // Clone to avoid borrow conflicts with push_run's &mut self.
        let prefixes: Vec<Prefix> = self.prefix_stack.clone();
        for prefix in prefixes {
            self.push_run(&prefix.cont_text, prefix.cont_attrs);
        }
    }

    fn ensure_blank_line(&mut self) {
        self.flush_line();
        if self.lines.last().is_some_and(|l| !l.runs.is_empty()) {
            self.lines.push(Line::new(vec![]));
        }
    }

    fn start_tag(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                if !self.in_list_item_start
                    && !self.in_block_quote
                    && (!self.lines.is_empty() || !self.current_line.is_empty())
                {
                    self.ensure_blank_line();
                }
                self.in_list_item_start = false;
            }
            Tag::Heading { level, .. } => {
                self.ensure_blank_line();
                let level_num = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                self.heading_level = Some(level_num);
                let prefix = "#".repeat(level_num as usize) + " ";
                self.push_run(
                    &prefix,
                    Attrs {
                        heading: true,
                        heading_marker: true,
                        ..Attrs::default()
                    },
                );
                self.attrs_stack.push(Attrs {
                    heading: true,
                    ..Attrs::default()
                });
            }
            Tag::CodeBlock(kind) => {
                self.ensure_blank_line();
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                let fence = if lang.is_empty() {
                    "```".to_string()
                } else {
                    format!("```{}", lang)
                };
                self.push_run(
                    &fence,
                    Attrs {
                        dimmed: true,
                        code_fence: true,
                        ..Attrs::default()
                    },
                );
                self.flush_line();
                self.in_code_block = true;
                self.code_block_content.clear();
                self.code_block_lang = lang;
            }
            Tag::List(start) => {
                // Don't add blank line for nested lists inside list items
                if self.prefix_stack.is_empty() {
                    self.ensure_blank_line();
                }
                let depth = self.list_stack.len();
                self.list_stack.push(ListContext {
                    index: start,
                    depth,
                });
            }
            Tag::Item => {
                self.flush_line();
                let indent = self
                    .list_stack
                    .last()
                    .map(|ctx| "  ".repeat(ctx.depth))
                    .unwrap_or_default();
                let (bullet, is_numbered) = if let Some(ctx) = self.list_stack.last_mut() {
                    match &mut ctx.index {
                        None => {
                            let b = format!("{}- ", indent);
                            self.prefix_stack.push(Prefix {
                                cont_text: " ".repeat(b.width()),
                                cont_attrs: Attrs::default(),
                            });
                            (b, false)
                        }
                        Some(n) => {
                            let b = format!("{}{}. ", indent, n);
                            self.prefix_stack.push(Prefix {
                                cont_text: " ".repeat(b.width()),
                                cont_attrs: Attrs::default(),
                            });
                            *n += 1;
                            (b, true)
                        }
                    }
                } else {
                    (String::new(), false)
                };
                self.push_run(
                    &bullet,
                    if is_numbered {
                        Attrs {
                            dimmed: true,
                            ..Attrs::default()
                        }
                    } else {
                        Attrs::default()
                    },
                );
                self.in_list_item_start = true;
            }
            Tag::Emphasis => self.attrs_stack.push(Attrs {
                italic: true,
                ..Attrs::default()
            }),
            Tag::Strong => self.attrs_stack.push(Attrs {
                bold: true,
                ..Attrs::default()
            }),
            Tag::Strikethrough => self.attrs_stack.push(Attrs {
                strikethrough: true,
                ..Attrs::default()
            }),
            Tag::BlockQuote(_) => {
                self.ensure_blank_line();
                let quote_attrs = Attrs {
                    quote: true,
                    ..Attrs::default()
                };
                self.push_run("> ", quote_attrs.clone());
                self.prefix_stack.push(Prefix {
                    cont_text: "> ".to_string(),
                    cont_attrs: quote_attrs.clone(),
                });
                self.attrs_stack.push(quote_attrs);
                self.in_block_quote = true;
            }
            Tag::Link { dest_url, .. } => {
                self.link_url = Some(dest_url.to_string());
                self.attrs_stack.push(Attrs {
                    link: true,
                    underline: true,
                    ..Attrs::default()
                });
            }
            Tag::Table(_) => {
                self.ensure_blank_line();
                self.table_state = Some(TableState::new());
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(ref mut state) = self.table_state {
                    state.current_row = Vec::new();
                }
            }
            Tag::TableCell => {
                if let Some(ref mut state) = self.table_state {
                    state.current_cell = String::new();
                }
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
            }
            TagEnd::Heading(_) => {
                self.flush_line();
                self.attrs_stack.pop();
                self.heading_level = None;
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                let code = std::mem::take(&mut self.code_block_content);
                let rows = crate::markdown::wrap_code_lines(&code, self.max_width);
                let wrapped: String = rows.iter().map(|row| format!("{}\n", row.text)).collect();
                let lang = std::mem::take(&mut self.code_block_lang);
                let highlighted = crate::syntax::highlight_code_tui(&wrapped, &lang);
                if let Some(lines) = highlighted {
                    for (index, line_tokens) in lines.into_iter().enumerate() {
                        self.current_wrap = LineWrap {
                            continues_above: rows.get(index).is_some_and(|row| row.continues_above),
                            prefix_len: 0,
                        };
                        for token in line_tokens {
                            let text = token.text.trim_end_matches('\n');
                            if text.is_empty() {
                                continue;
                            }
                            self.push_run(
                                text,
                                Attrs {
                                    code_block_lang: Some(lang.clone()),
                                    fg: Some(token.fg),
                                    bold: token.bold,
                                    italic: token.italic,
                                    ..Attrs::default()
                                },
                            );
                        }
                        self.flush_code_line(&lang);
                    }
                } else {
                    for row in &rows {
                        self.current_wrap = LineWrap {
                            continues_above: row.continues_above,
                            prefix_len: 0,
                        };
                        if !row.text.is_empty() {
                            self.push_run(
                                &row.text,
                                Attrs {
                                    code_block_lang: Some(lang.clone()),
                                    ..Attrs::default()
                                },
                            );
                        }
                        self.flush_code_line(&lang);
                    }
                }
                self.push_run(
                    "```",
                    Attrs {
                        dimmed: true,
                        code_fence: true,
                        ..Attrs::default()
                    },
                );
                self.flush_line();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                self.in_list_item_start = false;
            }
            TagEnd::Item => {
                self.flush_line();
                self.prefix_stack.pop();
                self.in_list_item_start = false;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.attrs_stack.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.attrs_stack.pop();
                self.prefix_stack.pop();
                self.in_block_quote = false;
            }
            TagEnd::Link => {
                self.attrs_stack.pop();
                if let Some(url) = self.link_url.take() {
                    self.push_run(
                        &format!(" ({})", url),
                        Attrs {
                            link: true,
                            underline: true,
                            link_url: true,
                            ..Attrs::default()
                        },
                    );
                }
            }
            TagEnd::Table => {
                if let Some(state) = self.table_state.take() {
                    let table_lines = render_table_to_lines(&state.rows, self.max_width);
                    self.lines.extend(table_lines);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(ref mut state) = self.table_state {
                    let row = std::mem::take(&mut state.current_row);
                    state.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                if let Some(ref mut state) = self.table_state {
                    let cell = std::mem::take(&mut state.current_cell);
                    state.current_row.push(cell);
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        let text = expand_tabs(text, self.current_width, 8);

        if let Some(ref mut state) = self.table_state {
            state.current_cell.push_str(&text.replace('\n', " "));
            return;
        }

        if self.in_code_block {
            self.code_block_content.push_str(&text);
            return;
        }

        let attrs = self.current_attrs();

        // Normalize embedded newlines to spaces for regular text.
        let text = text.replace('\n', " ");

        // Handle text wrapping word-by-word
        for word in text.split_inclusive(char::is_whitespace) {
            let word_width = word.width();
            if self.current_width + word_width > self.max_width && self.current_width > 0 {
                self.wrap_line();
            }
            self.push_run(word, attrs.clone());
        }
    }

    fn inline_code(&mut self, code: &str) {
        if let Some(ref mut state) = self.table_state {
            state.current_cell.push_str(code);
            return;
        }

        let code_width = code.width();
        if self.current_width + code_width > self.max_width && self.current_width > 0 {
            self.wrap_line();
        }

        self.push_run(
            code,
            Attrs {
                code: true,
                ..Attrs::default()
            },
        );
    }

    fn soft_break(&mut self) {
        self.break_line_with_indent();
    }

    fn hard_break(&mut self) {
        self.break_line_with_indent();
    }

    fn rule(&mut self) {
        self.ensure_blank_line();
        let rule = "─".repeat(self.max_width.min(40));
        self.push_run(
            &rule,
            Attrs {
                dimmed: true,
                ..Attrs::default()
            },
        );
        self.flush_line();
    }

    fn finish(mut self) -> LayoutDoc {
        self.flush_line();
        // Trim trailing blank lines
        while self.lines.last().is_some_and(|l| l.runs.is_empty()) {
            self.lines.pop();
        }
        LayoutDoc {
            lines: self.lines,
            blocks: self
                .block_recorder
                .map(|recorder| recorder.blocks)
                .unwrap_or_default(),
        }
    }
}

/// Expand tab characters to spaces using tab stops.
fn expand_tabs(input: &str, start_col: usize, tab_width: usize) -> String {
    let mut out = String::with_capacity(input.len());
    let mut col = start_col;
    for ch in input.chars() {
        if ch == '\t' {
            let spaces = tab_width - (col % tab_width);
            out.extend(std::iter::repeat_n(' ', spaces));
            col += spaces;
        } else {
            out.push(ch);
            col += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    out
}

/// Render a table as layout lines with box-drawing characters.
fn render_table_to_lines(rows: &[Vec<String>], _max_width: usize) -> Vec<Line> {
    if rows.is_empty() {
        return vec![];
    }

    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut col_widths = vec![0usize; num_cols];

    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < num_cols {
                col_widths[i] = col_widths[i].max(cell.trim().width());
            }
        }
    }

    let h = '─';
    let v = '│';
    let tl = '┌';
    let tr = '┐';
    let bl = '└';
    let br = '┘';
    let lj = '├';
    let rj = '┤';
    let tj = '┬';
    let bj = '┴';
    let cj = '┼';

    let mut lines = Vec::new();

    let build_line = |left: char, mid: char, right: char| -> String {
        let mut line = String::new();
        line.push(left);
        for (i, &width) in col_widths.iter().enumerate() {
            line.extend(std::iter::repeat_n(h, width + 2));
            if i < col_widths.len() - 1 {
                line.push(mid);
            }
        }
        line.push(right);
        line
    };

    let border_attrs = Attrs {
        dimmed: true,
        ..Attrs::default()
    };

    // Top border
    lines.push(Line::new(vec![Run {
        text: build_line(tl, tj, tr),
        attrs: border_attrs.clone(),
    }]));

    for (row_idx, row) in rows.iter().enumerate() {
        // Row content
        let mut runs = Vec::new();
        runs.push(Run {
            text: v.to_string(),
            attrs: border_attrs.clone(),
        });
        for (i, width) in col_widths.iter().enumerate() {
            let cell = row.get(i).map(|s| s.trim()).unwrap_or("");
            let cell_width = cell.width();
            let padding = width.saturating_sub(cell_width);
            runs.push(Run {
                text: format!(" {} ", cell),
                attrs: Attrs::default(),
            });
            if padding > 0 {
                runs.push(Run {
                    text: " ".repeat(padding),
                    attrs: Attrs::default(),
                });
            }
            runs.push(Run {
                text: v.to_string(),
                attrs: border_attrs.clone(),
            });
        }
        lines.push(Line::new(runs));

        // Separator
        if row_idx < rows.len() - 1 {
            lines.push(Line::new(vec![Run {
                text: build_line(lj, cj, rj),
                attrs: border_attrs.clone(),
            }]));
        }
    }

    // Bottom border
    lines.push(Line::new(vec![Run {
        text: build_line(bl, bj, br),
        attrs: border_attrs.clone(),
    }]));

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_to_text(input: &str, width: usize) -> String {
        let doc = LayoutEngine::render(input, width);
        doc.lines
            .iter()
            .map(|line| {
                line.runs
                    .iter()
                    .map(|r| r.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_plain_text() {
        let result = render_to_text("Hello world", 80);
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn test_heading() {
        let result = render_to_text("# Heading 1", 80);
        assert!(result.contains("Heading 1"));
    }

    #[test]
    fn test_list() {
        let result = render_to_text("- item 1\n- item 2", 80);
        assert!(result.contains("- item 1"));
        assert!(result.contains("- item 2"));
    }

    #[test]
    fn test_soft_breaks_preserved() {
        let input = "Line one\nLine two";
        let result = render_to_text(input, 80);
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 2, "Expected 2 lines, got: {:?}", lines);
    }

    #[test]
    fn test_list_continuation_indent() {
        let input = "- Item 1\n  continuation";
        let result = render_to_text(input, 80);
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 2, "Expected 2 lines, got: {:?}", lines);
        assert!(
            lines[1].starts_with("  "),
            "Continuation should be indented: {:?}",
            lines[1]
        );
    }

    #[test]
    fn test_numbered_list_prefix_width() {
        let input = "1. First\n2. Second";
        let result = render_to_text(input, 80);
        assert!(result.contains("1. First"));
        assert!(result.contains("2. Second"));
    }

    fn line_wraps(input: &str, width: usize) -> Vec<(String, LineWrap)> {
        LayoutEngine::render(input, width)
            .lines
            .into_iter()
            .map(|line| {
                let text = line.runs.iter().map(|run| run.text.as_str()).collect();
                (text, line.wrap)
            })
            .collect()
    }

    fn continues(prefix_len: usize) -> LineWrap {
        LineWrap {
            continues_above: true,
            prefix_len,
        }
    }

    #[test]
    fn wrapped_paragraph_continues_its_line_with_space_kept_above() {
        assert_eq!(
            line_wraps("cache rebuild now", 12),
            [
                ("cache ".to_string(), LineWrap::default()),
                ("rebuild now".to_string(), continues(0)),
            ]
        );
    }

    #[test]
    fn wrapped_list_item_or_quote_records_its_prefix() {
        let item = line_wraps("- cache rebuild", 10);
        assert_eq!(item[1], ("  rebuild".to_string(), continues(2)));
        let quote = line_wraps("> cache rebuild", 10);
        assert_eq!(quote[1], ("> rebuild".to_string(), continues(2)));
    }

    #[test]
    fn line_break_in_source_doesnt_continue_line_above() {
        let lines = line_wraps("first line\nsecond line", 80);
        assert_eq!(lines[1], ("second line".to_string(), LineWrap::default()));
    }

    const EVERY_BLOCK: &str = "# Title\n\nFirst line\nsecond line.\n\n```rust\nlet x = 1;\n```\n\n> quoted\n> more\n\n- a\n- b\n  - nested\n\n---\n\n| h | i |\n|---|---|\n| 1 | 2 |\n";

    fn texts_of(doc: &LayoutDoc, lines: &Range<usize>) -> Vec<String> {
        doc.lines[lines.clone()]
            .iter()
            .map(|line| line.runs.iter().map(|run| run.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn recorded_block_holds_its_source_and_its_lines() {
        let doc = LayoutEngine::render_with_blocks(EVERY_BLOCK, 80);
        let blocks: Vec<(BlockKind, &str, Vec<String>)> = doc
            .blocks
            .iter()
            .map(|block| {
                (
                    block.kind.clone(),
                    EVERY_BLOCK[block.source.clone()].trim_end(),
                    texts_of(&doc, &block.lines),
                )
            })
            .collect();
        assert_eq!(
            blocks,
            [
                (BlockKind::Heading, "# Title", vec!["# Title".to_owned()]),
                (
                    BlockKind::Paragraph,
                    "First line\nsecond line.",
                    vec!["First line".to_owned(), "second line.".to_owned()]
                ),
                (
                    BlockKind::CodeBlock {
                        code: "let x = 1;\n".to_owned()
                    },
                    "```rust\nlet x = 1;\n```",
                    vec![
                        "```rust".to_owned(),
                        "let x = 1;".to_owned(),
                        "```".to_owned()
                    ]
                ),
                (
                    BlockKind::Quote,
                    "> quoted\n> more",
                    vec!["> quoted".to_owned(), "> more".to_owned()]
                ),
                (
                    BlockKind::List,
                    "- a\n- b\n  - nested",
                    vec!["- a".to_owned(), "- b".to_owned(), "  - nested".to_owned()]
                ),
                (
                    BlockKind::Table,
                    "| h | i |\n|---|---|\n| 1 | 2 |",
                    texts_of(&doc, &(doc.lines.len() - 5..doc.lines.len()))
                ),
            ],
            "every block but the thematic break is recorded, without the blank line above it"
        );
    }

    #[test]
    fn list_item_holds_its_nested_list() {
        let doc = LayoutEngine::render_with_blocks(EVERY_BLOCK, 80);
        let list = doc
            .blocks
            .iter()
            .find(|block| block.kind == BlockKind::List)
            .unwrap();
        let items: Vec<(&str, Vec<String>)> = list
            .items
            .iter()
            .map(|item| {
                (
                    EVERY_BLOCK[item.source.clone()].trim_end(),
                    texts_of(&doc, &item.lines),
                )
            })
            .collect();
        assert_eq!(
            items,
            [
                ("- a", vec!["- a".to_owned()]),
                (
                    "- b\n  - nested",
                    vec!["- b".to_owned(), "  - nested".to_owned()]
                ),
            ]
        );
    }

    #[test]
    fn indented_code_block_records_code_without_indent() {
        let doc = LayoutEngine::render_with_blocks("Code:\n\n    first\n    second\n", 80);
        assert_eq!(
            doc.blocks[1].kind,
            BlockKind::CodeBlock {
                code: "first\nsecond\n".to_owned()
            }
        );
    }

    #[test]
    fn recording_blocks_doesnt_change_lines() {
        let html_between = "<div>\nboxed\n</div>\n\nAfter the box.\n\n<br>\n\n- item";
        let html_in_a_row = "<div>one</div>\n\n<div>two</div>\n";
        let html_before_and_after =
            "# Title\n\n<div>one</div>\n\n<div>two</div>\n\n```\ncode\n```\n\n<p>end</p>\n";
        for input in [
            EVERY_BLOCK,
            html_between,
            html_in_a_row,
            html_before_and_after,
        ] {
            let plain = LayoutEngine::render(input, 12);
            let recorded = LayoutEngine::render_with_blocks(input, 12);
            assert_eq!(
                texts_of(&recorded, &(0..recorded.lines.len())),
                texts_of(&plain, &(0..plain.lines.len())),
                "{input:?} lays out the same lines"
            );
            assert!(plain.blocks.is_empty(), "a plain render records no block");
        }
        let block_texts = |input: &str| {
            let recorded = LayoutEngine::render_with_blocks(input, 80);
            recorded
                .blocks
                .iter()
                .map(|block| texts_of(&recorded, &block.lines))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            block_texts(html_between),
            [vec!["After the box.".to_owned()], vec!["- item".to_owned()]],
            "an HTML block's line stays out of the next block"
        );
        assert_eq!(
            block_texts(html_before_and_after),
            [
                vec!["# Title".to_owned()],
                vec!["```".to_owned(), "code".to_owned(), "```".to_owned()]
            ],
            "HTML blocks' line stays out of the blocks around it"
        );
    }

    #[test]
    fn code_line_cut_at_width_continues_on_next_row() {
        let lines = line_wraps("```\nabcdefgh\nij\n```", 4);
        let wraps: Vec<bool> = lines.iter().map(|(_, wrap)| wrap.continues_above).collect();
        let texts: Vec<&str> = lines.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(texts, ["```", "abcd", "efgh", "ij", "```"]);
        assert_eq!(wraps, [false, false, true, false, false]);
    }
}
