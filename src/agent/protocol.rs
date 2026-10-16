use crate::agent::diagnostic::AgentError;
use crate::agent::diagnostic::{AgentWarning, format_warning_records, warning_atoms};
use crate::agent::hidden_parts::HiddenParts;
use crate::agent::refs::{MessageRange, ResolvedConversation};
use crate::agent::sanitize::sanitize_agent_text;
use crate::agent::time_span::{RangeSpans, TimeSpan, time_atoms};
use crate::agent::transcript::{
    AgentMessage, AgentMessagePart, AgentMessageRole, AgentTranscript, CommandLines,
    MESSAGE_ANCHOR_LEN, tool_call_read_text,
};
use crate::agent::visibility::ContentVisibility;
use crate::error::Result;
use crate::time_filter::TimeFilter;
use serde_json::Value;
use std::cell::OnceCell;
use std::collections::BTreeSet;
use std::ops::Range;
use std::str::FromStr;

const OUTLINE_SHORT_MESSAGE_LIMIT: usize = 20;
/// Visible messages per outline segment when the budget holds them; a
/// smaller budget gets larger segments.
const OUTLINE_MIN_SEGMENT_SIZE: usize = 10;
const SNIPPET_LIMIT: usize = 80;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageLineRange {
    pub start: usize,
    pub end: usize,
}

impl FromStr for MessageLineRange {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let (start, end) = value
            .split_once("..")
            .ok_or_else(|| "line range must use START..END".to_string())?;
        let start = start
            .parse::<usize>()
            .map_err(|_| "line range must contain positive integers".to_string())?;
        let end = end
            .parse::<usize>()
            .map_err(|_| "line range must contain positive integers".to_string())?;
        if start == 0 || end == 0 {
            return Err("line range starts at line 1".to_string());
        }
        if start > end {
            return Err("line range start must not exceed its end".to_string());
        }
        Ok(Self { start, end })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadSlice {
    Lines(MessageLineRange),
    Match { query: String, context: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolOptions {
    pub budget: Option<usize>,
    pub tools: bool,
    pub tool_results: bool,
    pub thinking: bool,
    pub subagents: bool,
    /// Reads and outlines hold only the messages inside it.
    pub window: TimeFilter,
}

impl ProtocolOptions {
    pub fn visibility(self) -> ContentVisibility {
        ContentVisibility {
            tools: self.tools,
            tool_results: self.tool_results,
            thinking: self.thinking,
            subagents: self.subagents,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReadRequest<'a> {
    pub resolved: &'a ResolvedConversation,
    pub transcript: &'a AgentTranscript,
    pub range: Option<MessageRange>,
}

#[derive(Clone, Debug)]
pub struct ProtocolFocus {
    pub conversation_full_ref: Option<String>,
    pub range: MessageRange,
}

#[derive(Clone, Debug)]
struct RenderedMessage<'a> {
    conversation: &'a ResolvedConversation,
    transcript: &'a AgentTranscript,
    message: &'a AgentMessage,
    times: Option<TimeSpan>,
    body: String,
    slice: Option<String>,
}

/// A record of `agent read` after its conversation record: a message it
/// shows, or a run of consecutive messages its flags hide whole.
#[derive(Clone, Debug)]
enum ReadEntry<'a> {
    Message {
        rendered: RenderedMessage<'a>,
        hidden: HiddenParts<'a>,
    },
    Hidden(HiddenRun<'a>),
}

impl<'a> ReadEntry<'a> {
    fn conversation(&self) -> &'a ResolvedConversation {
        match self {
            Self::Message { rendered, .. } => rendered.conversation,
            Self::Hidden(run) => run.conversation,
        }
    }

    fn first_ordinal(&self) -> usize {
        match self {
            Self::Message { rendered, .. } => rendered.message.ordinal,
            Self::Hidden(run) => run.first,
        }
    }

    fn last_ordinal(&self) -> usize {
        match self {
            Self::Message { rendered, .. } => rendered.message.ordinal,
            Self::Hidden(run) => run.last,
        }
    }

    fn hidden(&self) -> &HiddenParts<'a> {
        match self {
            Self::Message { hidden, .. } => hidden,
            Self::Hidden(run) => &run.hidden,
        }
    }

    fn record(&self) -> String {
        match self {
            Self::Message { rendered, hidden } => message_record(rendered, hidden),
            Self::Hidden(run) => run.record(),
        }
    }
}

/// Consecutive messages that do not show text; at least one hides a part.
#[derive(Clone, Debug)]
struct HiddenRun<'a> {
    conversation: &'a ResolvedConversation,
    first: usize,
    last: usize,
    hidden: HiddenParts<'a>,
    times: Option<TimeSpan>,
}

impl<'a> HiddenRun<'a> {
    fn new(
        conversation: &'a ResolvedConversation,
        message: &'a AgentMessage,
        hidden: HiddenParts<'a>,
    ) -> Self {
        Self {
            conversation,
            first: message.ordinal,
            last: message.ordinal,
            hidden,
            times: TimeSpan::of_message(message),
        }
    }

    fn push(&mut self, message: &'a AgentMessage, hidden: &HiddenParts<'a>) {
        self.last = message.ordinal;
        self.hidden.add(hidden);
        self.times = [self.times, TimeSpan::of_message(message)]
            .into_iter()
            .flatten()
            .reduce(TimeSpan::union);
    }

    /// `hidden m3..m5 tool-results=2 at=…`.
    fn record(&self) -> String {
        format!(
            "hidden {}{}{}\n",
            ordinal_range(self.first, self.last),
            self.hidden.count_atoms(),
            time_atoms(self.times)
        )
    }
}

/// The `agent read` header. `cut=`, `omit=` and the `hidden-` atoms depend on
/// the entries the budget selects; the other atoms are fixed here.
struct ReadHeader {
    budget: Option<usize>,
    policy: String,
    /// ` messages-in-window=N`, or empty without a window.
    window: String,
    /// ` warnings=N warning-records=M`, or empty without warning records.
    warnings: String,
}

impl ReadHeader {
    fn new(
        options: ProtocolOptions,
        messages_in_window: usize,
        warning_count: usize,
        warning_records: &[String],
    ) -> Self {
        Self {
            budget: options.budget,
            policy: options.visibility().atom(),
            window: window_atom(options.window, messages_in_window),
            warnings: warning_atoms(warning_count, warning_records),
        }
    }

    fn line(&self, cut: &str, omit: &str, hidden_atoms: &str) -> String {
        format!(
            "protocol agent-read cut={} chars={} policy={} omit={omit}{}{hidden_atoms}{}\n",
            escape_atom(cut),
            budget_atom(self.budget),
            self.policy,
            self.window,
            self.warnings
        )
    }

    /// The fixed atoms, with `every_hidden_atom`, the `hidden-` atoms of every
    /// entry: the header length the budget estimate counts beyond `cut=` and
    /// `chars=`.
    fn fixed_atoms(&self, every_hidden_atom: &str) -> String {
        format!(
            " policy={}{}{every_hidden_atom}{}",
            self.policy, self.window, self.warnings
        )
    }
}

#[cfg(test)]
pub fn format_read(
    requests: &[ReadRequest<'_>],
    focus: Option<ProtocolFocus>,
    slice: Option<&ReadSlice>,
    options: ProtocolOptions,
) -> Result<String> {
    format_read_with_warnings(requests, focus, slice, options, &[])
}

pub fn format_read_with_warnings(
    requests: &[ReadRequest<'_>],
    focus: Option<ProtocolFocus>,
    slice: Option<&ReadSlice>,
    options: ProtocolOptions,
    warnings: &[AgentWarning],
) -> Result<String> {
    let commands = if reads_one_message(requests) {
        CommandLines::All
    } else {
        CommandLines::FirstLine
    };
    let mut entries = Vec::new();
    let mut messages_in_window = 0;
    for request in requests {
        let selected = selected_messages(request.transcript, request.range)?;
        messages_in_window += selected
            .iter()
            .filter(|message| is_in_window(message, options.window))
            .count();
        entries.extend(read_entries(request, selected, options, commands, slice)?);
    }
    let (warning_count, warning_records) = format_warning_records(warnings);
    let header = ReadHeader::new(options, messages_in_window, warning_count, &warning_records);
    let every_hidden_atom = hidden_in(&entries, 0..entries.len()).total_atoms();

    let protected = focused_indices(&entries, focus.as_ref());
    let mut selected = select_for_budget(
        &entries,
        focus,
        options.budget,
        &header.fixed_atoms(&every_hidden_atom),
    );
    let mut cut = cut_marker(entries.len(), &selected);
    let render = |entries: &[ReadEntry<'_>], selected: &[usize], cut: &str| {
        let hidden_atoms = hidden_in(entries, selected.iter().copied()).total_atoms();
        let mut output = header.line(
            cut,
            &omitted_message_ranges(entries, selected),
            &hidden_atoms,
        );
        let mut last_ref = None;
        render_selected_entries(&mut output, entries, selected, &mut last_ref);
        for record in continuation_records(entries, selected) {
            output.push_str(&record);
        }
        for record in &warning_records {
            if options
                .budget
                .is_none_or(|budget| output.chars().count() + record.chars().count() <= budget)
            {
                output.push_str(record);
            }
        }
        output
    };

    let mut output = render(&entries, &selected, &cut);
    if let Some(budget) = options.budget
        && output.chars().count() > budget
    {
        cut = if selected.len() == entries.len() {
            "body".to_string()
        } else {
            cut
        };
        if !protected.is_empty() {
            selected.retain(|index| protected.contains(index));
            cut = cut_marker(entries.len(), &selected);
        }
        trim_selected_message_bodies(&mut entries, &selected, budget, &render, &cut);
        output = render(&entries, &selected, &cut);
        while output.chars().count() > budget && !selected.is_empty() {
            let removable = selected
                .iter()
                .rposition(|index| !protected.contains(index));
            let Some(removable) = removable else {
                break;
            };
            selected.remove(removable);
            cut = cut_marker(entries.len(), &selected);
            trim_selected_message_bodies(&mut entries, &selected, budget, &render, &cut);
            output = render(&entries, &selected, &cut);
        }
        if output.chars().count() > budget {
            output = output.chars().take(budget).collect();
        }
    }
    Ok(output)
}

#[cfg(test)]
pub fn format_outline(
    resolved: &ResolvedConversation,
    transcript: &AgentTranscript,
    options: ProtocolOptions,
) -> String {
    format_outline_with_warnings(resolved, transcript, options, &[])
}

pub fn format_outline_with_warnings(
    resolved: &ResolvedConversation,
    transcript: &AgentTranscript,
    options: ProtocolOptions,
    warnings: &[AgentWarning],
) -> String {
    let messages = OutlineMessages::new(resolved, transcript, options);
    let messages_in_window = transcript
        .messages
        .iter()
        .filter(|message| is_in_window(message, options.window))
        .count();
    let header_fields = format!(
        "policy={} messages={}{}{}",
        options.visibility().atom(),
        transcript.messages.len(),
        time_atoms(TimeSpan::of_messages(&transcript.messages)),
        window_atom(options.window, messages_in_window)
    );
    let (warning_count, warning_records) = format_warning_records(warnings);
    let header = format!(
        "protocol agent-outline cut=none chars={} {header_fields}{}\n",
        budget_atom(options.budget),
        warning_atoms(warning_count, &warning_records)
    );
    let conversation = conversation_record(resolved);
    let render = |records: &[OutlineRecord]| {
        let mut output = format!("{header}{conversation}");
        for record in records {
            output.push_str(&record.line);
        }
        for record in &warning_records {
            output.push_str(record);
        }
        output
    };

    let Some(budget) = options.budget else {
        return render(&messages.records(OUTLINE_MIN_SEGMENT_SIZE));
    };
    fitted_outline(&messages, budget, render).unwrap_or_else(|| {
        let coarsest = messages.records(messages.coarsest_segment_size());
        truncated_outline(&messages, &coarsest, budget, &header_fields, warning_count)
    })
}

/// The outline at the smallest segment size, from `OUTLINE_MIN_SEGMENT_SIZE`
/// up, that fits `budget`. `None` when one segment holding every visible
/// message does not fit, or, in a session of 20 or fewer visible messages,
/// when one record per message does not fit.
fn fitted_outline(
    messages: &OutlineMessages<'_>,
    budget: usize,
    render: impl Fn(&[OutlineRecord]) -> String,
) -> Option<String> {
    let fits = |size| render(&messages.measured_records(size)).chars().count() <= budget;
    let coarsest = messages.coarsest_segment_size();
    if !fits(coarsest) {
        return None;
    }
    let size = (OUTLINE_MIN_SEGMENT_SIZE..=coarsest).find(|&size| fits(size))?;
    Some(render(&messages.records(size)))
}

/// The leading `records` that fit `budget` under a `cut=tail` header, then a
/// `continue read` record for the omitted messages.
fn truncated_outline(
    messages: &OutlineMessages<'_>,
    records: &[OutlineRecord],
    budget: usize,
    header_fields: &str,
    warning_count: usize,
) -> String {
    let conversation = conversation_record(messages.resolved);
    for keep in (0..=records.len()).rev() {
        let omitted = records.len() - keep;
        let mut truncated = format!(
            "protocol agent-outline cut=tail chars={budget} {header_fields} omitted-records={omitted} warnings={warning_count} warnings-emitted=0\n"
        );
        truncated.push_str(&conversation);
        for record in &records[..keep] {
            truncated.push_str(&record.line);
        }
        if let Some(first_omitted) = records.get(keep) {
            truncated.push_str(&format!(
                "continue read ref={}:m{}..m{}\n",
                messages.resolved.reference.canonical(),
                messages.ordinal(first_omitted.covers.start),
                messages.ordinal(messages.len() - 1)
            ));
        }
        if truncated.chars().count() <= budget {
            return truncated;
        }
    }
    format!(
        "protocol agent-outline cut=tail chars={budget} {header_fields} omitted-records={}\n",
        records.len()
    )
    .chars()
    .take(budget)
    .collect()
}

/// One line of an outline, and the indices of the visible messages it covers.
struct OutlineRecord {
    line: String,
    covers: Range<usize>,
}

/// The messages an outline shows. A segment's character count and times are
/// found in constant time, and each message's snippet is made on first use,
/// because choosing a segment size renders an outline per candidate size.
struct OutlineMessages<'a> {
    resolved: &'a ResolvedConversation,
    visible: Vec<RenderedMessage<'a>>,
    /// The characters of the visible messages before each index.
    chars_before: Vec<usize>,
    times: RangeSpans,
    snippets: Vec<OnceCell<String>>,
}

impl<'a> OutlineMessages<'a> {
    fn new(
        resolved: &'a ResolvedConversation,
        transcript: &'a AgentTranscript,
        options: ProtocolOptions,
    ) -> Self {
        let visible: Vec<_> = transcript
            .messages
            .iter()
            .filter(|message| is_in_window(message, options.window))
            .filter_map(|message| render_message(resolved, transcript, message, options))
            .collect();
        let chars_before = std::iter::once(0)
            .chain(visible.iter().scan(0, |total, message| {
                *total += message.body.chars().count();
                Some(*total)
            }))
            .collect();
        Self {
            resolved,
            chars_before,
            times: RangeSpans::new(visible.iter().map(|message| message.times).collect()),
            snippets: visible.iter().map(|_| OnceCell::new()).collect(),
            visible,
        }
    }

    fn len(&self) -> usize {
        self.visible.len()
    }

    /// The segment size that puts every visible message in one segment. A
    /// session of 20 or fewer visible messages has one record per message at
    /// any size.
    fn coarsest_segment_size(&self) -> usize {
        self.len().max(OUTLINE_MIN_SEGMENT_SIZE)
    }

    fn ordinal(&self, index: usize) -> usize {
        self.visible[index].message.ordinal
    }

    fn chars(&self, range: Range<usize>) -> usize {
        self.chars_before[range.end] - self.chars_before[range.start]
    }

    fn anchor(&self, index: usize) -> String {
        let rendered = &self.visible[index];
        rendered
            .transcript
            .message_anchor(rendered.conversation, rendered.message)
    }

    fn snippet(&self, index: usize) -> &str {
        self.snippets[index].get_or_init(|| snippet(&self.visible[index].body))
    }

    /// One record per message in a short session; otherwise one per
    /// `segment_size` messages.
    fn records(&self, segment_size: usize) -> Vec<OutlineRecord> {
        self.records_with(segment_size, &|index| self.anchor(index))
    }

    /// The records `records` makes, each anchor replaced by a stand-in of its
    /// length, to measure an outline without hashing its messages.
    fn measured_records(&self, segment_size: usize) -> Vec<OutlineRecord> {
        self.records_with(segment_size, &|_| "a".repeat(MESSAGE_ANCHOR_LEN))
    }

    fn records_with(
        &self,
        segment_size: usize,
        anchor: &dyn Fn(usize) -> String,
    ) -> Vec<OutlineRecord> {
        let count = self.len();
        if count <= OUTLINE_SHORT_MESSAGE_LIMIT {
            return (0..count)
                .map(|index| self.message_record(index, anchor))
                .collect();
        }
        (0..count)
            .step_by(segment_size)
            .map(|start| self.segment_record(start..count.min(start + segment_size), anchor))
            .collect()
    }

    fn message_record(&self, index: usize, anchor: &dyn Fn(usize) -> String) -> OutlineRecord {
        let rendered = &self.visible[index];
        OutlineRecord {
            line: format!(
                "m{} role={} chars={} anchor={}{} | {}\n",
                rendered.message.ordinal,
                role_atom(rendered.message.role),
                self.chars(index..index + 1),
                anchor(index),
                time_atoms(rendered.times),
                self.snippet(index)
            ),
            covers: index..index + 1,
        }
    }

    fn segment_record(
        &self,
        covers: Range<usize>,
        anchor: &dyn Fn(usize) -> String,
    ) -> OutlineRecord {
        let (first, last) = (covers.start, covers.end - 1);
        OutlineRecord {
            line: format!(
                "seg m{}..m{} chars={} anchors={}..{}{} | {} / {}\n",
                self.ordinal(first),
                self.ordinal(last),
                self.chars(covers.clone()),
                anchor(first),
                anchor(last),
                time_atoms(self.times.of(covers.clone())),
                self.snippet(first),
                self.snippet(last)
            ),
            covers,
        }
    }
}

fn is_in_window(message: &AgentMessage, window: TimeFilter) -> bool {
    window.contains_message(TimeSpan::of_message(message))
}

/// ` messages-in-window=N` for a window with bounds; empty without one.
fn window_atom(window: TimeFilter, messages_in_window: usize) -> String {
    if window.is_active() {
        format!(" messages-in-window={messages_in_window}")
    } else {
        String::new()
    }
}

fn conversation_record(resolved: &ResolvedConversation) -> String {
    format!(
        "conversation project={} uuid={} ref={}\n",
        escape_atom(&resolved.key.project_id()),
        escape_atom(&resolved.reference.uuid()),
        escape_atom(&resolved.reference.canonical())
    )
}

pub fn escape_atom(value: &str) -> String {
    let mut escaped = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'.'
            | b'_'
            | b'~'
            | b':'
            | b'/'
            | b'+'
            | b'-' => escaped.push(byte as char),
            _ => escaped.push_str(&format!("%{byte:02X}")),
        }
    }
    escaped
}

pub(crate) fn protocol_snippet(text: &str, limit: usize) -> String {
    let sanitized = sanitize_agent_text(text);
    let normalized = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= limit {
        normalized
    } else {
        let mut snippet = normalized
            .chars()
            .take(limit.saturating_sub(3))
            .collect::<String>();
        snippet.push_str("...");
        snippet
    }
}

pub(crate) fn title_for_conversation(conversation: &crate::history::Conversation) -> String {
    conversation
        .custom_title
        .as_deref()
        .or(conversation.summary.as_deref())
        .unwrap_or(&conversation.preview)
        .to_string()
}

fn selected_messages(
    transcript: &AgentTranscript,
    range: Option<MessageRange>,
) -> Result<Vec<&AgentMessage>> {
    let Some(range) = range else {
        return Ok(transcript.messages.iter().collect());
    };
    let max = transcript.messages.len();
    if range.end > max {
        return Err(AgentError::out_of_range(
            None,
            format!(
                "message range m{}..m{} exceeds transcript length m{}",
                range.start, range.end, max
            ),
        )
        .into());
    }
    Ok(transcript
        .messages
        .iter()
        .filter(|message| range.start <= message.ordinal && message.ordinal <= range.end)
        .collect())
}

/// The parts that the entries at `indices` hide.
fn hidden_in<'a>(
    entries: &[ReadEntry<'a>],
    indices: impl IntoIterator<Item = usize>,
) -> HiddenParts<'a> {
    let mut hidden = HiddenParts::default();
    for index in indices {
        hidden.add(entries[index].hidden());
    }
    hidden
}

/// True for a read of one message: `REF:m7`, or a ref with `--anchor`.
fn reads_one_message(requests: &[ReadRequest<'_>]) -> bool {
    matches!(requests, [request] if request.range.is_some_and(|range| range.is_single()))
}

/// Each message of `request` inside the window that shows text, and a
/// [`HiddenRun`] for each run of those that do not, when the run hides a
/// part. A message outside the window ends the run.
fn read_entries<'a>(
    request: &ReadRequest<'a>,
    messages: Vec<&'a AgentMessage>,
    options: ProtocolOptions,
    commands: CommandLines,
    slice: Option<&ReadSlice>,
) -> Result<Vec<ReadEntry<'a>>> {
    let mut entries = Vec::new();
    let mut run: Option<HiddenRun<'a>> = None;
    for message in messages {
        if !is_in_window(message, options.window) {
            entries.extend(hidden_run_entry(run.take()));
            continue;
        }
        let hidden = hidden_parts(message, options.visibility());
        let rendered = render_message_with_commands(
            request.resolved,
            request.transcript,
            message,
            options,
            commands,
        );
        match rendered {
            Some(rendered) => {
                entries.extend(hidden_run_entry(run.take()));
                entries.push(ReadEntry::Message {
                    rendered: apply_read_slice(rendered, slice)?,
                    hidden,
                });
            }
            None => match run.as_mut() {
                Some(current) => current.push(message, &hidden),
                None => run = Some(HiddenRun::new(request.resolved, message, hidden)),
            },
        }
    }
    entries.extend(hidden_run_entry(run));
    Ok(entries)
}

fn hidden_run_entry(run: Option<HiddenRun<'_>>) -> Option<ReadEntry<'_>> {
    run.filter(|run| !run.hidden.is_empty())
        .map(ReadEntry::Hidden)
}

/// The parts of `message` that `visibility` hides and that hold text. A
/// sub-agent message that `visibility` hides whole counts as one message.
fn hidden_parts(message: &AgentMessage, visibility: ContentVisibility) -> HiddenParts<'_> {
    let mut hidden = HiddenParts::default();
    if !visibility.message_is_visible(message) {
        if message.parts.iter().any(part_has_text) {
            hidden.add_subagent_message();
        }
        return hidden;
    }
    for part in &message.parts {
        if !visibility.part_is_visible(part) && part_has_text(part) {
            hidden.add_part(part);
        }
    }
    hidden
}

/// True when `part` holds text. Claude Code records most thinking parts
/// without text; a read neither shows nor counts them.
fn part_has_text(part: &AgentMessagePart) -> bool {
    match part {
        AgentMessagePart::Text { text, .. } => !text.trim().is_empty(),
        AgentMessagePart::ToolUse { .. } => true,
        AgentMessagePart::ToolResult { content, .. } => {
            content.as_ref().is_some_and(tool_result_has_text)
        }
        AgentMessagePart::Thinking { thinking, .. } => !thinking.trim().is_empty(),
    }
}

/// [`render_message_with_commands`] showing each command's first line.
fn render_message<'a>(
    conversation: &'a ResolvedConversation,
    transcript: &'a AgentTranscript,
    message: &'a AgentMessage,
    options: ProtocolOptions,
) -> Option<RenderedMessage<'a>> {
    render_message_with_commands(
        conversation,
        transcript,
        message,
        options,
        CommandLines::FirstLine,
    )
}

fn render_message_with_commands<'a>(
    conversation: &'a ResolvedConversation,
    transcript: &'a AgentTranscript,
    message: &'a AgentMessage,
    options: ProtocolOptions,
    commands: CommandLines,
) -> Option<RenderedMessage<'a>> {
    let visibility = options.visibility();
    if !visibility.message_is_visible(message) {
        return None;
    }
    let mut parts = Vec::new();
    for part in &message.parts {
        if !visibility.part_is_visible(part) || !part_has_text(part) {
            continue;
        }
        match part {
            AgentMessagePart::Text { text, .. } => parts.push(sanitize_agent_text(text)),
            AgentMessagePart::ToolUse {
                name, tool, input, ..
            } => parts.push(tool_call_read_text(name, *tool, input, commands)),
            AgentMessagePart::ToolResult {
                content: Some(content),
                ..
            } => {
                parts.push(sanitize_agent_text(&tool_result_text(content)));
            }
            AgentMessagePart::Thinking { thinking, .. } => {
                parts.push(format!("thinking: {}", sanitize_agent_text(thinking)));
            }
            AgentMessagePart::ToolResult { content: None, .. } => {}
        }
    }
    let body = parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!body.is_empty()).then(|| RenderedMessage {
        conversation,
        transcript,
        message,
        times: TimeSpan::of_message(message),
        body,
        slice: None,
    })
}

fn apply_read_slice<'a>(
    mut rendered: RenderedMessage<'a>,
    slice: Option<&ReadSlice>,
) -> Result<RenderedMessage<'a>> {
    let Some(slice) = slice else {
        return Ok(rendered);
    };
    let lines = rendered.body.split('\n').collect::<Vec<_>>();
    match slice {
        ReadSlice::Lines(range) => {
            if range.end > lines.len() {
                return Err(AgentError::out_of_range(
                    None,
                    format!(
                        "content line range {}..{} exceeds message m{} length {}",
                        range.start,
                        range.end,
                        rendered.message.ordinal,
                        lines.len()
                    ),
                )
                .into());
            }
            rendered.body = numbered_lines(&lines, range.start..=range.end, &[]);
            rendered.slice = Some(format!("lines={}..{}", range.start, range.end));
        }
        ReadSlice::Match { query, context } => {
            let query_lower = query.to_lowercase();
            let matches = lines
                .iter()
                .enumerate()
                .filter_map(|(index, line)| {
                    line.to_lowercase()
                        .contains(&query_lower)
                        .then_some(index + 1)
                })
                .collect::<Vec<_>>();
            let mut selected = BTreeSet::new();
            for line in &matches {
                let start = line.saturating_sub(*context).max(1);
                let end = line.saturating_add(*context).min(lines.len());
                selected.extend(start..=end);
            }
            rendered.body = numbered_lines(&lines, selected.iter().copied(), &matches);
            rendered.slice = Some(format!(
                "match={} context={} hits={}",
                escape_atom(query),
                context,
                matches.len()
            ));
        }
    }
    Ok(rendered)
}

fn numbered_lines(
    lines: &[&str],
    selected: impl IntoIterator<Item = usize>,
    matches: &[usize],
) -> String {
    let selected = selected.into_iter().collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut previous = None;
    for line_number in selected {
        if previous.is_some_and(|previous| line_number > previous + 1) {
            output.push("...".to_string());
        }
        let marker = if matches.contains(&line_number) {
            ">"
        } else {
            " "
        };
        output.push(format!("{marker}{line_number}: {}", lines[line_number - 1]));
        previous = Some(line_number);
    }
    output.join("\n")
}

fn trim_selected_message_bodies(
    entries: &mut [ReadEntry<'_>],
    selected: &[usize],
    budget: usize,
    render: &impl Fn(&[ReadEntry<'_>], &[usize], &str) -> String,
    cut: &str,
) {
    let bodies = selected
        .iter()
        .filter_map(|index| match &mut entries[*index] {
            ReadEntry::Message { rendered, .. } => {
                Some((*index, std::mem::take(&mut rendered.body)))
            }
            ReadEntry::Hidden(_) => None,
        })
        .collect::<Vec<_>>();
    if bodies.is_empty() {
        return;
    }
    let base_len = render(entries, selected, cut).chars().count();
    let available = budget.saturating_sub(base_len) / bodies.len();
    for (index, body) in bodies {
        if let ReadEntry::Message { rendered, .. } = &mut entries[index] {
            rendered.body = truncated_message_body(rendered, &body, available);
        }
    }
}

fn truncated_message_body(_rendered: &RenderedMessage<'_>, body: &str, available: usize) -> String {
    if framed_body_len(body) <= available {
        return body.to_string();
    }
    let total_chars = body.chars().count();
    let hint = format!("[omitted chars={{start}}..{total_chars}; use --lines or --match]");
    let mut low = 0;
    let mut high = total_chars;
    let mut best = String::new();
    while low <= high {
        let keep = low + (high - low) / 2;
        let start = keep + 1;
        let marker = hint.replace("{start}", &start.to_string());
        let mut candidate = body.chars().take(keep).collect::<String>();
        if keep > 0 && !candidate.ends_with('\n') {
            candidate.push('\n');
        }
        candidate.push_str(&marker);
        if framed_body_len(&candidate) <= available {
            best = candidate;
            low = keep + 1;
        } else if keep == 0 {
            break;
        } else {
            high = keep - 1;
        }
    }
    if best.is_empty() {
        let compact = format!("[omit c1..{total_chars}]");
        if framed_body_len(&compact) <= available {
            return compact;
        }
    }
    best
}

fn continuation_records(entries: &[ReadEntry<'_>], selected: &[usize]) -> Vec<String> {
    let selected = selected.iter().copied().collect::<BTreeSet<_>>();
    let mut groups: Vec<(&ResolvedConversation, Vec<usize>)> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if selected.contains(&index) {
            continue;
        }
        let entry_ordinals = entry.first_ordinal()..=entry.last_ordinal();
        if let Some((_, ordinals)) = groups.iter_mut().find(|(conversation, _)| {
            conversation.reference.full_ref() == entry.conversation().reference.full_ref()
        }) {
            ordinals.extend(entry_ordinals);
        } else {
            groups.push((entry.conversation(), entry_ordinals.collect()));
        }
    }
    groups
        .into_iter()
        .map(|(conversation, ordinals)| {
            let refs = compact_ordinals(&conversation.reference.canonical(), &ordinals);
            format!("continue read refs={}\n", escape_atom(&refs.join(";")))
        })
        .collect()
}

fn compact_ordinals(reference: &str, ordinals: &[usize]) -> Vec<String> {
    let mut ranges = Vec::new();
    let mut start = ordinals[0];
    let mut end = start;
    for ordinal in ordinals.iter().copied().skip(1) {
        if ordinal == end + 1 {
            end = ordinal;
        } else {
            ranges.push(format_message_range(reference, start, end));
            start = ordinal;
            end = ordinal;
        }
    }
    ranges.push(format_message_range(reference, start, end));
    ranges
}

fn format_message_range(reference: &str, start: usize, end: usize) -> String {
    format!("{reference}:{}", ordinal_range(start, end))
}

/// `m7`, or `m7..m9`.
fn ordinal_range(start: usize, end: usize) -> String {
    if start == end {
        format!("m{start}")
    } else {
        format!("m{start}..m{end}")
    }
}

fn omitted_message_ranges(entries: &[ReadEntry<'_>], selected: &[usize]) -> String {
    let qualify = entries.first().is_some_and(|first| {
        entries.iter().any(|entry| {
            entry.conversation().reference.full_ref() != first.conversation().reference.full_ref()
        })
    });
    let selected = selected.iter().copied().collect::<BTreeSet<_>>();
    let mut ranges = Vec::new();
    let mut start = None;
    for index in 0..=entries.len() {
        let omitted = index < entries.len() && !selected.contains(&index);
        match (start, omitted) {
            (None, true) => start = Some(index),
            (Some(first), false) => {
                let last = index - 1;
                let first_entry = &entries[first];
                let last_entry = &entries[last];
                let prefix = if qualify {
                    format!("{}:", first_entry.conversation().reference.canonical())
                } else {
                    Default::default()
                };
                if first_entry.conversation().reference.full_ref()
                    == last_entry.conversation().reference.full_ref()
                {
                    ranges.push(format!(
                        "{prefix}{}",
                        ordinal_range(first_entry.first_ordinal(), last_entry.last_ordinal())
                    ));
                } else {
                    for entry in &entries[first..=last] {
                        ranges.push(format_message_range(
                            &entry.conversation().reference.canonical(),
                            entry.first_ordinal(),
                            entry.last_ordinal(),
                        ));
                    }
                }
                start = None;
            }
            _ => {}
        }
    }
    if ranges.is_empty() {
        "none".to_string()
    } else {
        ranges.join(",")
    }
}

fn framed_body_len(body: &str) -> usize {
    body.split('\n').map(framed_line_len).sum()
}

fn framed_line_len(line: &str) -> usize {
    if line.is_empty() {
        2
    } else {
        line.chars().count() + 3
    }
}

/// The entries holding a message inside `focus`.
fn focused_indices(entries: &[ReadEntry<'_>], focus: Option<&ProtocolFocus>) -> BTreeSet<usize> {
    let Some(focus) = focus else {
        return BTreeSet::new();
    };
    entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let conversation_matches = focus
                .conversation_full_ref
                .as_deref()
                .is_none_or(|target| entry.conversation().reference.full_ref() == target);
            (conversation_matches
                && focus.range.start <= entry.last_ordinal()
                && entry.first_ordinal() <= focus.range.end)
                .then_some(index)
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SelectionWork {
    serialized_entries: usize,
    candidate_checks: usize,
}

struct CompactReadLayout<'h> {
    entry_chars: Vec<usize>,
    conversation_chars: Vec<usize>,
    conversation_refs: Vec<String>,
    /// [`ReadHeader::fixed_atoms`], with the `hidden-` atoms for every entry.
    /// A header shows the `hidden-` atoms of the entries it shows, which are
    /// never longer.
    header_atoms: &'h str,
}

impl<'h> CompactReadLayout<'h> {
    fn new(entries: &[ReadEntry<'_>], header_atoms: &'h str) -> Self {
        Self {
            entry_chars: entries
                .iter()
                .map(|entry| entry.record().chars().count())
                .collect(),
            conversation_chars: entries
                .iter()
                .map(|entry| conversation_record(entry.conversation()).chars().count())
                .collect(),
            conversation_refs: entries
                .iter()
                .map(|entry| entry.conversation().reference.full_ref())
                .collect(),
            header_atoms,
        }
    }

    fn rendered_chars(&self, selected: &BTreeSet<usize>, cut: &str, budget: usize) -> usize {
        let header_atoms = self.header_atoms;
        let mut chars =
            format!("protocol agent-read cut={cut} budget-chars={budget}{header_atoms}\n")
                .chars()
                .count();
        let mut last_ref = None;
        for index in selected {
            if last_ref != Some(self.conversation_refs[*index].as_str()) {
                chars += self.conversation_chars[*index];
                last_ref = Some(self.conversation_refs[*index].as_str());
            }
            chars += self.entry_chars[*index];
        }
        chars
    }

    fn expansion_chars(&self, candidate: usize, neighbor: usize) -> usize {
        self.entry_chars[candidate]
            + if self.conversation_refs[candidate] == self.conversation_refs[neighbor] {
                0
            } else {
                self.conversation_chars[candidate]
            }
    }
}

fn select_for_budget(
    entries: &[ReadEntry<'_>],
    focus: Option<ProtocolFocus>,
    budget: Option<usize>,
    header_atoms: &str,
) -> Vec<usize> {
    select_for_budget_with_work(entries, focus, budget, header_atoms).0
}

fn select_for_budget_with_work(
    entries: &[ReadEntry<'_>],
    focus: Option<ProtocolFocus>,
    budget: Option<usize>,
    header_atoms: &str,
) -> (Vec<usize>, SelectionWork) {
    let all = (0..entries.len()).collect::<BTreeSet<_>>();
    let Some(budget) = budget else {
        return (
            all.into_iter().collect(),
            SelectionWork {
                serialized_entries: entries.len(),
                candidate_checks: 0,
            },
        );
    };
    let layout = CompactReadLayout::new(entries, header_atoms);
    let mut work = SelectionWork {
        serialized_entries: entries.len(),
        candidate_checks: 0,
    };
    if layout.rendered_chars(&all, "none", budget) <= budget {
        return (all.into_iter().collect(), work);
    }

    let mut selected = focused_indices(entries, focus.as_ref());
    if selected.is_empty() && !entries.is_empty() {
        selected.insert(0);
    }

    let mut rendered_chars = layout.rendered_chars(&selected, "head+focus+tail", budget);
    loop {
        let mut changed = false;
        if let Some(first) = selected.first().copied()
            && first > 0
        {
            work.candidate_checks += 1;
            let candidate = first - 1;
            let added = layout.expansion_chars(candidate, first);
            if rendered_chars.saturating_add(added) <= budget {
                selected.insert(candidate);
                rendered_chars += added;
                changed = true;
            }
        }
        if let Some(last) = selected.last().copied()
            && last + 1 < entries.len()
        {
            work.candidate_checks += 1;
            let candidate = last + 1;
            let added = layout.expansion_chars(candidate, last);
            if rendered_chars.saturating_add(added) <= budget {
                selected.insert(candidate);
                rendered_chars += added;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    (selected.into_iter().collect(), work)
}

fn message_record(rendered: &RenderedMessage<'_>, hidden: &HiddenParts<'_>) -> String {
    let mut output = format!(
        "message m{} role={} line={}{} anchor={}{}{}\n",
        rendered.message.ordinal,
        role_atom(rendered.message.role),
        rendered.message.jsonl_line,
        rendered
            .slice
            .as_ref()
            .map(|slice| format!(" slice={slice}"))
            .unwrap_or_default(),
        rendered
            .transcript
            .message_anchor(rendered.conversation, rendered.message),
        hidden.message_atoms(),
        time_atoms(rendered.times)
    );
    push_body(&mut output, &rendered.body);
    output
}

fn render_selected_entries(
    output: &mut String,
    entries: &[ReadEntry<'_>],
    selected: &[usize],
    last_ref: &mut Option<String>,
) {
    for index in selected {
        let entry = &entries[*index];
        let canonical = entry.conversation().reference.canonical();
        if last_ref.as_deref() != Some(canonical.as_str()) {
            output.push_str(&conversation_record(entry.conversation()));
            *last_ref = Some(canonical);
        }
        output.push_str(&entry.record());
    }
}

fn cut_marker(total: usize, selected: &[usize]) -> String {
    if selected.len() == total {
        return "none".to_string();
    }
    let Some(first) = selected.first().copied() else {
        return "tail".to_string();
    };
    let last = selected.last().copied().unwrap_or(first);
    let mut parts = Vec::new();
    if first > 0 {
        parts.push("head");
    }
    if selected.windows(2).any(|pair| pair[1] != pair[0] + 1) || (first > 0 && last + 1 < total) {
        parts.push("focus");
    }
    if last + 1 < total {
        parts.push("tail");
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join("+")
    }
}

fn push_body(output: &mut String, body: &str) {
    for line in body.split('\n') {
        if line.is_empty() {
            output.push_str("|\n");
        } else {
            output.push_str("| ");
            output.push_str(line);
            output.push('\n');
        }
    }
}

fn role_atom(role: AgentMessageRole) -> &'static str {
    match role {
        AgentMessageRole::User => "user",
        AgentMessageRole::Assistant => "assistant",
    }
}

pub(crate) fn budget_atom(budget: Option<usize>) -> String {
    budget
        .map(|budget| budget.to_string())
        .unwrap_or_else(|| "none".to_string())
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(text.clone()),
                Value::Object(map) => map
                    .get("text")
                    .and_then(|value| value.as_str())
                    .map(ToOwned::to_owned),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => content.to_string(),
    }
}

/// True when [`tool_result_text`] is not blank, without building the text.
fn tool_result_has_text(content: &Value) -> bool {
    let is_text = |text: &str| !text.trim().is_empty();
    match content {
        Value::String(text) => is_text(text),
        Value::Array(items) => items.iter().any(|item| match item {
            Value::String(text) => is_text(text),
            Value::Object(map) => map.get("text").and_then(Value::as_str).is_some_and(is_text),
            _ => false,
        }),
        _ => true,
    }
}

/// The first `SNIPPET_LIMIT` characters of `body`, without its leading and
/// trailing whitespace and with each inner run of whitespace as one space.
fn snippet(body: &str) -> String {
    let mut normalized = String::new();
    let mut chars = 0;
    for word in body.split_whitespace() {
        if chars >= SNIPPET_LIMIT {
            break;
        }
        if chars > 0 {
            normalized.push(' ');
            chars += 1;
        }
        normalized.push_str(word);
        chars += word.chars().count();
    }
    normalized.chars().take(SNIPPET_LIMIT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::refs::{AgentConversationKey, AgentConversationRef};
    use crate::agent::test_support::{local_time, source, text_message, text_message_at};
    use crate::agent::transcript::AgentPartSource;
    use crate::log_entry::Tool;
    use serde_json::json;
    use std::path::PathBuf;

    fn resolved(filename: &str) -> ResolvedConversation {
        let key =
            AgentConversationKey::new("project with space", filename, PathBuf::from(filename));
        ResolvedConversation {
            reference: AgentConversationRef::from_parts("project with space", filename),
            key,
        }
    }

    fn transcript(messages: Vec<AgentMessage>) -> AgentTranscript {
        crate::agent::test_support::transcript(messages, "test.jsonl")
    }

    fn options() -> ProtocolOptions {
        ProtocolOptions {
            budget: Some(6000),
            tools: false,
            tool_results: false,
            thinking: false,
            subagents: false,
            window: TimeFilter::default(),
        }
    }

    /// A read entry for `rendered`, a message that does not hide a part.
    fn shown(rendered: RenderedMessage<'_>) -> ReadEntry<'_> {
        ReadEntry::Message {
            rendered,
            hidden: HiddenParts::default(),
        }
    }

    fn legacy_select_for_budget(
        messages: &[ReadEntry<'_>],
        focus: Option<ProtocolFocus>,
        budget: usize,
    ) -> Vec<usize> {
        let render_chars = |selected: &BTreeSet<usize>, cut: &str| {
            let mut output = format!("protocol agent-read cut={cut} budget-chars={budget}\n");
            let mut last_ref = None;
            render_selected_entries(
                &mut output,
                messages,
                &selected.iter().copied().collect::<Vec<_>>(),
                &mut last_ref,
            );
            output.chars().count()
        };
        let all = (0..messages.len()).collect::<BTreeSet<_>>();
        if render_chars(&all, "none") <= budget {
            return all.into_iter().collect();
        }
        let mut selected = BTreeSet::new();
        if let Some(focus) = focus {
            for (index, entry) in messages.iter().enumerate() {
                let conversation_matches = focus
                    .conversation_full_ref
                    .as_deref()
                    .is_none_or(|target| entry.conversation().reference.full_ref() == target);
                if conversation_matches
                    && focus.range.start <= entry.first_ordinal()
                    && entry.first_ordinal() <= focus.range.end
                {
                    selected.insert(index);
                }
            }
        }
        if selected.is_empty() && !messages.is_empty() {
            selected.insert(0);
        }
        loop {
            let mut changed = false;
            if let Some(first) = selected.first().copied()
                && first > 0
            {
                selected.insert(first - 1);
                if render_chars(&selected, "head+focus+tail") <= budget {
                    changed = true;
                } else {
                    selected.remove(&(first - 1));
                }
            }
            if let Some(last) = selected.last().copied()
                && last + 1 < messages.len()
            {
                selected.insert(last + 1);
                if render_chars(&selected, "head+focus+tail") <= budget {
                    changed = true;
                } else {
                    selected.remove(&(last + 1));
                }
            }
            if !changed {
                break;
            }
        }
        selected.into_iter().collect()
    }

    #[test]
    fn compact_read_layout_matches_legacy_selection_with_linear_serialization() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(
            (1..=40)
                .map(|ordinal| {
                    text_message(
                        ordinal,
                        AgentMessageRole::User,
                        &format!("message {ordinal} {}", "🙂x".repeat(ordinal % 7 + 1)),
                    )
                })
                .collect(),
        );
        let messages = transcript
            .messages
            .iter()
            .filter_map(|message| render_message(&resolved, &transcript, message, options()))
            .map(shown)
            .collect::<Vec<_>>();

        for budget in (100..=4_000).step_by(37) {
            let focus = ProtocolFocus {
                conversation_full_ref: Some(resolved.reference.full_ref()),
                range: MessageRange::single(20),
            };
            let expected = legacy_select_for_budget(&messages, Some(focus.clone()), budget);
            let (actual, work) =
                select_for_budget_with_work(&messages, Some(focus), Some(budget), "");

            assert_eq!(actual, expected, "selection differs at budget {budget}");
            assert_eq!(work.serialized_entries, messages.len());
            assert!(work.candidate_checks <= messages.len().saturating_mul(2));
        }
    }

    #[test]
    fn compact_read_layout_accounts_for_conversation_boundaries() {
        let first = resolved("first.jsonl");
        let second = resolved("second.jsonl");
        let first_transcript = transcript(
            (1..=8)
                .map(|ordinal| text_message(ordinal, AgentMessageRole::User, "alpha message"))
                .collect(),
        );
        let second_transcript = transcript(
            (1..=8)
                .map(|ordinal| text_message(ordinal, AgentMessageRole::User, "beta message"))
                .collect(),
        );
        let messages = first_transcript
            .messages
            .iter()
            .filter_map(|message| render_message(&first, &first_transcript, message, options()))
            .chain(second_transcript.messages.iter().filter_map(|message| {
                render_message(&second, &second_transcript, message, options())
            }))
            .map(shown)
            .collect::<Vec<_>>();

        for budget in (100..=2_000).step_by(29) {
            let expected = legacy_select_for_budget(&messages, None, budget);
            let (actual, work) = select_for_budget_with_work(&messages, None, Some(budget), "");

            assert_eq!(actual, expected, "selection differs at budget {budget}");
            assert_eq!(work.serialized_entries, messages.len());
            assert!(work.candidate_checks <= messages.len().saturating_mul(2));
        }
    }

    #[test]
    fn escapes_header_atom_delimiters() {
        assert_eq!(escape_atom("a b%c=d|e\tf\ng"), "a%20b%25c%3Dd%7Ce%09f%0Ag");
    }

    #[test]
    fn read_defaults_hide_non_text_parts_and_frame_body_lines() {
        let resolved = resolved("session file.jsonl");
        let mut message = text_message(1, AgentMessageRole::Assistant, "hello\nprotocol fake");
        message.parts.push(AgentMessagePart::ToolUse {
            id: "toolu_1".to_string(),
            name: "Bash".to_string(),
            tool: Tool::Shell,
            input: json!({"command": "pwd"}),
            source: source(AgentMessageRole::Assistant),
        });
        message.parts.push(AgentMessagePart::Thinking {
            thinking: "secret".to_string(),
            source: source(AgentMessageRole::Assistant),
        });
        let transcript = transcript(vec![message]);
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: None,
            }],
            None,
            None,
            options(),
        )
        .unwrap();

        assert!(output.starts_with(
            "protocol agent-read cut=none chars=6000 policy=text omit=none hidden-tools=1 hidden-thinking=1\n"
        ));
        assert!(output.contains("project=pr_"));
        assert!(!output.contains("path="));
        assert!(output.contains("| hello\n| protocol fake\n"));
        assert!(!output.contains("pwd"));
        assert!(!output.contains("secret"));
        assert!(!output.contains("\x1b["));
    }

    #[test]
    fn read_with_tools_shows_a_launching_calls_prompt() {
        let resolved = resolved("session.jsonl");
        let mut message = text_message(1, AgentMessageRole::Assistant, "Starting a reviewer.");
        message.parts.push(AgentMessagePart::ToolUse {
            id: "toolu_1".to_string(),
            name: "Agent".to_string(),
            tool: Tool::Agent,
            input: json!({"description": "Review the parser", "prompt": "Read src/parser.rs and list its defects."}),
            source: source(AgentMessageRole::Assistant),
        });
        let transcript = transcript(vec![message]);
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: None,
            }],
            None,
            None,
            ProtocolOptions {
                tools: true,
                ..options()
            },
        )
        .unwrap();

        assert!(
            output.contains("Read src/parser.rs and list its defects."),
            "{output}"
        );
    }

    #[test]
    fn read_preserves_focus_when_budget_truncates() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(
            (1..=7)
                .map(|index| {
                    text_message(
                        index,
                        AgentMessageRole::User,
                        &format!("message {index} with padding padding padding"),
                    )
                })
                .collect(),
        );
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: Some(MessageRange { start: 1, end: 7 }),
            }],
            Some(ProtocolFocus {
                conversation_full_ref: None,
                range: MessageRange::single(4),
            }),
            None,
            ProtocolOptions {
                budget: Some(800),
                ..options()
            },
        )
        .unwrap();

        assert!(output.starts_with("protocol agent-read cut="));
        assert!(output.lines().next().unwrap().contains("chars=800"));
        assert!(output.lines().next().unwrap().contains("omit="));
        assert!(output.contains("message m4 role=user"));
        assert!(output.contains("message 4 with padding"));
    }

    #[test]
    fn no_budget_read_emits_full_output_with_no_cut() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(vec![
            text_message(1, AgentMessageRole::User, "one"),
            text_message(2, AgentMessageRole::Assistant, "two"),
        ]);
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: None,
            }],
            None,
            None,
            ProtocolOptions {
                budget: None,
                ..options()
            },
        )
        .unwrap();

        assert!(
            output.starts_with("protocol agent-read cut=none chars=none policy=text omit=none\n")
        );
        assert!(output.contains("message m1 role=user"));
        assert!(output.contains("message m2 role=assistant"));
    }

    #[test]
    fn line_slice_returns_numbered_content_lines() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(vec![text_message(
            1,
            AgentMessageRole::Assistant,
            "one\ntwo\nthree\nfour\nfive",
        )]);
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: Some(MessageRange::single(1)),
            }],
            None,
            Some(&ReadSlice::Lines(MessageLineRange { start: 2, end: 4 })),
            options(),
        )
        .unwrap();

        assert!(output.contains("line=1 slice=lines=2..4 anchor=ma_"));
        assert!(output.contains("|  2: two\n|  3: three\n|  4: four\n"));
        assert!(!output.contains("|  1: one"));
        assert!(!output.contains("|  5: five"));
    }

    #[test]
    fn match_slice_returns_merged_numbered_context_windows() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(vec![text_message(
            1,
            AgentMessageRole::Assistant,
            "zero\nNeedle first\ntwo\nthree\nfour\nneedle second\nsix",
        )]);
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: Some(MessageRange::single(1)),
            }],
            None,
            Some(&ReadSlice::Match {
                query: "needle".to_string(),
                context: 1,
            }),
            options(),
        )
        .unwrap();

        assert!(output.contains("slice=match=needle context=1 hits=2 anchor=ma_"));
        assert!(output.contains("|  1: zero\n| >2: Needle first\n|  3: two\n| ...\n"));
        assert!(output.contains("|  5: four\n| >6: needle second\n|  7: six\n"));
        assert!(!output.contains("4: three"));
    }

    #[test]
    fn match_slice_reports_zero_hits_without_dumping_message() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(vec![text_message(1, AgentMessageRole::User, "haystack")]);
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: Some(MessageRange::single(1)),
            }],
            None,
            Some(&ReadSlice::Match {
                query: "needle".to_string(),
                context: 3,
            }),
            options(),
        )
        .unwrap();

        assert!(output.contains("slice=match=needle context=3 hits=0 anchor=ma_"));
        assert!(!output.contains("haystack"));
    }

    #[test]
    fn line_slice_rejects_ranges_past_message_end() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(vec![text_message(1, AgentMessageRole::User, "one\ntwo")]);
        let error = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: Some(MessageRange::single(1)),
            }],
            None,
            Some(&ReadSlice::Lines(MessageLineRange { start: 2, end: 3 })),
            options(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("exceeds message m1 length 2"));
    }

    #[test]
    fn sliced_message_truncates_content_within_budget() {
        let resolved = resolved("session.jsonl");
        let body = (1..=20)
            .map(|line| format!("needle line {line} with padding padding"))
            .collect::<Vec<_>>()
            .join("\n");
        let transcript = transcript(vec![text_message(1, AgentMessageRole::User, &body)]);
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: Some(MessageRange::single(1)),
            }],
            None,
            Some(&ReadSlice::Match {
                query: "needle".to_string(),
                context: 0,
            }),
            ProtocolOptions {
                budget: Some(400),
                ..options()
            },
        )
        .unwrap();

        assert!(
            output.starts_with("protocol agent-read cut=body chars=400 policy=text omit=none\n")
        );
        assert!(output.contains("| >1: needle line 1"));
        assert!(output.chars().count() <= 400);
    }

    #[test]
    fn non_focused_oversized_read_respects_budget_with_header_only_cut() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(vec![text_message(
            1,
            AgentMessageRole::User,
            &"x".repeat(500),
        )]);
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: None,
            }],
            None,
            None,
            ProtocolOptions {
                budget: Some(80),
                ..options()
            },
        )
        .unwrap();

        assert!(output.starts_with("protocol agent-read cut="));
        assert!(output.chars().count() <= 80);
    }

    #[test]
    fn focused_oversized_message_is_truncated_inside_the_message() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(vec![
            text_message(1, AgentMessageRole::User, "before"),
            text_message(2, AgentMessageRole::Assistant, &"界".repeat(500)),
            text_message(3, AgentMessageRole::User, "after"),
        ]);
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: None,
            }],
            Some(ProtocolFocus {
                conversation_full_ref: None,
                range: MessageRange::single(2),
            }),
            None,
            ProtocolOptions {
                budget: Some(500),
                ..options()
            },
        )
        .unwrap();

        assert!(output.chars().count() <= 500);
        assert!(output.contains("message m2 role=assistant"));
        assert!(output.contains("omitted chars="));
        assert!(output.contains("use --lines or --match"));
    }

    #[test]
    fn multiple_focused_messages_share_the_hard_budget() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(
            (1..=4)
                .map(|ordinal| {
                    text_message(
                        ordinal,
                        AgentMessageRole::Assistant,
                        &format!("focus {ordinal} {}", "x".repeat(300)),
                    )
                })
                .collect(),
        );
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: None,
            }],
            Some(ProtocolFocus {
                conversation_full_ref: None,
                range: MessageRange { start: 2, end: 3 },
            }),
            None,
            ProtocolOptions {
                budget: Some(420),
                ..options()
            },
        )
        .unwrap();

        assert!(output.chars().count() <= 420);
        assert!(output.contains("message m2 role=assistant"));
        assert!(output.contains("message m3 role=assistant"));
        assert!(output.contains("omit=m1,m4"));
    }

    #[test]
    fn read_sanitizes_visible_text_tools_results_and_thinking() {
        let resolved = resolved("session.jsonl");
        let mut message = text_message(1, AgentMessageRole::Assistant, "safe\u{1b}[31mred");
        message.parts.extend([
            AgentMessagePart::ToolUse {
                id: "toolu_1".to_string(),
                name: "Ba\u{1b}]0;title\u{7}sh".to_string(),
                tool: Tool::Shell,
                input: json!({"command": "pwd"}),
                source: source(AgentMessageRole::Assistant),
            },
            AgentMessagePart::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: Some(json!("ok\u{1b}[2Jdone")),
                source: source(AgentMessageRole::User),
            },
            AgentMessagePart::Thinking {
                thinking: "think\u{0}ing".to_string(),
                source: source(AgentMessageRole::Assistant),
            },
        ]);
        let transcript = transcript(vec![message]);
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: None,
            }],
            None,
            None,
            ProtocolOptions {
                tools: true,
                tool_results: true,
                thinking: true,
                ..options()
            },
        )
        .unwrap();

        assert!(output.contains("safered"));
        assert!(output.contains("| Bash: pwd\n"));
        assert!(output.contains("okdone"));
        assert!(output.contains("thinking: thinking"));
        assert!(!output.contains('\u{1b}'));
        assert!(!output.contains('\u{0}'));
    }

    #[test]
    fn qualified_focus_only_matches_target_conversation() {
        let first = resolved("first.jsonl");
        let second = resolved("second.jsonl");
        let first_transcript = transcript(vec![
            text_message(1, AgentMessageRole::User, "first one padding padding"),
            text_message(2, AgentMessageRole::User, "first two padding padding"),
        ]);
        let second_transcript = transcript(vec![
            text_message(1, AgentMessageRole::User, "second one padding padding"),
            text_message(2, AgentMessageRole::User, "second two padding padding"),
        ]);
        let output = format_read(
            &[
                ReadRequest {
                    resolved: &first,
                    transcript: &first_transcript,
                    range: None,
                },
                ReadRequest {
                    resolved: &second,
                    transcript: &second_transcript,
                    range: None,
                },
            ],
            Some(ProtocolFocus {
                conversation_full_ref: Some(second.reference.full_ref()),
                range: MessageRange::single(2),
            }),
            None,
            ProtocolOptions {
                budget: Some(500),
                ..options()
            },
        )
        .unwrap();

        assert!(output.contains("second two padding padding"));
        assert!(!output.contains("first two padding padding"));
    }

    #[test]
    fn snippet_truncates_to_eighty_characters() {
        assert_eq!(snippet(&"a".repeat(100)).chars().count(), SNIPPET_LIMIT);
    }

    #[test]
    fn snippet_shows_each_whitespace_run_as_one_space() {
        let body = format!(" \n first\n\n  second\t{}", "word ".repeat(40));

        assert_eq!(
            snippet(&body),
            format!("first second {}", "word ".repeat(40))
                .chars()
                .take(SNIPPET_LIMIT)
                .collect::<String>()
        );
    }

    #[test]
    fn subagent_messages_are_hidden_by_default_and_visible_with_option() {
        let resolved = resolved("session.jsonl");
        let mut subagent = text_message(2, AgentMessageRole::Assistant, "subagent hidden text");
        subagent.parent_tool_use_id = Some("agent-abcdef".to_string());
        let transcript = transcript(vec![
            text_message(1, AgentMessageRole::User, "question"),
            subagent,
            text_message(3, AgentMessageRole::Assistant, "answer"),
        ]);

        let hidden = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: None,
            }],
            None,
            None,
            options(),
        )
        .unwrap();
        let visible = format_read(
            &[ReadRequest {
                resolved: &resolved,
                transcript: &transcript,
                range: None,
            }],
            None,
            None,
            ProtocolOptions {
                subagents: true,
                ..options()
            },
        )
        .unwrap();

        assert!(!hidden.contains("subagent hidden text"));
        assert!(visible.contains("message m2 role=assistant"));
        assert!(visible.contains("subagent hidden text"));
    }

    fn source_at(role: AgentMessageRole, timestamp: &str) -> AgentPartSource {
        AgentPartSource {
            timestamp: Some(timestamp.to_owned()),
            ..source(role)
        }
    }

    fn call(name: &str, tool: Tool, input: Value, source: AgentPartSource) -> AgentMessagePart {
        AgentMessagePart::ToolUse {
            id: format!("toolu_{name}"),
            name: name.to_owned(),
            tool,
            input,
            source,
        }
    }

    fn shell_call(name: &str, command: &str, source: AgentPartSource) -> AgentMessagePart {
        call(name, Tool::Shell, json!({"command": command}), source)
    }

    fn tool_result(content: Option<Value>, source: AgentPartSource) -> AgentMessagePart {
        AgentMessagePart::ToolResult {
            tool_use_id: "toolu_1".to_owned(),
            content,
            source,
        }
    }

    fn thinking(text: &str, source: AgentPartSource) -> AgentMessagePart {
        AgentMessagePart::Thinking {
            thinking: text.to_owned(),
            source,
        }
    }

    fn message(
        ordinal: usize,
        role: AgentMessageRole,
        parts: Vec<AgentMessagePart>,
    ) -> AgentMessage {
        AgentMessage {
            parts,
            ..text_message(ordinal, role, "")
        }
    }

    fn read(
        transcript: &AgentTranscript,
        range: Option<MessageRange>,
        options: ProtocolOptions,
    ) -> String {
        format_read(
            &[ReadRequest {
                resolved: &resolved("session.jsonl"),
                transcript,
                range,
            }],
            None,
            None,
            options,
        )
        .unwrap()
    }

    fn every_flag() -> ProtocolOptions {
        ProtocolOptions {
            tools: true,
            tool_results: true,
            thinking: true,
            subagents: true,
            ..options()
        }
    }

    /// The record lines of `output`, without message bodies.
    fn records(output: &str) -> Vec<&str> {
        output
            .lines()
            .filter(|line| !line.starts_with('|'))
            .collect()
    }

    /// m2..m5 hold calls, results and thinking alone: a read with default
    /// flags shows m1 and m6.
    fn session_with_a_tool_run() -> AgentTranscript {
        use AgentMessageRole::{Assistant, User};
        transcript(vec![
            text_message(1, User, "Check the sheets"),
            message(
                2,
                Assistant,
                vec![shell_call(
                    "PowerShell",
                    "pwsh -File check.ps1",
                    source_at(Assistant, "2026-10-14T15:41:28Z"),
                )],
            ),
            message(
                3,
                User,
                vec![tool_result(
                    Some(json!("3 sheets")),
                    source_at(User, "2026-10-14T15:41:30Z"),
                )],
            ),
            message(
                4,
                Assistant,
                vec![
                    thinking("compare them", source_at(Assistant, "2026-10-14T15:41:40Z")),
                    shell_call(
                        "Bash",
                        "git diff",
                        source_at(Assistant, "2026-10-14T15:41:41Z"),
                    ),
                ],
            ),
            message(
                5,
                User,
                vec![tool_result(
                    Some(json!("no diff")),
                    source_at(User, "2026-10-14T15:42:05Z"),
                )],
            ),
            text_message(6, Assistant, "All three sheets are active."),
        ])
    }

    #[test]
    fn a_run_of_messages_the_flags_hide_whole_shows_as_one_hidden_record() {
        let output = read(&session_with_a_tool_run(), None, options());

        let records = records(&output);
        assert_eq!(
            records[0],
            "protocol agent-read cut=none chars=6000 policy=text omit=none hidden-tools=2 hidden-tool-results=2 hidden-thinking=1"
        );
        assert!(records[2].starts_with("message m1 "), "{output}");
        assert_eq!(
            records[3],
            format!(
                "hidden m2..m5 tools=2 tool-results=2 thinking=1 at={} to={}",
                local_time("2026-10-14T15:41:28Z"),
                local_time("2026-10-14T15:42:05Z")
            )
        );
        assert!(records[4].starts_with("message m6 "), "{output}");
        assert_eq!(records.len(), 5, "{output}");
    }

    #[test]
    fn a_read_with_every_flag_doesnt_report_a_hidden_part() {
        let output = read(&session_with_a_tool_run(), None, every_flag());

        assert!(!output.contains(" hidden-"), "{output}");
        assert!(
            !output.lines().any(|line| line.starts_with("hidden ")),
            "{output}"
        );
        for ordinal in 1..=6 {
            assert!(output.contains(&format!("message m{ordinal} ")), "{output}");
        }
    }

    #[test]
    fn a_shown_message_names_its_hidden_calls_and_counts_its_other_hidden_parts() {
        use AgentMessageRole::Assistant;
        let mut mixed = text_message(1, Assistant, "I'll write the script and run it.");
        mixed.parts.extend([
            call(
                "Write",
                Tool::Write,
                json!({"file_path": "check.ps1", "content": "Get-Date"}),
                source(Assistant),
            ),
            thinking("it needs a header", source(Assistant)),
            shell_call("PowerShell", "pwsh -File check.ps1", source(Assistant)),
            tool_result(Some(json!("ok")), source(Assistant)),
        ]);
        let transcript = transcript(vec![mixed]);
        let hidden_atoms = |options| -> Vec<String> {
            read(&transcript, None, options)
                .lines()
                .find(|line| line.starts_with("message m1 "))
                .unwrap()
                .split_whitespace()
                .filter(|atom| atom.starts_with("hidden-"))
                .map(str::to_owned)
                .collect()
        };

        assert_eq!(
            hidden_atoms(options()),
            [
                "hidden-tools=Write,PowerShell",
                "hidden-tool-results=1",
                "hidden-thinking=1"
            ]
        );
        assert_eq!(
            hidden_atoms(ProtocolOptions {
                tools: true,
                ..options()
            }),
            ["hidden-tool-results=1", "hidden-thinking=1"]
        );
        assert!(hidden_atoms(every_flag()).is_empty());
    }

    #[test]
    fn a_part_without_text_doesnt_count_as_hidden() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            text_message(1, User, "question"),
            message(
                2,
                Assistant,
                vec![
                    thinking("", source(Assistant)),
                    tool_result(None, source(Assistant)),
                    tool_result(Some(json!("  \n")), source(Assistant)),
                ],
            ),
            text_message(3, Assistant, "answer"),
        ]);

        let output = read(&transcript, None, options());

        assert!(!output.contains("hidden"), "{output}");
    }

    #[test]
    fn a_tool_result_has_text_when_its_read_text_is_not_blank() {
        for content in [
            json!("ok"),
            json!(" \n"),
            json!(["", {"text": " "}]),
            json!([{"type": "image"}, {"text": "done"}]),
            json!({"exit": 0}),
            json!(null),
        ] {
            assert_eq!(
                tool_result_has_text(&content),
                !tool_result_text(&content).trim().is_empty(),
                "{content}"
            );
        }
    }

    #[test]
    fn a_read_and_an_outline_with_thinking_dont_show_a_thinking_part_without_text() {
        use AgentMessageRole::Assistant;
        let mut reply = text_message(1, Assistant, "answer");
        reply.parts.extend([
            thinking("", source(Assistant)),
            thinking(" \n", source(Assistant)),
            thinking("weigh it", source(Assistant)),
        ]);
        let transcript = transcript(vec![reply]);
        let with_thinking = ProtocolOptions {
            thinking: true,
            ..options()
        };

        let read = read(&transcript, None, with_thinking);
        let outline = format_outline(&resolved("session.jsonl"), &transcript, with_thinking);

        assert!(read.contains("| answer\n| thinking: weigh it\n"), "{read}");
        assert_eq!(read.matches("thinking:").count(), 1, "{read}");
        assert!(
            outline.contains(" | answer thinking: weigh it\n"),
            "{outline}"
        );
    }

    #[test]
    fn a_hidden_record_past_the_budget_is_omitted_and_continued() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            text_message(1, User, "question"),
            text_message(2, Assistant, &"long answer ".repeat(50)),
            message(
                3,
                Assistant,
                vec![shell_call("Bash", "cargo test", source(Assistant))],
            ),
            message(4, User, vec![tool_result(Some(json!("ok")), source(User))]),
        ]);
        let budget = 400;

        let output = read(
            &transcript,
            None,
            ProtocolOptions {
                budget: Some(budget),
                ..options()
            },
        );

        assert!(output.chars().count() <= budget, "{output}");
        assert!(
            output.starts_with("protocol agent-read cut=tail chars=400 policy=text omit=m2..m4\n"),
            "{output}"
        );
        assert!(output.contains(":m2..m4\n"), "{output}");
        assert!(output.contains("message m1 "), "{output}");
        assert!(!output.contains("hidden"), "{output}");
    }

    /// m1 hides nine calls, nine results and nine thinking parts. The header's
    /// totals, not m1's record, decide whether m2 fits.
    #[test]
    fn a_message_the_header_totals_push_past_the_budget_is_omitted_whole() {
        use AgentMessageRole::Assistant;
        let mut reply = text_message(1, Assistant, "short answer");
        for _ in 0..9 {
            reply.parts.extend([
                shell_call("Bash", "cargo test", source(Assistant)),
                tool_result(Some(json!("ok")), source(Assistant)),
                thinking("weigh it", source(Assistant)),
            ]);
        }
        let transcript = transcript(vec![reply, text_message(2, Assistant, &"x".repeat(300))]);
        let unbudgeted = read(
            &transcript,
            None,
            ProtocolOptions {
                budget: None,
                ..options()
            },
        );
        let budget = unbudgeted.chars().count() - 40;

        let output = read(
            &transcript,
            None,
            ProtocolOptions {
                budget: Some(budget),
                ..options()
            },
        );

        assert!(
            unbudgeted.contains(" hidden-tools=9 hidden-tool-results=9 hidden-thinking=9\n"),
            "{unbudgeted}"
        );
        assert!(output.chars().count() <= budget, "{output}");
        assert!(
            output.starts_with("protocol agent-read cut=tail "),
            "{output}"
        );
        assert!(output.contains(" omit=m2 "), "{output}");
        assert!(output.contains("| short answer\n"), "{output}");
        assert!(!output.contains("omitted chars="), "{output}");
    }

    /// From 15:40 to 15:45 UTC on 2026-10-14, both included.
    fn window_from_15_40_to_15_45() -> TimeFilter {
        let at = |utc: &str| {
            chrono::DateTime::parse_from_rfc3339(utc)
                .unwrap()
                .with_timezone(&chrono::Local)
        };
        TimeFilter {
            after: Some(at("2026-10-14T15:40:00Z")),
            before: Some(at("2026-10-14T15:45:00Z")),
        }
    }

    /// The header's `policy=` and `messages-in-window=`, not the messages'
    /// records, decide whether m2 fits.
    #[test]
    fn a_message_the_window_count_pushes_past_the_budget_is_omitted_whole() {
        use AgentMessageRole::Assistant;
        let at = "2026-10-14T15:42:00Z";
        let transcript = transcript(vec![
            text_message_at(1, Assistant, "short answer", at),
            text_message_at(2, Assistant, &"x".repeat(300), at),
        ]);
        let windowed = |budget| ProtocolOptions {
            budget,
            window: window_from_15_40_to_15_45(),
            ..options()
        };
        let unbudgeted = read(&transcript, None, windowed(None));
        let budget = unbudgeted.chars().count() - 10;

        let output = read(&transcript, None, windowed(Some(budget)));

        assert!(
            unbudgeted.starts_with(
                "protocol agent-read cut=none chars=none policy=text omit=none messages-in-window=2\n"
            ),
            "{unbudgeted}"
        );
        assert!(output.chars().count() <= budget, "{output}");
        assert!(output.contains(" omit=m2 "), "{output}");
        assert!(output.contains("| short answer\n"), "{output}");
        assert!(!output.contains("omitted chars="), "{output}");
    }

    /// m3 has no time and m6 falls after the window: both are outside it.
    #[test]
    fn a_windowed_read_reports_only_the_hidden_parts_inside_the_window() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            text_message_at(1, User, "Run the tests", "2026-10-14T15:41:00Z"),
            message(
                2,
                Assistant,
                vec![shell_call(
                    "Bash",
                    "cargo test",
                    source_at(Assistant, "2026-10-14T15:41:10Z"),
                )],
            ),
            message(3, User, vec![tool_result(Some(json!("ok")), source(User))]),
            message(
                4,
                Assistant,
                vec![shell_call(
                    "Bash",
                    "cargo build",
                    source_at(Assistant, "2026-10-14T15:41:20Z"),
                )],
            ),
            text_message_at(5, Assistant, "Both pass", "2026-10-14T15:41:30Z"),
            message(
                6,
                Assistant,
                vec![shell_call(
                    "Bash",
                    "git push",
                    source_at(Assistant, "2026-10-14T15:50:00Z"),
                )],
            ),
        ]);

        let output = read(
            &transcript,
            None,
            ProtocolOptions {
                window: window_from_15_40_to_15_45(),
                ..options()
            },
        );

        let records = records(&output);
        assert!(
            records[0].ends_with(" omit=none messages-in-window=4 hidden-tools=2"),
            "{output}"
        );
        assert!(records[2].starts_with("message m1 "), "{output}");
        assert!(records[3].starts_with("hidden m2 tools=1 "), "{output}");
        assert!(records[4].starts_with("hidden m4 tools=1 "), "{output}");
        assert!(records[5].starts_with("message m5 "), "{output}");
        assert_eq!(records.len(), 6, "{output}");
    }

    #[test]
    fn a_focus_inside_a_hidden_run_keeps_the_run() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            text_message(1, User, &"long question ".repeat(50)),
            message(
                2,
                Assistant,
                vec![shell_call("Bash", "cargo test", source(Assistant))],
            ),
            message(3, User, vec![tool_result(Some(json!("ok")), source(User))]),
            text_message(4, Assistant, &"long answer ".repeat(50)),
        ]);

        let output = format_read(
            &[ReadRequest {
                resolved: &resolved("session.jsonl"),
                transcript: &transcript,
                range: None,
            }],
            Some(ProtocolFocus {
                conversation_full_ref: None,
                range: MessageRange::single(3),
            }),
            None,
            ProtocolOptions {
                budget: Some(400),
                ..options()
            },
        )
        .unwrap();

        assert!(
            output.starts_with(
                "protocol agent-read cut=head+focus+tail chars=400 policy=text omit=m1,m4 hidden-tools=1 hidden-tool-results=1\n"
            ),
            "{output}"
        );
        assert!(
            output.contains("\nhidden m2..m3 tools=1 tool-results=1\n"),
            "{output}"
        );
    }

    #[test]
    fn a_hidden_subagent_message_counts_once_and_its_calls_stay_out_of_the_tools_count() {
        use AgentMessageRole::{Assistant, User};
        let mut subagent = text_message(2, Assistant, "The sub-agent checks the cache.");
        subagent.parent_tool_use_id = Some("agent-abcdef".to_owned());
        subagent
            .parts
            .push(shell_call("Grep", "rg stale_cache", source(Assistant)));
        let transcript = transcript(vec![
            text_message(1, User, "question"),
            subagent,
            text_message(3, Assistant, "answer"),
        ]);

        let default = read(&transcript, None, options());
        let with_subagents = read(
            &transcript,
            None,
            ProtocolOptions {
                subagents: true,
                ..options()
            },
        );

        assert!(
            default.starts_with(
                "protocol agent-read cut=none chars=6000 policy=text omit=none hidden-subagents=1\n"
            ),
            "{default}"
        );
        assert!(default.contains("\nhidden m2 subagents=1\n"), "{default}");
        assert!(
            with_subagents.starts_with(
                "protocol agent-read cut=none chars=6000 policy=text+subagents omit=none hidden-tools=1\n"
            ),
            "{with_subagents}"
        );
    }

    #[test]
    fn a_one_message_read_shows_every_line_of_each_command() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            message(
                1,
                Assistant,
                vec![
                    shell_call("Bash", "cd src\ncargo test", source(Assistant)),
                    shell_call("Bash", "cargo clippy", source(Assistant)),
                ],
            ),
            text_message(2, User, "thanks"),
        ]);
        let with_tools = ProtocolOptions {
            tools: true,
            ..options()
        };

        let one = read(&transcript, Some(MessageRange::single(1)), with_tools);
        let several = read(
            &transcript,
            Some(MessageRange { start: 1, end: 2 }),
            with_tools,
        );

        assert!(
            one.contains("| Bash: cd src\n|   cargo test\n| Bash: cargo clippy\n"),
            "{one}"
        );
        assert!(several.contains("| Bash: cd src (+1 line)\n"), "{several}");
    }

    #[test]
    fn short_outline_emits_one_line_per_message() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(vec![
            text_message(1, AgentMessageRole::User, "one"),
            text_message(2, AgentMessageRole::Assistant, "two"),
        ]);
        let output = format_outline(&resolved, &transcript, options());

        assert!(output.contains("m1 role=user chars=3 anchor=ma_"));
        assert!(output.contains(" | one"));
        assert!(output.contains("m2 role=assistant chars=3 anchor=ma_"));
        assert!(output.contains(" | two"));
        assert!(!output.contains("seg "));
    }

    #[test]
    fn long_outline_emits_deterministic_segments() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(
            (1..=21)
                .map(|index| {
                    text_message(index, AgentMessageRole::User, &format!("message {index}"))
                })
                .collect(),
        );
        let output = format_outline(&resolved, &transcript, options());

        assert!(output.contains("seg m1..m10 chars=91 anchors=ma_"));
        assert!(output.contains(" | message 1 / message 10"));
        assert!(output.contains("seg m11..m20 chars=100 anchors=ma_"));
        assert!(output.contains(" | message 11 / message 20"));
        assert!(output.contains("seg m21..m21 chars=10 anchors=ma_"));
        assert!(output.contains(" | message 21 / message 21"));
        assert!(!output.contains("summary"));
    }

    fn segment<'a>(output: &'a str, range: &str) -> &'a str {
        output
            .lines()
            .find(|line| line.starts_with(&format!("seg {range} ")))
            .unwrap_or_else(|| panic!("no segment {range}: {output}"))
    }

    /// The fifth message is the earliest and the seventh the latest; neither
    /// is at an end of the segment.
    #[test]
    fn an_outline_segment_shows_the_first_and_last_time_of_its_messages() {
        let resolved = resolved("session.jsonl");
        let minutes = [35, 33, 34, 36, 31, 37, 40, 38, 39, 38];
        let timed = minutes.iter().enumerate().map(|(index, minute)| {
            text_message_at(
                index + 1,
                AgentMessageRole::User,
                &format!("message {}", index + 1),
                &format!("2026-10-14T15:{minute}:00Z"),
            )
        });
        let untimed = (11..=21).map(|ordinal| {
            text_message(
                ordinal,
                AgentMessageRole::User,
                &format!("message {ordinal}"),
            )
        });
        let transcript = transcript(timed.chain(untimed).collect());

        let output = format_outline(&resolved, &transcript, options());

        assert!(
            segment(&output, "m1..m10").contains(&format!(
                " at={} to={} | ",
                local_time("2026-10-14T15:31:00Z"),
                local_time("2026-10-14T15:40:00Z")
            )),
            "{output}"
        );
        assert!(!segment(&output, "m11..m20").contains(" at="), "{output}");
    }

    /// `count` messages written a minute apart from 2026-10-14T00:00Z, each
    /// long enough to fill a snippet.
    fn long_session(count: usize) -> AgentTranscript {
        let start = chrono::DateTime::parse_from_rfc3339("2026-10-14T00:00:00Z").unwrap();
        transcript(
            (1..=count)
                .map(|ordinal| {
                    let role = if ordinal % 2 == 1 {
                        AgentMessageRole::User
                    } else {
                        AgentMessageRole::Assistant
                    };
                    let written = start + chrono::Duration::minutes(ordinal as i64 - 1);
                    text_message_at(
                        ordinal,
                        role,
                        &format!("message {ordinal} {}", "padding ".repeat(12)),
                        &written.to_rfc3339(),
                    )
                })
                .collect(),
        )
    }

    fn with_budget(budget: usize) -> ProtocolOptions {
        ProtocolOptions {
            budget: Some(budget),
            ..options()
        }
    }

    #[test]
    fn a_long_outline_spans_the_session_at_each_budget() {
        let resolved = resolved("session.jsonl");
        let transcript = long_session(2_000);
        let session = format!(
            " messages=2000 at={} to={}",
            local_time("2026-10-14T00:00:00Z"),
            local_time("2026-10-15T09:19:00Z")
        );

        for budget in [1_500, 3_000, 6_000, 20_000] {
            let output = format_outline(&resolved, &transcript, with_budget(budget));

            let header = output.lines().next().unwrap();
            assert!(
                header.starts_with("protocol agent-outline cut=none "),
                "{budget}: {header}"
            );
            assert!(header.contains(&session), "{budget}: {header}");
            let last = output.lines().last().unwrap();
            assert!(
                last.starts_with("seg m") && last.contains("..m2000 "),
                "{budget}: {output}"
            );
            assert!(output.chars().count() <= budget, "{budget}: {output}");
        }
    }

    #[test]
    fn a_long_outline_uses_the_smallest_segment_size_that_fits() {
        let resolved = resolved("session.jsonl");
        let transcript = long_session(2_000);
        let budget = 6_000;
        let output = format_outline(&resolved, &transcript, with_budget(budget));
        let messages = OutlineMessages::new(&resolved, &transcript, with_budget(budget));
        let record_chars = |size| {
            messages
                .records(size)
                .iter()
                .map(|record| record.line.chars().count())
                .sum::<usize>()
        };
        let size = output
            .lines()
            .find_map(|line| line.strip_prefix("seg m1..m"))
            .and_then(|rest| rest.split_once(' '))
            .map(|(last, _)| last.parse::<usize>().unwrap())
            .unwrap_or_else(|| panic!("no first segment: {output}"));
        let unsegmented = output.chars().count() - record_chars(size);

        assert!(size > OUTLINE_MIN_SEGMENT_SIZE, "{output}");
        for smaller in OUTLINE_MIN_SEGMENT_SIZE..size {
            assert!(
                unsegmented + record_chars(smaller) > budget,
                "segments of {smaller} fit: {output}"
            );
        }
    }

    #[test]
    fn a_measured_outline_record_is_as_long_as_the_record_shown() {
        let resolved = resolved("session.jsonl");
        let line_lengths = |records: Vec<OutlineRecord>| {
            records
                .iter()
                .map(|record| record.line.chars().count())
                .collect::<Vec<_>>()
        };

        for transcript in [long_session(15), long_session(200)] {
            let messages = OutlineMessages::new(&resolved, &transcript, options());

            assert_eq!(
                line_lengths(messages.measured_records(OUTLINE_MIN_SEGMENT_SIZE)),
                line_lengths(messages.records(OUTLINE_MIN_SEGMENT_SIZE))
            );
        }
    }

    #[test]
    fn a_budget_too_small_for_one_segment_continues_with_the_whole_session() {
        let resolved = resolved("session.jsonl");
        let transcript = long_session(2_000);

        let output = format_outline(&resolved, &transcript, with_budget(400));

        let header = output.lines().next().unwrap();
        assert!(
            header.starts_with("protocol agent-outline cut=tail "),
            "{output}"
        );
        assert!(header.contains(" messages=2000 at="), "{output}");
        assert!(header.contains(" omitted-records=1 "), "{output}");
        assert!(!output.contains("\nseg "), "{output}");
        assert!(
            output.ends_with(&format!(
                "continue read ref={}:m1..m2000\n",
                resolved.reference.canonical()
            )),
            "{output}"
        );
        assert!(output.chars().count() <= 400, "{output}");
    }

    fn skipped_warning() -> AgentWarning {
        AgentWarning::skipped(Some("ch_000000000000"), "padding ".repeat(60))
    }

    #[test]
    fn a_warning_that_overflows_the_budget_is_dropped_before_the_segment() {
        let resolved = resolved("session.jsonl");
        let transcript = long_session(2_000);
        let one_segment = OutlineMessages::new(&resolved, &transcript, options())
            .records(2_000)
            .remove(0)
            .line;
        let budget = one_segment.chars().count() + 400;

        let output = format_outline_with_warnings(
            &resolved,
            &transcript,
            with_budget(budget),
            &[skipped_warning()],
        );

        let header = output.lines().next().unwrap();
        assert!(
            header.starts_with("protocol agent-outline cut=tail "),
            "{output}"
        );
        assert!(
            header.ends_with(" omitted-records=0 warnings=1 warnings-emitted=0"),
            "{output}"
        );
        assert!(one_segment.starts_with("seg m1..m2000 "), "{one_segment}");
        assert!(output.contains(&one_segment), "{output}");
        assert!(!output.contains("agent-warning"), "{output}");
    }

    #[test]
    fn a_roomy_outline_shows_its_warning() {
        let resolved = resolved("session.jsonl");
        let transcript = long_session(2_000);

        let output = format_outline_with_warnings(
            &resolved,
            &transcript,
            with_budget(20_000),
            &[skipped_warning()],
        );

        let header = output.lines().next().unwrap();
        assert!(
            header.starts_with("protocol agent-outline cut=none "),
            "{output}"
        );
        assert!(
            header.ends_with(" warnings=1 warning-records=1"),
            "{output}"
        );
        assert!(
            output.contains("\nprotocol agent-warning kind=skipped "),
            "{output}"
        );
    }
}
