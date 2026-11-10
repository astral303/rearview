use crate::agent::call_links::{AnsweredCall, Call, CallAddress, CallLinks, numbered_calls};
use crate::agent::diagnostic::{AgentError, AgentErrorKind};
use crate::agent::diagnostic::{
    AgentWarning, budget_beside_warnings, format_warning_records, warning_atoms,
};
use crate::agent::hidden_parts::HiddenParts;
use crate::agent::record_writer::{
    CallCopy, CallRecord, CallResultRecord, HiddenRecord, MessageRecord, MessageRuns,
    OmittedRecord, RecordSize, RecordWriter, continue_read_record, conversation_record,
    handles_field, message_range_ref, message_record_start, ordinal_range, read_record,
};
use crate::agent::refs::{MessageRange, ResolvedConversation};
use crate::agent::sanitize::sanitize_agent_text;
use crate::agent::subagents::{Handle, SubagentTag, Subagents};
use crate::agent::time_span::{
    Day, DayRecords, RangeSpans, RecordTime, TimeSpan, output_offset, time_atoms, tz_atom,
};
use crate::agent::transcript::{
    AgentMessage, AgentMessagePart, AgentMessageRole, AgentTranscript, CommandLines,
    tool_call_read_text,
};
use crate::agent::visibility::ContentVisibility;
use crate::error::Result;
use crate::log_entry::Tool;
use crate::time_filter::TimeFilter;
use chrono::{DateTime, FixedOffset, Utc};
use serde_json::Value;
use std::cell::OnceCell;
use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::num::NonZeroUsize;
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

    /// The options, also showing what `visibility` shows.
    fn showing(mut self, visibility: ContentVisibility) -> Self {
        self.tools |= visibility.tools;
        self.tool_results |= visibility.tool_results;
        self.thinking |= visibility.thinking;
        self.subagents |= visibility.subagents;
        self
    }
}

#[derive(Clone, Debug)]
pub struct ReadRequest<'a> {
    pub resolved: &'a ResolvedConversation,
    pub transcript: &'a AgentTranscript,
    pub range: Option<MessageRange>,
    /// A sub-agent read alone: its launch, its messages and its reports, in
    /// place of `range`.
    pub subagent: Option<Handle>,
    /// The number of the call read alone, in the message `range` names,
    /// with the messages holding its result.
    pub call: Option<usize>,
}

impl ReadRequest<'_> {
    /// The read's `options`, with the flags the request implies: a
    /// sub-agent read alone shows its messages, a read of one message
    /// (`REF:mN`, `--anchor`) or one call (`REF:mN.K`) shows it whoever
    /// wrote it, and a read of one call shows its call and results.
    fn options(&self, mut options: ProtocolOptions) -> ProtocolOptions {
        options.subagents |= self.subagent.is_some() || self.names_a_subagents_message();
        if self.call.is_some() {
            options.tools = true;
            options.tool_results = true;
        }
        options
    }

    /// True when the request names one message, or a call in it, and a
    /// sub-agent wrote that message.
    fn names_a_subagents_message(&self) -> bool {
        let messages = &self.transcript.messages;
        self.range
            .filter(|range| range.is_single())
            .and_then(|range| {
                let index = messages
                    .binary_search_by_key(&range.start, |message| message.ordinal)
                    .ok()?;
                Some(&messages[index])
            })
            .is_some_and(|message| message.parent_tool_use_id.is_some())
    }

    /// Every line of each command for a call read alone, or in a read of
    /// one message (`one_message`); each command's first line otherwise.
    fn command_lines(&self, one_message: bool) -> CommandLines {
        if one_message || self.call.is_some() {
            CommandLines::All
        } else {
            CommandLines::FirstLine
        }
    }

    /// The number of the call read alone when `message` holds it.
    fn call_in(&self, message: &AgentMessage) -> Option<usize> {
        self.call.filter(|_| {
            self.range
                .is_some_and(|range| range.start == message.ordinal)
        })
    }
}

#[derive(Clone, Debug)]
pub struct ProtocolFocus {
    pub conversation_full_ref: Option<String>,
    pub range: MessageRange,
}

/// The messages a read keeps first when its budget cannot hold every message.
#[derive(Clone, Debug)]
pub enum ReadPriority {
    Start,
    Focus(ProtocolFocus),
    /// Each request's last N shown messages, and each request's end first.
    Tail(NonZeroUsize),
}

#[derive(Clone, Debug)]
struct RenderedMessage<'a> {
    conversation: &'a ResolvedConversation,
    message: &'a AgentMessage,
    times: Option<TimeSpan>,
    content: MessageContent<'a>,
    /// The content a budget truncated, shown in place of `content`.
    truncation: Option<Truncation>,
    slice: Option<String>,
    /// `anchor=` on the record of a one-message read.
    anchor: Option<String>,
    /// The calls in other messages that its shown results answer: `call=`.
    answered: Vec<AnsweredCall<'a>>,
    /// True when the message prints a record of its own. A message with a
    /// body always does.
    has_own_record: bool,
    /// The read's flags, repeated in the read an omission marker names.
    visibility: ContentVisibility,
    /// The sub-agent the message belongs to, in position 2.
    subagent: Option<SubagentTag>,
    /// The sub-agent whose report or finish notice it carries: `from=`.
    from: Option<Handle>,
}

impl<'a> RenderedMessage<'a> {
    fn source_word(&self) -> &'static str {
        source_word(self.message, !self.answered.is_empty())
    }

    /// Name the sub-agents of `subagents` the message and its calls belong
    /// to, launch, send to and report from.
    fn name_subagents(&mut self, subagents: &Subagents) {
        let ordinal = self.message.ordinal;
        self.subagent = subagents.tag_of(ordinal);
        self.from = subagents.reporter_of(ordinal);
        for call in &mut self.content.calls {
            call.launches = subagents.launched_by(call.address);
            call.target = subagents.targeted_by(call.address);
        }
    }

    /// Show the message's call `number` alone, with its result when the
    /// message holds it, for a read of that call. Their lines keep the
    /// message's numbers, so an omission names the lines `--lines` reads.
    fn keep_call_alone(&mut self, number: usize) {
        let content = &self.content;
        debug_assert!(
            matches!(content.numbering, LineNumbering::Own),
            "a call read alone shows every line of each command, without a slice"
        );
        let is_kept = |owner| match owner {
            BlockOwner::Call(call) => content.calls[call].address.number == number,
            BlockOwner::Result(result) => content.results[result].call.address.number == number,
            BlockOwner::Message => false,
        };
        let line_numbers = content
            .blocks
            .iter()
            .filter(|block| is_kept(block.owner))
            .flat_map(|block| block.first_line + 1..=block.first_line + block.lines)
            .map(Some)
            .collect();
        let calls = content
            .calls
            .iter()
            .filter(|call| call.address.number == number)
            .cloned()
            .collect();
        let results = content
            .results
            .iter()
            .filter(|result| result.call.address.number == number)
            .cloned()
            .collect();
        self.content = MessageContent::new(
            String::new(),
            calls,
            results,
            LineNumbering::Part(line_numbers),
        );
        self.answered.clear();
        self.has_own_record = false;
    }

    /// Give a message showing calls alone a record of its own, without
    /// content lines, when the record carries an atom: a source word, a
    /// hidden part, or the anchor of a one-message read.
    fn keep_own_record_with_atoms(&mut self, hidden: &HiddenParts<'_>) {
        self.has_own_record |=
            !self.source_word().is_empty() || hidden.has_message_atoms() || self.anchor.is_some();
    }

    /// Each timed record the message prints, its content truncated to
    /// `truncation`: its own, then each printed call's.
    fn printed_record_times(
        &self,
        truncation: Option<&Truncation>,
    ) -> impl Iterator<Item = TimeSpan> + '_ {
        let content = &self.content;
        let own = self.has_own_record.then_some(self.times);
        let calls_and_results =
            content
                .printed_blocks(kept_chars(truncation))
                .filter_map(|block| match block.owner {
                    BlockOwner::Message => None,
                    BlockOwner::Call(call) => Some(content.calls[call].time),
                    BlockOwner::Result(result) => Some(content.results[result].time),
                });
        own.into_iter().chain(calls_and_results).flatten()
    }

    /// The days of the message's first and last timed records, its content
    /// truncated to `truncation`.
    fn days(&self, truncation: Option<&Truncation>) -> EntryDays {
        let mut days = self
            .printed_record_times(truncation)
            .map(|span| RecordTime::of(span).day());
        let first = days.next();
        (first, days.last().or(first))
    }

    /// The message's records and content lines, the content truncated to
    /// `truncation`.
    fn write<'e>(
        &'e self,
        hidden: &HiddenParts<'_>,
        truncation: Option<&Truncation>,
        writer: &mut RecordWriter<'e>,
    ) {
        let content = &self.content;
        let kept_chars = kept_chars(truncation);
        let lines = truncation.map_or_else(
            || content.text.split('\n').collect(),
            |truncation| truncation.lines(),
        );
        if self.has_own_record && !content.has_body_block() {
            writer.write_message_record(&self.record(hidden), &[], None);
        }
        for block in content.printed_blocks(kept_chars) {
            let repeatable = block.first_char + block.chars <= kept_chars && self.slice.is_none();
            let end = (block.first_line + block.lines).min(lines.len());
            let block_lines = &lines[block.first_line.min(end)..end];
            match block.owner {
                BlockOwner::Message => writer.write_message_record(
                    &self.record(hidden),
                    block_lines,
                    repeatable.then_some(content.body.as_str()),
                ),
                BlockOwner::Call(call) => {
                    let call = &content.calls[call];
                    let copy = if repeatable {
                        call.header_copy
                    } else {
                        CallCopy::Truncated
                    };
                    writer.write_call_record(
                        &call.record(self.subagent.as_ref()),
                        block_lines,
                        copy,
                    );
                }
                BlockOwner::Result(result) => writer.write_call_result_record(
                    &content.results[result].record(self.subagent.as_ref()),
                    block_lines,
                ),
            }
        }
        if let Some(omission) = truncation.and_then(|truncation| truncation.omission.as_ref()) {
            let reference = self.conversation.reference.canonical();
            writer.write_omitted_records(&self.omitted_record(&reference, omission));
        }
    }

    fn record(&self, hidden: &HiddenParts<'_>) -> MessageRecord<'_> {
        let slice = self
            .slice
            .as_ref()
            .map(|slice| format!(" slice={slice}"))
            .unwrap_or_default();
        let anchor = self
            .anchor
            .as_ref()
            .map(|anchor| format!(" anchor={anchor}"))
            .unwrap_or_default();
        MessageRecord {
            ordinal: self.message.ordinal,
            subagent: self.subagent.as_ref(),
            source_word: self.source_word(),
            time: self.times,
            from: self.from,
            answered: &self.answered,
            trailing_atoms: format!("{}{slice}{anchor}", hidden.message_atoms()),
        }
    }

    /// The characters of the message's records and content alone, the
    /// content truncated to `truncation`.
    fn measured_chars(&self, hidden: &HiddenParts<'_>, truncation: Option<&Truncation>) -> usize {
        let mut writer = RecordWriter::measuring(self.printed_record_times(truncation).next());
        self.write(hidden, truncation, &mut writer);
        writer.into_output().chars().count()
    }

    /// The truncation that keeps the most of the content within `room`, as
    /// `chars` counts the output of each; `None` when the whole content fits.
    /// When no truncation fits, the omission marker alone: a read never drops
    /// content without naming the read that shows it. `chars` counts at
    /// least the characters a truncation keeps.
    fn truncated_to_fit(
        &self,
        room: usize,
        chars: impl Fn(Option<&Truncation>) -> usize,
    ) -> Option<Truncation> {
        let total_chars = self.content.text.chars().count();
        if total_chars == 0 || chars(None) <= room {
            return None;
        }
        let fits = |truncation: &Truncation| chars(Some(truncation)) <= room;
        // The output grows with the characters kept while the same blocks
        // print, but can shrink when a block starts to print: its call
        // leaves `calls=`. Search the truncations printing each set of
        // blocks, the most blocks first.
        let mut last_keep = room.min(total_chars - 1);
        for first_keep in self.content.block_print_starts().rev() {
            if first_keep > last_keep {
                continue;
            }
            let shortest = self.truncation_keeping(first_keep);
            if fits(&shortest) {
                return Some(self.longest_fitting(shortest, last_keep, fits));
            }
            last_keep = first_keep.saturating_sub(1);
        }
        Some(self.truncation_keeping(0))
    }

    /// The truncation keeping the most characters, from `shortest`'s to
    /// `last_keep`, that `fits`; `shortest` fits, and each truncation prints
    /// the same blocks.
    fn longest_fitting(
        &self,
        shortest: Truncation,
        last_keep: usize,
        fits: impl Fn(&Truncation) -> bool,
    ) -> Truncation {
        let mut best = shortest;
        let (mut low, mut high) = (best.kept_chars + 1, last_keep);
        while low <= high {
            let keep = low + (high - low) / 2;
            let candidate = self.truncation_keeping(keep);
            if fits(&candidate) {
                best = candidate;
                low = keep + 1;
            } else {
                high = keep - 1;
            }
        }
        best
    }

    /// The content truncated to its [`TRUNCATION_FLOOR_CHARS`]; `None` for
    /// content no longer than that.
    fn floor_truncation(&self) -> Option<Truncation> {
        (self.content.text.chars().count() > TRUNCATION_FLOOR_CHARS)
            .then(|| self.truncation_keeping(TRUNCATION_FLOOR_CHARS))
    }

    /// The content's first `keep` characters, fewer than it holds, and what
    /// they leave out.
    fn truncation_keeping(&self, keep: usize) -> Truncation {
        Truncation::keeping(&self.content.text, keep, self.content.omission_after(keep))
    }

    /// The fields of the records naming what `omission` leaves out of the
    /// message's content.
    fn omitted_record<'r>(
        &'r self,
        reference: &'r str,
        omission: &'r Omission,
    ) -> OmittedRecord<'r> {
        OmittedRecord {
            ordinal: self.message.ordinal,
            subagent: self.subagent.as_ref(),
            reference,
            first_line: omission.first_line,
            last_line: omission.last_line,
            dropped_calls: &omission.dropped_calls,
            visibility: self.visibility,
        }
    }
}

/// The word after a message's address naming what produced it, with its
/// leading space: empty for the agent's own messages and for a tool result
/// linking to its call, ` report` for a hand-back, ` result` for a tool
/// result without that link, and ` user` for any other message that arrived
/// as the user.
fn source_word(message: &AgentMessage, links_to_its_call: bool) -> &'static str {
    match message.role {
        AgentMessageRole::Assistant => "",
        AgentMessageRole::User if message.is_report => " report",
        AgentMessageRole::User if is_tool_result_message(message) => {
            if links_to_its_call {
                ""
            } else {
                " result"
            }
        }
        AgentMessageRole::User => " user",
    }
}

fn is_tool_result_message(message: &AgentMessage) -> bool {
    !message.parts.is_empty()
        && message
            .parts
            .iter()
            .all(|part| matches!(part, AgentMessagePart::ToolResult { .. }))
}

/// The body, the shown calls and the results in their calls' own message of
/// one message, in the order a read prints their records. A budget truncates
/// them as one text, the message's content, and `--lines` numbers its lines.
#[derive(Clone, Debug)]
struct MessageContent<'a> {
    /// The text, thinking and results of calls in other messages, joined by
    /// line breaks.
    body: String,
    calls: Vec<ShownCall<'a>>,
    results: Vec<ShownResult<'a>>,
    /// The blocks joined by line breaks.
    text: String,
    /// The body's block, then each call header's, each followed by the
    /// blocks of its results. A message showing calls or results alone has
    /// no body block.
    blocks: Vec<ContentBlock>,
    numbering: LineNumbering,
}

impl<'a> MessageContent<'a> {
    fn new(
        body: String,
        calls: Vec<ShownCall<'a>>,
        results: Vec<ShownResult<'a>>,
        numbering: LineNumbering,
    ) -> Self {
        let mut block_texts = Vec::new();
        if !body.is_empty() || (calls.is_empty() && results.is_empty()) {
            block_texts.push((BlockOwner::Message, body.clone()));
        }
        let mut calls_then_their_results = calls
            .iter()
            .enumerate()
            .map(|(index, call)| (call.address, BlockOwner::Call(index), call.header.clone()))
            .chain(results.iter().enumerate().map(|(index, result)| {
                (
                    result.call.address,
                    BlockOwner::Result(index),
                    result.text.clone(),
                )
            }))
            .collect::<Vec<_>>();
        calls_then_their_results
            .sort_by_key(|&(call, owner, _)| (call.number, matches!(owner, BlockOwner::Result(_))));
        block_texts.extend(
            calls_then_their_results
                .into_iter()
                .map(|(_, owner, text)| (owner, text)),
        );
        Self::of_blocks(body, calls, results, block_texts, numbering)
    }

    /// The content of `block_texts`, each block's text below the record its
    /// owner names.
    fn of_blocks(
        body: String,
        calls: Vec<ShownCall<'a>>,
        results: Vec<ShownResult<'a>>,
        block_texts: Vec<(BlockOwner, String)>,
        numbering: LineNumbering,
    ) -> Self {
        let mut text = String::new();
        let mut blocks = Vec::new();
        let (mut chars, mut lines) = (0, 0);
        for (owner, block_text) in block_texts {
            if !blocks.is_empty() {
                text.push('\n');
                chars += 1;
            }
            let block = ContentBlock {
                owner,
                first_char: chars,
                chars: block_text.chars().count(),
                first_line: lines,
                lines: block_text.split('\n').count(),
            };
            text.push_str(&block_text);
            chars += block.chars;
            lines += block.lines;
            blocks.push(block);
        }
        Self {
            body,
            calls,
            results,
            text,
            blocks,
            numbering,
        }
    }

    fn has_body_block(&self) -> bool {
        self.blocks
            .first()
            .is_some_and(|block| block.owner == BlockOwner::Message)
    }

    /// The blocks a read prints when a truncation keeps the first
    /// `kept_chars` characters: the first block, then each block starting
    /// among the kept characters.
    fn printed_blocks(&self, kept_chars: usize) -> impl Iterator<Item = &ContentBlock> {
        self.blocks
            .iter()
            .enumerate()
            .take_while(move |(index, block)| *index == 0 || block.first_char < kept_chars)
            .map(|(_, block)| block)
    }

    /// The characters a truncation keeps when one more block prints than
    /// with fewer: none for the first block, then one past each later
    /// block's first character, in order.
    fn block_print_starts(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        std::iter::once(0).chain(self.blocks.iter().skip(1).map(|block| block.first_char + 1))
    }

    /// The content of the lines `selected`, numbered as `--lines` numbers
    /// them, each block's lines below its owner's record; `matches` are
    /// marked `>`. A block without a selected line is left out.
    fn sliced(&self, selected: &BTreeSet<usize>, matches: &[usize]) -> Self {
        let lines = self.text.split('\n').collect::<Vec<_>>();
        let mut block_texts = Vec::new();
        let mut numbers = Vec::new();
        for block in &self.blocks {
            let block_lines = block.first_line + 1..=block.first_line + block.lines;
            let in_block = selected
                .iter()
                .copied()
                .filter(|line| block_lines.contains(line));
            let (text, block_numbers) = numbered_lines(&lines, in_block, matches);
            if !block_numbers.is_empty() {
                numbers.extend(block_numbers);
                block_texts.push((block.owner, text));
            }
        }
        let body = match block_texts.first() {
            Some((BlockOwner::Message, text)) => text.clone(),
            _ => String::new(),
        };
        Self::of_blocks(
            body,
            self.calls.clone(),
            self.results.clone(),
            block_texts,
            LineNumbering::Part(numbers),
        )
    }

    /// The lines and call records a truncation keeping the first `keep`
    /// characters, fewer than the content holds, leaves out.
    fn omission_after(&self, keep: usize) -> Omission {
        let mut chars = self.text.chars();
        let newlines_kept = chars
            .by_ref()
            .take(keep)
            .filter(|&char| char == '\n')
            .count();
        let first_omitted = newlines_kept + usize::from(chars.next() == Some('\n'));
        let (first_line, last_line) = self.numbering.reread_lines(self, first_omitted);
        let dropped_calls = self
            .blocks
            .iter()
            .skip(1)
            .filter(|block| block.first_char >= keep)
            .filter_map(|block| match block.owner {
                BlockOwner::Call(call) => Some(self.calls[call].address),
                _ => None,
            })
            .collect();
        Omission {
            first_line,
            last_line,
            dropped_calls,
        }
    }
}

/// The record a block of a message's content belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockOwner {
    /// The message's own record.
    Message,
    /// The record of the call at this index of the content's calls.
    Call(usize),
    /// The record of the result at this index of the content's results.
    Result(usize),
}

/// The span of one block of a message's content: its first character and
/// line in the content's text, and how many it holds.
#[derive(Clone, Copy, Debug)]
struct ContentBlock {
    owner: BlockOwner,
    first_char: usize,
    chars: usize,
    first_line: usize,
    lines: usize,
}

/// The map from the lines of a message's content to the numbering `--lines`
/// reads: the lines of a one-message read, each command whole.
#[derive(Clone, Debug)]
enum LineNumbering {
    /// Line `i` of the content is line `i + 1`.
    Own,
    /// The content shows a command's first line alone, as a read of several
    /// messages does: a call's header has fewer lines than `--lines` counts.
    CommandsShortened,
    /// The line each line of part of the content shows, as a slice or a call
    /// read alone shows it; `None` for a `...` between a slice's runs.
    Part(Vec<Option<usize>>),
}

impl LineNumbering {
    /// The first and last line, in the numbering `--lines` reads, of the
    /// content from its line `first_omitted` on. A call's header shortened
    /// in the content is reread whole.
    fn reread_lines(&self, content: &MessageContent<'_>, first_omitted: usize) -> (usize, usize) {
        match self {
            Self::Own => (first_omitted + 1, content.text.split('\n').count()),
            Self::Part(numbers) => {
                let last = numbers.iter().flatten().copied().max().unwrap_or(1);
                let first = numbers
                    .get(first_omitted..)
                    .and_then(|rest| rest.iter().flatten().next().copied())
                    .unwrap_or(last);
                (first, last)
            }
            Self::CommandsShortened => {
                let mut next_line = 1;
                let mut first = None;
                for block in &content.blocks {
                    let block_lines = block.first_line..block.first_line + block.lines;
                    if first.is_none() && block_lines.contains(&first_omitted) {
                        first = Some(match block.owner {
                            BlockOwner::Call(_) => next_line,
                            _ => next_line + first_omitted - block.first_line,
                        });
                    }
                    next_line += match block.owner {
                        BlockOwner::Call(call) => content.calls[call].whole_header_lines(),
                        _ => block.lines,
                    };
                }
                let last = next_line - 1;
                (first.unwrap_or(last), last)
            }
        }
    }
}

/// The lines and call records a truncation leaves out of a message's content.
#[derive(Clone, Debug)]
struct Omission {
    /// The first and last omitted line, in the numbering `--lines` reads.
    first_line: usize,
    last_line: usize,
    dropped_calls: Vec<CallAddress>,
}

#[derive(Clone, Debug)]
struct ShownCall<'a> {
    address: CallAddress,
    time: Option<TimeSpan>,
    header: String,
    /// `Abbreviated` when `header` shortens a command, `Whole` otherwise.
    header_copy: CallCopy,
    name: &'a str,
    tool: Tool,
    input: &'a Value,
    /// The messages other than its own holding its result: `result=`.
    results: Vec<usize>,
    /// The sub-agent the call launches: `launches=`.
    launches: Option<Handle>,
    /// The running sub-agent the call sends to: `target=`.
    target: Option<Handle>,
}

impl ShownCall<'_> {
    fn record<'r>(&'r self, subagent: Option<&'r SubagentTag>) -> CallRecord<'r> {
        CallRecord {
            address: self.address,
            subagent,
            time: self.time,
            launches: self.launches,
            target: self.target,
            results: &self.results,
            name: self.name,
            input: self.input,
        }
    }

    /// The lines of the header as a one-message read shows it, each line of
    /// a command.
    fn whole_header_lines(&self) -> usize {
        tool_call_read_text(self.name, self.tool, self.input, CommandLines::All)
            .split('\n')
            .count()
    }
}

/// A tool result in the message holding its call, printed below the call's
/// address: `m5.1 result 10:00:00`.
#[derive(Clone, Debug)]
struct ShownResult<'a> {
    call: Call<'a>,
    time: Option<TimeSpan>,
    text: String,
}

impl ShownResult<'_> {
    fn record<'r>(&'r self, subagent: Option<&'r SubagentTag>) -> CallResultRecord<'r> {
        CallResultRecord {
            call: self.call,
            subagent,
            time: self.time,
        }
    }
}

/// The characters of a message's content that `truncation` keeps; every one
/// without a truncation.
fn kept_chars(truncation: Option<&Truncation>) -> usize {
    truncation.map_or(usize::MAX, |truncation| truncation.kept_chars)
}

/// A message's content as a budget shortens it.
#[derive(Clone, Debug)]
struct Truncation {
    /// The characters of the content kept, from its start.
    kept_chars: usize,
    /// Those characters, without a line break at their end.
    text: String,
    /// The lines and call records the kept characters leave out; `None` for
    /// a content measured without any of it.
    omission: Option<Omission>,
}

impl Truncation {
    fn empty() -> Self {
        Self {
            kept_chars: 0,
            text: String::new(),
            omission: None,
        }
    }

    /// The first `keep` characters of `text`, leaving out `omission`.
    fn keeping(text: &str, keep: usize, omission: Omission) -> Self {
        let mut kept = text.chars().take(keep).collect::<String>();
        if kept.ends_with('\n') {
            kept.pop();
        }
        Self {
            kept_chars: keep,
            text: kept,
            omission: Some(omission),
        }
    }

    /// The kept lines; none when no character is kept.
    fn lines(&self) -> Vec<&str> {
        if self.kept_chars == 0 {
            Vec::new()
        } else {
            self.text.split('\n').collect()
        }
    }
}

/// A record of `agent read` after its conversation record: a message it
/// shows, or a run of consecutive messages its flags hide whole.
#[derive(Clone, Debug)]
enum ReadEntry<'a> {
    Message {
        rendered: Box<RenderedMessage<'a>>,
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

    /// The sub-agent whose messages the entry holds; `None` for the
    /// session's own.
    fn subagent(&self) -> Option<&SubagentTag> {
        match self {
            Self::Message { rendered, .. } => rendered.subagent.as_ref(),
            Self::Hidden(run) => run.subagent.as_ref(),
        }
    }

    fn write<'e>(&'e self, writer: &mut RecordWriter<'e>) {
        match self {
            Self::Message { rendered, hidden } => {
                rendered.write(hidden, rendered.truncation.as_ref(), writer)
            }
            Self::Hidden(run) => run.write(writer),
        }
    }

    /// The characters of the entry's records alone, at full size, without a
    /// leading `day` record.
    fn measured_chars(&self) -> usize {
        match self {
            Self::Message { rendered, hidden } => {
                rendered.measured_chars(hidden, rendered.truncation.as_ref())
            }
            Self::Hidden(run) => {
                let mut writer = RecordWriter::measuring(run.times);
                run.write(&mut writer);
                writer.into_output().chars().count()
            }
        }
    }

    /// The days of the entry's first and last timed records, a message's
    /// content truncated to `truncation`.
    fn days(&self, truncation: Option<&Truncation>) -> EntryDays {
        match self {
            Self::Message { rendered, .. } => rendered.days(truncation),
            Self::Hidden(run) => {
                let day = run.times.map(|span| RecordTime::of(span).day());
                (day, day)
            }
        }
    }
}

/// Write the records of the `selected` entries, each run of one
/// conversation's after its `conversation` record and the legend records of
/// the sub-agents the run's entries name. A run whose messages are all hidden
/// ends with the `read` record that shows them.
fn write_entries<'e>(
    writer: &mut RecordWriter<'e>,
    entries: &'e [ReadEntry<'_>],
    selected: &[usize],
) {
    for run in selected.chunk_by(|&first, &next| {
        entries[first].conversation().reference.canonical()
            == entries[next].conversation().reference.canonical()
    }) {
        let mut subagents = run
            .iter()
            .filter_map(|&index| entries[index].subagent())
            .collect::<Vec<_>>();
        subagents.sort_by_key(|subagent| subagent.handle);
        subagents.dedup();
        writer.enter_conversation(entries[run[0]].conversation(), &subagents);
        for &index in run {
            entries[index].write(writer);
        }
        let spans = run
            .iter()
            .map(|&index| EntrySpan::of(&entries[index]))
            .collect::<Vec<_>>();
        if let Some((reference, runs, flags)) = recipe_showing_hidden_entries(spans.iter()) {
            writer.write_read_record(reference, &runs, flags);
        }
    }
}

/// Consecutive messages of one owner, the session or a sub-agent, that do
/// not show text; at least one hides a part.
#[derive(Clone, Debug)]
struct HiddenRun<'a> {
    conversation: &'a ResolvedConversation,
    first: usize,
    last: usize,
    hidden: HiddenParts<'a>,
    times: Option<TimeSpan>,
    /// The calls outside the run that its hidden results answer: `calls=`.
    answered: Vec<CallAddress>,
    /// The sub-agent whose messages it holds; `None` for the session's own.
    subagent: Option<SubagentTag>,
    /// The sub-agents its hidden calls launch: `launches=`.
    launches: Vec<Handle>,
    /// The running sub-agents its hidden calls send to, in handle order:
    /// `target=`.
    targets: Vec<Handle>,
    /// The read's flags and those that show its messages.
    flags: ContentVisibility,
}

/// One message's share of a [`HiddenRun`].
struct HiddenMessage<'a> {
    message: &'a AgentMessage,
    hidden: HiddenParts<'a>,
    /// The calls in other messages its hidden results answer.
    answered: Vec<CallAddress>,
    subagent: Option<SubagentTag>,
    /// The sub-agents its hidden calls launch.
    launches: Vec<Handle>,
    /// The running sub-agents its hidden calls send to.
    targets: Vec<Handle>,
}

impl<'a> HiddenRun<'a> {
    fn new(
        conversation: &'a ResolvedConversation,
        visibility: ContentVisibility,
        first: HiddenMessage<'a>,
    ) -> Self {
        let ordinal = first.message.ordinal;
        let mut run = Self {
            conversation,
            first: ordinal,
            last: ordinal,
            hidden: HiddenParts::default(),
            times: None,
            answered: Vec::new(),
            subagent: first.subagent.clone(),
            launches: Vec::new(),
            targets: Vec::new(),
            flags: visibility,
        };
        run.push(first);
        run
    }

    /// True when `next` follows the run's last message and has the run's
    /// owner.
    fn continues_with(&self, next: &HiddenMessage<'_>) -> bool {
        let handle = |subagent: &Option<SubagentTag>| subagent.as_ref().map(|tag| tag.handle);
        self.last + 1 == next.message.ordinal && handle(&self.subagent) == handle(&next.subagent)
    }

    fn push(&mut self, next: HiddenMessage<'a>) {
        self.last = next.message.ordinal;
        self.flags.merge(flags_showing(next.message));
        self.hidden.add(&next.hidden);
        self.times = [self.times, TimeSpan::of_message(next.message)]
            .into_iter()
            .flatten()
            .reduce(TimeSpan::union);
        self.answered.extend(next.answered);
        self.launches.extend(next.launches);
        self.targets.extend(next.targets);
    }

    /// The run ends here: the calls its results answer from inside it are
    /// not named.
    fn finished(mut self) -> Self {
        self.answered
            .retain(|call| !(self.first..=self.last).contains(&call.ordinal));
        self.answered.sort();
        self.answered.dedup();
        self.targets.sort_unstable();
        self.targets.dedup();
        self
    }

    fn write(&self, writer: &mut RecordWriter<'_>) {
        writer.write_hidden_record(&HiddenRecord {
            first: self.first,
            last: self.last,
            subagent: self.subagent.as_ref(),
            time: self.times,
            launches: &self.launches,
            targets: &self.targets,
            count_atoms: self.hidden.count_atoms(),
            answered: &self.answered,
        });
    }
}

/// The `agent read` header. `cut=`, `omit=`, `tz=` and the `hidden-` atoms
/// depend on the entries the budget selects; the other atoms are fixed here.
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

    fn line(&self, cut: &str, omit: &str, offset: FixedOffset, hidden_atoms: &str) -> String {
        format!(
            "protocol agent-read cut={} chars={} policy={} omit={omit}{}{}{hidden_atoms}{}\n",
            escape_atom(cut),
            budget_atom(self.budget),
            self.policy,
            tz_atom(offset),
            self.window,
            self.warnings
        )
    }

    /// The fixed atoms: the header a budget estimate counts beyond `cut=`,
    /// `chars=`, `omit=` and the `hidden-` atoms. Every offset is as long as
    /// the local offset at `written_at`.
    fn fixed_atoms(&self, written_at: DateTime<Utc>) -> String {
        format!(
            " policy={}{}{}{}",
            self.policy,
            tz_atom(output_offset(None, written_at)),
            self.window,
            self.warnings
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
    let priority = focus.map_or(ReadPriority::Start, ReadPriority::Focus);
    format_read_with_warnings(
        requests,
        priority,
        slice,
        options,
        &[],
        crate::clock::TestClock::start_wall(),
    )
}

/// The read of `requests`. `written_at` sets the `tz=` offset of a read that
/// does not show a time.
pub fn format_read_with_warnings(
    requests: &[ReadRequest<'_>],
    priority: ReadPriority,
    slice: Option<&ReadSlice>,
    options: ProtocolOptions,
    warnings: &[AgentWarning],
    written_at: DateTime<Utc>,
) -> Result<String> {
    let one_message = reads_one_message(requests);
    // The header names every flag a request reads with.
    let mut header_options = options;
    let mut entries = Vec::new();
    let mut request_entry_counts = Vec::with_capacity(requests.len());
    let mut messages_in_window = 0;
    for request in requests {
        let request_options = request.options(options);
        header_options = header_options.showing(request_options.visibility());
        let shape = ReadShape {
            options: request_options,
            commands: request.command_lines(one_message),
            slice,
            anchors: one_message && request.call.is_none(),
        };
        let index = TranscriptIndex::of(request.transcript);
        let mut selected = requested_messages(request, &index)?;
        messages_in_window += selected
            .iter()
            .filter(|message| is_in_window(message, options.window))
            .count();
        if let ReadPriority::Tail(count) = &priority {
            selected = tail_messages(request, selected, &index.links, *count, request_options);
        }
        let request_entries = read_entries(request, selected, &index, &shape)?;
        request_entry_counts.push(request_entries.len());
        entries.extend(request_entries);
    }
    let (warning_count, warning_records) = format_warning_records(warnings);
    let header = ReadHeader::new(
        header_options,
        messages_in_window,
        warning_count,
        &warning_records,
    );
    let (focus, kept_end) = match priority {
        ReadPriority::Start => (None, KeptEnd::Start),
        ReadPriority::Focus(focus) => (Some(focus), KeptEnd::Start),
        ReadPriority::Tail(_) => (
            None,
            KeptEnd::RequestEnds(TailOrder::new(&request_entry_counts)),
        ),
    };

    let budget = budget_beside_warnings(options.budget, &warning_records);
    let protected = focused_indices(&entries, focus.as_ref());
    let BudgetSelection {
        mut selected,
        truncated,
    } = kept_end.select(&entries, focus, budget, &header.fixed_atoms(written_at));
    let spans = entries.iter().map(EntrySpan::of).collect::<Vec<_>>();
    let render = |entries: &[ReadEntry<'_>], selected: &[usize], cut: &str, size| {
        let mut writer = RecordWriter::new(size);
        write_entries(&mut writer, entries, selected);
        let selected_set = selected.iter().copied().collect::<BTreeSet<_>>();
        let hidden_atoms = hidden_in(entries, selected.iter().copied()).total_atoms();
        let mut output = header.line(
            cut,
            &omitted_message_ranges(&spans, &selected_set),
            writer.header_offset(written_at),
            &hidden_atoms,
        );
        output.push_str(&writer.into_output());
        for record in continuation_records(&spans, &selected_set) {
            output.push_str(&record);
        }
        output
    };

    // Fit the bodies under a `cut=` that includes `body`, then render with the
    // `cut=` the shown bodies need: dropping `body` from `cut=` shortens the
    // header.
    let fit_and_render =
        |entries: &mut [ReadEntry<'_>], selected: &[usize], truncatable: &BTreeSet<usize>| {
            if let Some(budget) = budget {
                let cut = read_cut(entries.len(), selected, !truncatable.is_empty());
                fit_bodies(entries, selected, truncatable, budget, &render, &cut);
            }
            let cut = read_cut(
                entries.len(),
                selected,
                any_body_truncated(entries, selected),
            );
            render(entries, selected, &cut, RecordSize::Shortened)
        };
    let mut output = fit_and_render(&mut entries, &selected, &truncated);
    if let Some(budget) = budget
        && output.chars().count() > budget
    {
        if !protected.is_empty() {
            selected.retain(|index| protected.contains(index));
        }
        let all_selected = |selected: &[usize]| selected.iter().copied().collect::<BTreeSet<_>>();
        output = fit_and_render(&mut entries, &selected, &all_selected(&selected));
        while output.chars().count() > budget && !selected.is_empty() {
            let Some(omitted) = kept_end.next_to_omit(&selected, &protected) else {
                break;
            };
            selected.remove(omitted);
            output = fit_and_render(&mut entries, &selected, &all_selected(&selected));
        }
        if output.chars().count() > budget {
            return Ok(cut_short(&output, budget));
        }
    }
    output.push_str(&warning_records.concat());
    Ok(output)
}

#[cfg(test)]
pub fn format_outline(
    resolved: &ResolvedConversation,
    transcript: &AgentTranscript,
    options: ProtocolOptions,
) -> String {
    let request = ReadRequest {
        resolved,
        transcript,
        range: None,
        subagent: None,
        call: None,
    };
    format_outline_with_warnings(
        &request,
        options,
        &[],
        crate::clock::TestClock::start_wall(),
    )
    .expect("an outline of a whole session")
}

/// The outline of the messages `request` names: a range, a sub-agent's
/// alone, or the whole session. `written_at` sets the `tz=` offset of an
/// outline that does not show a time.
pub fn format_outline_with_warnings(
    request: &ReadRequest<'_>,
    options: ProtocolOptions,
    warnings: &[AgentWarning],
    written_at: DateTime<Utc>,
) -> Result<String> {
    let options = request.options(options);
    let (resolved, transcript) = (request.resolved, request.transcript);
    let index = TranscriptIndex::of(transcript);
    let requested = requested_messages(request, &index)?;
    let messages_in_window = requested
        .iter()
        .filter(|message| is_in_window(message, options.window))
        .count();
    let messages = OutlineMessages::new(request, requested, &index, options, written_at);
    let offset = messages.header_offset;
    let header_fields = format!(
        "policy={} messages={}{}{}{}",
        options.visibility().atom(),
        transcript.messages.len(),
        time_atoms(TimeSpan::of_messages(&transcript.messages), offset),
        tz_atom(offset),
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
        output.push_str(&messages.legend_records(records));
        output.push_str(&outline_records_text(records, offset));
        for record in &warning_records {
            output.push_str(record);
        }
        output
    };

    let Some(budget) = options.budget else {
        return Ok(render(&messages.records(OUTLINE_MIN_SEGMENT_SIZE)));
    };
    Ok(
        fitted_outline(&messages, budget, render).unwrap_or_else(|| {
            let coarsest = messages.records(messages.coarsest_segment_size());
            let header_fields = OutlineHeaderFields {
                fields: &header_fields,
                offset,
            };
            truncated_outline(
                &messages,
                &coarsest,
                budget,
                header_fields,
                (warning_count, &warning_records),
            )
        }),
    )
}

/// The fixed atoms of an outline's header, and the offset its `tz=` names.
#[derive(Clone, Copy)]
struct OutlineHeaderFields<'f> {
    fields: &'f str,
    offset: FixedOffset,
}

/// `records`, with the `day` record before each message record on another
/// day, below a header whose `tz=` names `offset`.
fn outline_records_text(records: &[OutlineRecord], offset: FixedOffset) -> String {
    let mut text = String::new();
    let mut days = DayRecords::below_header(Some(offset));
    for record in records {
        if let Some(day) = record.day.and_then(|day| days.record_before(day)) {
            text.push_str(&day);
        }
        text.push_str(&record.line);
    }
    text
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
    let fits = |size| render(&messages.records(size)).chars().count() <= budget;
    let coarsest = messages.coarsest_segment_size();
    if !fits(coarsest) {
        return None;
    }
    let size = (OUTLINE_MIN_SEGMENT_SIZE..=coarsest).find(|&size| fits(size))?;
    Some(render(&messages.records(size)))
}

/// The leading `records` that fit `budget` under a `cut=tail` header, a
/// `continue read` record for the omitted messages, then every warning
/// record, whole: `warnings` holds their count and the records.
fn truncated_outline(
    messages: &OutlineMessages<'_>,
    records: &[OutlineRecord],
    budget: usize,
    header: OutlineHeaderFields<'_>,
    warnings: (usize, &[String]),
) -> String {
    let (warning_count, warning_records) = warnings;
    let conversation = conversation_record(messages.resolved);
    let header_fields = header.fields;
    let warning_record_count = warning_records.len();
    let keeping = |keep: usize| {
        let omitted = records.len() - keep;
        let mut truncated = format!(
            "protocol agent-outline cut=tail chars={budget} {header_fields} omitted-records={omitted} warnings={warning_count} warning-records={warning_record_count}\n"
        );
        truncated.push_str(&conversation);
        truncated.push_str(&messages.legend_records(&records[..keep]));
        truncated.push_str(&outline_records_text(&records[..keep], header.offset));
        if let Some(first_omitted) = records.get(keep) {
            let omitted = messages.requested.between(
                messages.ordinal(first_omitted.covers.start),
                messages.ordinal(messages.len() - 1),
            );
            truncated.push_str(&continue_read_record(
                &messages.resolved.reference.canonical(),
                &omitted,
                messages.visibility,
            ));
        }
        truncated.push_str(&warning_records.concat());
        truncated
    };
    (0..=records.len())
        .rev()
        .map(keeping)
        .find(|truncated| truncated.chars().count() <= budget)
        .unwrap_or_else(|| cut_short(&keeping(0), budget))
}

/// One line of an outline, the indices of the visible messages it covers,
/// the handles of the sub-agents whose messages it covers, and, for a
/// message's record, the day its time falls on.
struct OutlineRecord {
    line: String,
    covers: Range<usize>,
    subagents: Vec<Handle>,
    day: Option<Day>,
}

/// The messages an outline shows. A segment's character count and times are
/// found in constant time, and each message's snippet is made on first use,
/// because choosing a segment size renders an outline per candidate size.
struct OutlineMessages<'a> {
    resolved: &'a ResolvedConversation,
    /// The messages the outline's ref names, shown or not: a range, a
    /// sub-agent's launch, messages and reports, or the whole session.
    requested: MessageRuns,
    visible: Vec<RenderedMessage<'a>>,
    /// The characters of the visible messages before each index.
    chars_before: Vec<usize>,
    times: RangeSpans,
    snippets: Vec<OnceCell<String>>,
    /// Each run of consecutive visible messages of one sub-agent, in order.
    subagent_runs: Vec<SubagentRun>,
    /// The sub-agent of each handle a visible message carries.
    subagents: HashMap<Handle, SubagentTag>,
    /// The offset the header's `tz=` names: that of the outline's first time
    /// and of its first `day` record.
    header_offset: FixedOffset,
    /// The outline's flags. A read with them shows the outline's messages.
    visibility: ContentVisibility,
}

/// Consecutive visible messages of one sub-agent: their indices among the
/// visible messages, and its handle.
struct SubagentRun {
    indices: Range<usize>,
    handle: Handle,
}

impl<'a> OutlineMessages<'a> {
    /// The `requested` messages of `request` that `options` show, each
    /// named as `index` names it. `written_at` sets the header offset when
    /// none of them shows a time.
    fn new(
        request: &ReadRequest<'a>,
        requested: Vec<&'a AgentMessage>,
        index: &TranscriptIndex<'a>,
        options: ProtocolOptions,
        written_at: DateTime<Utc>,
    ) -> Self {
        let resolved = request.resolved;
        let mut requested_runs = MessageRuns::default();
        for message in &requested {
            requested_runs.push(message.ordinal, message.ordinal);
        }
        let visible: Vec<_> = requested
            .into_iter()
            .filter_map(|message| {
                let mut rendered = shown_message(
                    resolved,
                    message,
                    &index.links,
                    options,
                    CommandLines::FirstLine,
                )?;
                rendered.name_subagents(&index.subagents);
                Some(rendered)
            })
            .collect();
        let chars_before = std::iter::once(0)
            .chain(visible.iter().scan(0, |total, message| {
                *total += message.content.text.chars().count();
                Some(*total)
            }))
            .collect();
        let mut subagent_runs: Vec<SubagentRun> = Vec::new();
        let mut subagents = HashMap::new();
        for (position, tag) in visible
            .iter()
            .enumerate()
            .filter_map(|(position, message)| Some((position, message.subagent.as_ref()?)))
        {
            match subagent_runs.last_mut() {
                Some(run) if run.handle == tag.handle && run.indices.end == position => {
                    run.indices.end += 1;
                }
                _ => subagent_runs.push(SubagentRun {
                    indices: position..position + 1,
                    handle: tag.handle,
                }),
            }
            subagents.insert(tag.handle, tag.clone());
        }
        Self {
            resolved,
            requested: requested_runs,
            chars_before,
            times: RangeSpans::new(visible.iter().map(|message| message.times).collect()),
            snippets: visible.iter().map(|_| OnceCell::new()).collect(),
            subagent_runs,
            subagents,
            header_offset: output_offset(
                visible.iter().find_map(|message| message.times),
                written_at,
            ),
            visible,
            visibility: options.visibility(),
        }
    }

    /// The legend record of each sub-agent whose messages `records` cover,
    /// in handle order.
    fn legend_records(&self, records: &[OutlineRecord]) -> String {
        records
            .iter()
            .flat_map(|record| record.subagents.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|handle| self.subagents[&handle].legend())
            .collect()
    }

    /// The handles of the sub-agents whose messages the visible messages at
    /// `covers` hold, in handle order.
    fn subagents_in(&self, covers: Range<usize>) -> Vec<Handle> {
        let first = self
            .subagent_runs
            .partition_point(|run| run.indices.end <= covers.start);
        let mut handles = self.subagent_runs[first..]
            .iter()
            .take_while(|run| run.indices.start < covers.end)
            .map(|run| run.handle)
            .collect::<Vec<_>>();
        handles.sort_unstable();
        handles.dedup();
        handles
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

    fn snippet(&self, index: usize) -> &str {
        self.snippets[index].get_or_init(|| snippet(&self.visible[index].content.text))
    }

    /// One record per message in a short session; otherwise one per
    /// `segment_size` messages.
    fn records(&self, segment_size: usize) -> Vec<OutlineRecord> {
        let count = self.len();
        if count <= OUTLINE_SHORT_MESSAGE_LIMIT {
            return (0..count).map(|index| self.message_record(index)).collect();
        }
        (0..count)
            .step_by(segment_size)
            .map(|start| self.segment_record(start..count.min(start + segment_size)))
            .collect()
    }

    /// `m12 user 14:30:12 chars=340 | snippet`. An outline does not print
    /// call records: a result names the tool of each call it answers.
    fn message_record(&self, index: usize) -> OutlineRecord {
        let rendered = &self.visible[index];
        let time = rendered.times.map(RecordTime::of);
        let record = MessageRecord {
            ordinal: rendered.message.ordinal,
            subagent: rendered.subagent.as_ref(),
            source_word: rendered.source_word(),
            time: rendered.times,
            from: rendered.from,
            answered: &rendered.answered,
            trailing_atoms: String::new(),
        };
        let start = message_record_start(
            &record,
            &time.map(RecordTime::field).unwrap_or_default(),
            true,
        );
        OutlineRecord {
            line: format!(
                "{start} chars={} | {}\n",
                self.chars(index..index + 1),
                self.snippet(index)
            ),
            covers: index..index + 1,
            subagents: rendered.subagent.iter().map(|tag| tag.handle).collect(),
            day: time.map(RecordTime::day),
        }
    }

    /// `seg m1..m20 s3,s4 chars=… | first snippet / last snippet`, with the
    /// handles of the sub-agents whose messages it covers.
    fn segment_record(&self, covers: Range<usize>) -> OutlineRecord {
        let (first, last) = (covers.start, covers.end - 1);
        let subagents = self.subagents_in(covers.clone());
        OutlineRecord {
            line: format!(
                "seg m{}..m{}{} chars={}{} | {} / {}\n",
                self.ordinal(first),
                self.ordinal(last),
                handles_field(&subagents),
                self.chars(covers.clone()),
                time_atoms(self.times.of(covers.clone()), self.header_offset),
                self.snippet(first),
                self.snippet(last)
            ),
            covers,
            subagents,
            day: None,
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

/// The first `budget` characters of `output`, an output longer than
/// `budget`, without a line break at their end: an output cut short never
/// ends on a whole record, and the command reports `budget-too-small` rather
/// than an output missing its last records and warnings.
pub(crate) fn cut_short(output: &str, budget: usize) -> String {
    let mut cut = output.chars().take(budget).collect::<String>();
    if cut.ends_with('\n') {
        cut.pop();
    }
    cut
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

/// The messages `request` names: a sub-agent's alone, a call's and its
/// results', its range, or the whole transcript.
fn requested_messages<'a>(
    request: &ReadRequest<'a>,
    index: &TranscriptIndex<'_>,
) -> Result<Vec<&'a AgentMessage>> {
    match (request.subagent, request.call, request.range) {
        (Some(handle), _, _) => subagent_messages(request.transcript, &index.subagents, handle),
        (None, Some(number), Some(range)) => {
            call_messages(request.transcript, &index.links, range.start, number)
        }
        _ => selected_messages(request.transcript, request.range),
    }
}

/// The message holding call `number` of message `ordinal`, and the messages
/// holding its result. A call the message does not hold is `out-of-range`.
fn call_messages<'a>(
    transcript: &'a AgentTranscript,
    links: &CallLinks<'_>,
    ordinal: usize,
    number: usize,
) -> Result<Vec<&'a AgentMessage>> {
    let message = selected_messages(transcript, Some(MessageRange::single(ordinal)))?
        .into_iter()
        .next()
        .ok_or_else(|| {
            AgentError::out_of_range(None, format!("message m{ordinal} is not in the transcript"))
        })?;
    let calls = numbered_calls(message).collect::<Vec<_>>();
    let call = number.checked_sub(1).and_then(|index| calls.get(index));
    let Some((_, AgentMessagePart::ToolUse { id, .. })) = call else {
        return Err(AgentError::out_of_range(
            None,
            format!(
                "call m{ordinal}.{number} exceeds message m{ordinal}'s {} calls",
                calls.len()
            ),
        )
        .into());
    };
    let results = links.results_of(message, id);
    Ok(transcript
        .messages
        .iter()
        .filter(|candidate| candidate.ordinal == ordinal || results.contains(&candidate.ordinal))
        .collect())
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

/// The messages a read of sub-agent `handle` alone shows: its launch, its
/// messages and its reports. A handle the transcript does not hold is a
/// `not-found` error.
fn subagent_messages<'a>(
    transcript: &'a AgentTranscript,
    subagents: &Subagents,
    handle: Handle,
) -> Result<Vec<&'a AgentMessage>> {
    let ordinals = subagents.messages_read_with(handle).ok_or_else(|| {
        AgentError::new(
            AgentErrorKind::NotFound,
            Some(&format!("s{handle}")),
            format!("sub-agent s{handle} was not found in the conversation"),
        )
    })?;
    Ok(transcript
        .messages
        .iter()
        .filter(|message| ordinals.binary_search(&message.ordinal).is_ok())
        .collect())
}

/// `selected` from its `count`th-last shown message onwards; all of
/// `selected` when fewer than `count` messages show.
fn tail_messages<'a>(
    request: &ReadRequest<'a>,
    mut selected: Vec<&'a AgentMessage>,
    links: &CallLinks<'a>,
    count: NonZeroUsize,
    options: ProtocolOptions,
) -> Vec<&'a AgentMessage> {
    let start = selected
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| {
            shown_message(
                request.resolved,
                message,
                links,
                options,
                CommandLines::FirstLine,
            )
            .is_some()
        })
        .nth(count.get() - 1)
        .map_or(0, |(index, _)| index);
    selected.split_off(start)
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

/// The options a read renders each message with.
struct ReadShape<'s> {
    options: ProtocolOptions,
    commands: CommandLines,
    slice: Option<&'s ReadSlice>,
    /// True for a one-message read: its record carries `anchor=`.
    anchors: bool,
}

/// The calls, results and sub-agents a read looks up across a whole
/// transcript.
struct TranscriptIndex<'a> {
    links: CallLinks<'a>,
    subagents: Subagents,
}

impl<'a> TranscriptIndex<'a> {
    fn of(transcript: &'a AgentTranscript) -> Self {
        let links = CallLinks::of(transcript);
        Self {
            subagents: Subagents::of(transcript, &links),
            links,
        }
    }
}

/// Each message of `request` inside the window that shows text, and a
/// [`HiddenRun`] for each run of those that do not, when the run hides a
/// part. A message outside the window, a gap in `messages`, or a message of
/// another owner ends the run.
fn read_entries<'a>(
    request: &ReadRequest<'a>,
    messages: Vec<&'a AgentMessage>,
    index: &TranscriptIndex<'a>,
    shape: &ReadShape<'_>,
) -> Result<Vec<ReadEntry<'a>>> {
    let options = shape.options;
    let mut entries = Vec::new();
    let mut run: Option<HiddenRun<'a>> = None;
    for message in messages {
        if !is_in_window(message, options.window) {
            entries.extend(hidden_run_entry(run.take()));
            continue;
        }
        let hidden = hidden_parts(message, options.visibility());
        match shown_message(
            request.resolved,
            message,
            &index.links,
            options,
            shape.commands,
        ) {
            Some(mut rendered) => {
                entries.extend(hidden_run_entry(run.take()));
                if shape.anchors {
                    rendered.anchor =
                        Some(request.transcript.message_anchor(request.resolved, message));
                }
                rendered.name_subagents(&index.subagents);
                let mut rendered = apply_read_slice(rendered, shape.slice)?;
                let hidden = match request.call_in(message) {
                    Some(number) => {
                        rendered.keep_call_alone(number);
                        HiddenParts::default()
                    }
                    None => {
                        rendered.keep_own_record_with_atoms(&hidden);
                        hidden
                    }
                };
                entries.push(ReadEntry::Message {
                    rendered: Box::new(rendered),
                    hidden,
                });
            }
            None => {
                let call_addresses = || {
                    numbered_calls(message).map(|(number, _)| CallAddress {
                        ordinal: message.ordinal,
                        number,
                    })
                };
                let next = HiddenMessage {
                    message,
                    hidden,
                    answered: calls_answered_by_hidden_results(message, &index.links, options),
                    subagent: index.subagents.tag_of(message.ordinal),
                    launches: call_addresses()
                        .filter_map(|address| index.subagents.launched_by(address))
                        .collect(),
                    targets: call_addresses()
                        .filter_map(|address| index.subagents.targeted_by(address))
                        .collect(),
                };
                match run.as_mut() {
                    Some(current) if current.continues_with(&next) => current.push(next),
                    _ => {
                        entries.extend(hidden_run_entry(run.take()));
                        run = Some(HiddenRun::new(request.resolved, options.visibility(), next));
                    }
                }
            }
        }
    }
    entries.extend(hidden_run_entry(run));
    Ok(entries)
}

fn hidden_run_entry(run: Option<HiddenRun<'_>>) -> Option<ReadEntry<'_>> {
    run.filter(|run| !run.hidden.is_empty())
        .map(|run| ReadEntry::Hidden(run.finished()))
}

/// The calls in other messages that `message`'s hidden results answer.
fn calls_answered_by_hidden_results(
    message: &AgentMessage,
    links: &CallLinks<'_>,
    options: ProtocolOptions,
) -> Vec<CallAddress> {
    let visibility = options.visibility();
    if !visibility.message_is_visible(message) {
        return Vec::new();
    }
    message
        .parts
        .iter()
        .filter(|part| !visibility.part_is_visible(part) && part_has_text(part))
        .filter_map(|part| match part {
            AgentMessagePart::ToolResult { tool_use_id, .. } => links.call_of(message, tool_use_id),
            _ => None,
        })
        .flat_map(|call| call.parts)
        .collect()
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

/// The flags a read needs to show `message`: `--subagents` for a sub-agent's
/// message, and, for a message without text, the flag of each kind of part
/// it holds.
pub(crate) fn flags_showing(message: &AgentMessage) -> ContentVisibility {
    let parts_with_text = || message.parts.iter().filter(|part| part_has_text(part));
    let has_text = parts_with_text().any(|part| matches!(part, AgentMessagePart::Text { .. }));
    let mut flags = ContentVisibility {
        subagents: message.parent_tool_use_id.is_some(),
        ..ContentVisibility::default()
    };
    if !has_text {
        for part in parts_with_text() {
            match part {
                AgentMessagePart::ToolUse { .. } => flags.tools = true,
                AgentMessagePart::ToolResult { .. } => flags.tool_results = true,
                AgentMessagePart::Thinking { .. } => flags.thinking = true,
                AgentMessagePart::Text { .. } => {}
            }
        }
    }
    flags
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

fn shown_message<'a>(
    conversation: &'a ResolvedConversation,
    message: &'a AgentMessage,
    links: &CallLinks<'a>,
    options: ProtocolOptions,
    commands: CommandLines,
) -> Option<RenderedMessage<'a>> {
    if !is_in_window(message, options.window) {
        return None;
    }
    render_message_with_commands(conversation, message, links, options, commands)
}

fn render_message_with_commands<'a>(
    conversation: &'a ResolvedConversation,
    message: &'a AgentMessage,
    links: &CallLinks<'a>,
    options: ProtocolOptions,
    commands: CommandLines,
) -> Option<RenderedMessage<'a>> {
    let visibility = options.visibility();
    if !visibility.message_is_visible(message) {
        return None;
    }
    let is_shown =
        |part: &AgentMessagePart| visibility.part_is_visible(part) && part_has_text(part);
    let mut body = Vec::new();
    let mut answered: Vec<AnsweredCall<'a>> = Vec::new();
    let mut results = Vec::new();
    for part in message.parts.iter().filter(|part| is_shown(part)) {
        match part {
            AgentMessagePart::Text { text, .. } => body.push(sanitize_agent_text(text)),
            AgentMessagePart::ToolResult {
                tool_use_id,
                content: Some(content),
                source,
            } => {
                let text = sanitize_agent_text(&tool_result_text(content));
                if let Some(call) = links.call_in_own_message(message, tool_use_id) {
                    results.push(ShownResult {
                        call,
                        time: source.timestamp.as_deref().and_then(TimeSpan::of_timestamp),
                        text,
                    });
                    continue;
                }
                if !text.trim().is_empty() {
                    answered.extend(links.call_of(message, tool_use_id));
                }
                body.push(text);
            }
            AgentMessagePart::Thinking { thinking, .. } => {
                body.push(format!("thinking: {}", sanitize_agent_text(thinking)));
            }
            AgentMessagePart::ToolUse { .. }
            | AgentMessagePart::ToolResult { content: None, .. } => {}
        }
    }
    let calls = numbered_calls(message)
        .filter(|(_, part)| is_shown(part))
        .filter_map(|(number, part)| match part {
            AgentMessagePart::ToolUse {
                id,
                name,
                tool,
                input,
                source,
            } => {
                let header = tool_call_read_text(name, *tool, input, commands);
                let header_copy = if commands == CommandLines::All
                    || header == tool_call_read_text(name, *tool, input, CommandLines::All)
                {
                    CallCopy::Whole
                } else {
                    CallCopy::Abbreviated
                };
                Some(ShownCall {
                    address: CallAddress {
                        ordinal: message.ordinal,
                        number,
                    },
                    time: source.timestamp.as_deref().and_then(TimeSpan::of_timestamp),
                    header,
                    header_copy,
                    name,
                    tool: *tool,
                    input,
                    results: links.results_of(message, id),
                    launches: None,
                    target: None,
                })
            }
            _ => None,
        })
        .filter(|call| !call.header.trim().is_empty())
        .collect::<Vec<_>>();
    let body = body
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    results.retain(|result| !result.text.trim().is_empty());
    answered.dedup();
    let numbering = match commands {
        CommandLines::All => LineNumbering::Own,
        CommandLines::FirstLine => LineNumbering::CommandsShortened,
    };
    (!body.is_empty() || !calls.is_empty() || !results.is_empty()).then(|| {
        let content = MessageContent::new(body, calls, results, numbering);
        RenderedMessage {
            conversation,
            message,
            times: TimeSpan::of_message(message),
            has_own_record: content.has_body_block(),
            content,
            truncation: None,
            slice: None,
            anchor: None,
            answered,
            visibility,
            subagent: None,
            from: None,
        }
    })
}

/// `rendered` sliced to `slice`. The slice numbers the lines of its content,
/// and each sliced line shows below the record it belongs to; the message's
/// own record, carrying `slice=`, always prints.
fn apply_read_slice<'a>(
    mut rendered: RenderedMessage<'a>,
    slice: Option<&ReadSlice>,
) -> Result<RenderedMessage<'a>> {
    let Some(slice) = slice else {
        return Ok(rendered);
    };
    let lines = rendered.content.text.split('\n').collect::<Vec<_>>();
    let selected: BTreeSet<usize>;
    let mut matches = Vec::new();
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
            selected = (range.start..=range.end).collect();
            rendered.slice = Some(format!("lines={}..{}", range.start, range.end));
        }
        ReadSlice::Match { query, context } => {
            let query_lower = query.to_lowercase();
            matches = lines
                .iter()
                .enumerate()
                .filter_map(|(index, line)| {
                    line.to_lowercase()
                        .contains(&query_lower)
                        .then_some(index + 1)
                })
                .collect();
            selected = matches
                .iter()
                .flat_map(|line| {
                    line.saturating_sub(*context).max(1)
                        ..=line.saturating_add(*context).min(lines.len())
                })
                .collect();
            rendered.slice = Some(format!(
                "match={} context={} hits={}",
                escape_atom(query),
                context,
                matches.len()
            ));
        }
    }
    rendered.content = rendered.content.sliced(&selected, &matches);
    rendered.has_own_record = true;
    Ok(rendered)
}

/// The `selected` lines of `lines`, each numbered from 1 and marked `>` when
/// it is one of `matches`, with `...` between runs; and the line each output
/// line shows, `None` for a `...`.
fn numbered_lines(
    lines: &[&str],
    selected: impl IntoIterator<Item = usize>,
    matches: &[usize],
) -> (String, Vec<Option<usize>>) {
    let mut output = Vec::new();
    let mut numbers = Vec::new();
    let mut previous = None;
    for line_number in selected {
        if previous.is_some_and(|previous| line_number > previous + 1) {
            output.push("...".to_string());
            numbers.push(None);
        }
        let marker = if matches.contains(&line_number) {
            ">"
        } else {
            " "
        };
        output.push(format!("{marker}{line_number}: {}", lines[line_number - 1]));
        numbers.push(Some(line_number));
        previous = Some(line_number);
    }
    (output.join("\n"), numbers)
}

fn any_body_truncated(entries: &[ReadEntry<'_>], selected: &[usize]) -> bool {
    selected.iter().any(|&index| {
        matches!(&entries[index], ReadEntry::Message { rendered, .. }
            if rendered.truncation.is_some())
    })
}

/// Truncate the content of the `truncatable` messages so `selected` fits
/// `budget`, every other message whole. When the read fits with each message
/// at its [`TRUNCATION_FLOOR_CHARS`], each keeps them; the rest of the budget
/// is shared among the messages from the shortest up, and a message shorter
/// than its share shows whole. A message's share pays for the `day` records
/// its truncation adds between entries.
fn fit_bodies(
    entries: &mut [ReadEntry<'_>],
    selected: &[usize],
    truncatable: &BTreeSet<usize>,
    budget: usize,
    render: &impl Fn(&[ReadEntry<'_>], &[usize], &str, RecordSize) -> String,
    cut: &str,
) {
    let mut messages = Vec::new();
    for &index in selected {
        if let ReadEntry::Message { rendered, hidden } = &mut entries[index] {
            rendered.truncation = None;
            if truncatable.contains(&index) {
                let whole = rendered.measured_chars(hidden, None);
                let floor = rendered
                    .floor_truncation()
                    .map(|floor| (rendered.measured_chars(hidden, Some(&floor)), floor))
                    .filter(|&(chars, _)| chars < whole);
                let floor_chars = floor.as_ref().map_or(whole, |&(chars, _)| chars);
                rendered.truncation = floor.map(|(_, floor)| floor);
                messages.push((index, whole, floor_chars));
            }
        }
    }
    if messages.is_empty() {
        return;
    }
    let rendered_chars = |entries: &[ReadEntry<'_>]| {
        render(entries, selected, cut, RecordSize::Full)
            .chars()
            .count()
    };
    // Measure the read with each message at its floor, not emptied: a floor
    // holding a date change prints the `day` record the next entry would
    // otherwise need.
    let at_floors = rendered_chars(entries);
    let floors_fit = at_floors <= budget;
    let mut extra = if floors_fit {
        budget - at_floors
    } else {
        let mut emptied = 0;
        for &(index, _, _) in &messages {
            if let ReadEntry::Message { rendered, hidden } = &mut entries[index] {
                let empty = Truncation::empty();
                emptied += rendered.measured_chars(hidden, Some(&empty));
                rendered.truncation = Some(empty);
            }
        }
        budget
            .saturating_sub(rendered_chars(entries))
            .saturating_add(emptied)
    };
    messages.sort_by_key(|&(_, whole, _)| whole);
    let mut days_between = DaysBetweenEntries::of(entries, selected);
    let count = messages.len();
    for (position, (index, _, floor)) in messages.into_iter().enumerate() {
        let reserved = if floors_fit { floor } else { 0 };
        let ReadEntry::Message { rendered, hidden } = &mut entries[index] else {
            continue;
        };
        let days_now = days_between.chars();
        let room = reserved + extra / (count - position) + days_now;
        let chars_with_days_between = |truncation: Option<&Truncation>| {
            rendered.measured_chars(hidden, truncation)
                + days_between.chars_with(index, rendered.days(truncation))
        };
        let fitted = rendered.truncated_to_fit(room, chars_with_days_between);
        extra =
            (extra + reserved + days_now).saturating_sub(chars_with_days_between(fitted.as_ref()));
        days_between.set(index, rendered.days(fitted.as_ref()));
        rendered.truncation = fitted;
    }
}

/// The days of an entry's first and last timed records: the `day` records
/// between entries depend on them.
type EntryDays = (Option<Day>, Option<Day>);

/// The characters of the `day` records a read prints between its entries,
/// each entry given by its conversation's `ref=` and its [`EntryDays`]. The
/// records inside an entry count toward the entry's own characters.
fn day_records_between<'r>(entries: impl IntoIterator<Item = (&'r str, EntryDays)>) -> usize {
    let mut chars = 0;
    let mut last_ref = None;
    let mut days = DayRecords::below_header(None);
    for (full_ref, (first_day, last_day)) in entries {
        if last_ref != Some(full_ref) {
            last_ref = Some(full_ref);
            days.restart();
        }
        if let Some(day) = first_day.and_then(|day| days.record_before(day)) {
            chars += day.chars().count();
        }
        if let Some(last_day) = last_day {
            days.record_before(last_day);
        }
    }
    chars
}

/// The `day` records between a read's selected entries while a budget
/// truncates them: a truncation that moves an entry's last record to another
/// date adds or removes the one before the next entry.
struct DaysBetweenEntries {
    /// Each selected entry's index, its conversation's `ref=` and the days of
    /// its records as they print now.
    entries: Vec<(usize, String, EntryDays)>,
}

impl DaysBetweenEntries {
    fn of(entries: &[ReadEntry<'_>], selected: &[usize]) -> Self {
        let printed_days = |entry: &ReadEntry<'_>| match entry {
            ReadEntry::Message { rendered, .. } => rendered.days(rendered.truncation.as_ref()),
            ReadEntry::Hidden(_) => entry.days(None),
        };
        Self {
            entries: selected
                .iter()
                .map(|&index| {
                    let entry = &entries[index];
                    (
                        index,
                        entry.conversation().reference.full_ref(),
                        printed_days(entry),
                    )
                })
                .collect(),
        }
    }

    fn chars(&self) -> usize {
        day_records_between(
            self.entries
                .iter()
                .map(|(_, full_ref, days)| (full_ref.as_str(), *days)),
        )
    }

    /// [`Self::chars`] with the entry at `index` on `days`.
    fn chars_with(&self, index: usize, days: EntryDays) -> usize {
        day_records_between(self.entries.iter().map(|(entry, full_ref, entry_days)| {
            let days = if *entry == index { days } else { *entry_days };
            (full_ref.as_str(), days)
        }))
    }

    fn set(&mut self, index: usize, days: EntryDays) {
        for (entry, _, entry_days) in &mut self.entries {
            if *entry == index {
                *entry_days = days;
            }
        }
    }
}

/// A budget reserves room for this many first characters of an oversized
/// message, one too long for the room the budget leaves it. A truncated
/// message keeps them when the budget has room for every truncated message's
/// floor.
const TRUNCATION_FLOOR_CHARS: usize = 200;

/// A `continue read` record per conversation for the entries not in
/// `selected`: each run of consecutive messages as one ref, `;` between
/// refs, then the flags that show every omitted entry.
fn continuation_records(spans: &[EntrySpan], selected: &BTreeSet<usize>) -> Vec<String> {
    let mut groups: Vec<OmittedEntries<'_>> = Vec::new();
    for (index, span) in spans.iter().enumerate() {
        if selected.contains(&index) {
            continue;
        }
        match groups
            .iter_mut()
            .find(|group| group.first.full_ref == span.full_ref)
        {
            Some(group) => group.add(span),
            None => {
                let mut group = OmittedEntries {
                    first: span,
                    runs: MessageRuns::default(),
                    flags: ContentVisibility::default(),
                };
                group.add(span);
                groups.push(group);
            }
        }
    }
    groups
        .iter()
        .map(|group| continue_read_record(&group.first.canonical, &group.runs, group.flags))
        .collect()
}

/// The entries a budget omits from one conversation.
struct OmittedEntries<'s> {
    first: &'s EntrySpan,
    runs: MessageRuns,
    flags: ContentVisibility,
}

impl<'s> OmittedEntries<'s> {
    fn add(&mut self, span: &'s EntrySpan) {
        self.runs.push(span.first, span.last);
        self.flags.merge(span.flags);
    }
}

/// An entry's conversation and messages: the `omit=` atom and the
/// `continue read` records name them when a budget omits the entry.
#[derive(Clone, Debug)]
struct EntrySpan {
    full_ref: String,
    canonical: String,
    first: usize,
    last: usize,
    /// The flags that show the entry's messages: the read's, and for a run
    /// of hidden messages those its messages need.
    flags: ContentVisibility,
    /// True for a run of hidden messages; false for a message the read
    /// shows.
    is_hidden: bool,
}

impl EntrySpan {
    fn of(entry: &ReadEntry<'_>) -> Self {
        let reference = &entry.conversation().reference;
        let (flags, is_hidden) = match entry {
            ReadEntry::Message { rendered, .. } => (rendered.visibility, false),
            ReadEntry::Hidden(run) => (run.flags, true),
        };
        Self {
            full_ref: reference.full_ref(),
            canonical: reference.canonical(),
            first: entry.first_ordinal(),
            last: entry.last_ordinal(),
            flags,
            is_hidden,
        }
    }
}

/// The `read` record of each run of one conversation's `selected` entries
/// whose messages are all hidden, as [`write_entries`] writes it after the
/// run.
fn recipes_showing_hidden_messages(spans: &[EntrySpan], selected: &BTreeSet<usize>) -> Vec<String> {
    let shown = selected
        .iter()
        .map(|&index| &spans[index])
        .collect::<Vec<_>>();
    shown
        .chunk_by(|first, next| first.full_ref == next.full_ref)
        .filter_map(|run| recipe_showing_hidden_entries(run.iter().copied()))
        .map(|(reference, runs, flags)| read_record(reference, &runs, flags))
        .collect()
}

/// The conversation of `spans`, consecutive entries of one conversation, the
/// runs of their messages and the flags that show them, when none of the
/// entries shows a message: for a `read` record.
fn recipe_showing_hidden_entries<'s>(
    spans: impl Iterator<Item = &'s EntrySpan>,
) -> Option<(&'s str, MessageRuns, ContentVisibility)> {
    let mut reference = None;
    let mut runs = MessageRuns::default();
    let mut flags = ContentVisibility::default();
    for span in spans {
        if !span.is_hidden {
            return None;
        }
        reference.get_or_insert(span.canonical.as_str());
        runs.push(span.first, span.last);
        flags.merge(span.flags);
    }
    Some((reference?, runs, flags))
}

fn omitted_message_ranges(spans: &[EntrySpan], selected: &BTreeSet<usize>) -> String {
    let qualify = spans
        .first()
        .is_some_and(|first| spans.iter().any(|span| span.full_ref != first.full_ref));
    let mut ranges = Vec::new();
    let mut start = None;
    for index in 0..=spans.len() {
        let omitted = index < spans.len() && !selected.contains(&index);
        match (start, omitted) {
            (None, true) => start = Some(index),
            (Some(first), false) => {
                let (first_span, last_span) = (&spans[first], &spans[index - 1]);
                if first_span.full_ref == last_span.full_ref {
                    let prefix = if qualify {
                        format!("{}:", first_span.canonical)
                    } else {
                        String::new()
                    };
                    ranges.push(format!(
                        "{prefix}{}",
                        ordinal_range(first_span.first, last_span.last)
                    ));
                } else {
                    for span in &spans[first..index] {
                        ranges.push(message_range_ref(&span.canonical, span.first, span.last));
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

/// The part of a read its budget keeps first.
enum KeptEnd {
    /// The read's start, or its focus and the entries around it.
    Start,
    RequestEnds(TailOrder),
}

impl KeptEnd {
    fn select(
        &self,
        entries: &[ReadEntry<'_>],
        focus: Option<ProtocolFocus>,
        budget: Option<usize>,
        header_atoms: &str,
    ) -> BudgetSelection {
        match self {
            Self::Start => select_for_budget(entries, focus, budget, header_atoms),
            Self::RequestEnds(order) => {
                select_request_ends_for_budget(entries, order, budget, header_atoms)
            }
        }
    }

    /// The position in `selected`, not the entry index, of the next entry to
    /// omit.
    fn next_to_omit(&self, selected: &[usize], protected: &BTreeSet<usize>) -> Option<usize> {
        match self {
            Self::Start => selected
                .iter()
                .rposition(|index| !protected.contains(index)),
            Self::RequestEnds(order) => order.position_kept_last(selected),
        }
    }
}

/// The order a tail read's budget keeps entries in: each request's last
/// entry, from the last request to the first, then each request's
/// second-last entry, and so on.
struct TailOrder {
    entries_after_in_request: Vec<usize>,
    request: Vec<usize>,
}

impl TailOrder {
    fn new(request_entry_counts: &[usize]) -> Self {
        let mut entries_after_in_request = Vec::new();
        let mut request = Vec::new();
        for (request_index, &count) in request_entry_counts.iter().enumerate() {
            entries_after_in_request.extend((0..count).rev());
            request.extend(std::iter::repeat_n(request_index, count));
        }
        Self {
            entries_after_in_request,
            request,
        }
    }

    fn rank(&self, entry: usize) -> (usize, Reverse<usize>) {
        (
            self.entries_after_in_request[entry],
            Reverse(self.request[entry]),
        )
    }

    fn kept_first(&self) -> Vec<usize> {
        let mut entries = (0..self.request.len()).collect::<Vec<_>>();
        entries.sort_by_key(|&entry| self.rank(entry));
        entries
    }

    fn position_kept_last(&self, selected: &[usize]) -> Option<usize> {
        selected
            .iter()
            .enumerate()
            .max_by_key(|&(_, &entry)| self.rank(entry))
            .map(|(position, _)| position)
    }
}

#[derive(Debug)]
struct BudgetSelection {
    /// The entries a budget keeps.
    selected: Vec<usize>,
    /// The kept entries with a truncated body.
    truncated: BTreeSet<usize>,
}

impl BudgetSelection {
    fn everything(entries: &[ReadEntry<'_>]) -> Self {
        Self {
            selected: (0..entries.len()).collect(),
            truncated: BTreeSet::new(),
        }
    }
}

/// Each request's last entries that fit `budget`, taken in [`TailOrder`]. An
/// oversized entry is kept truncated when its header and its
/// [`TRUNCATION_FLOOR_CHARS`] fit; a request stops at its first entry that
/// does not fit even so. The first entry kept stays even over budget, for the
/// body truncation that follows.
fn select_request_ends_for_budget(
    entries: &[ReadEntry<'_>],
    order: &TailOrder,
    budget: Option<usize>,
    header_atoms: &str,
) -> BudgetSelection {
    let Some(budget) = budget else {
        return BudgetSelection::everything(entries);
    };
    let layout = CompactReadLayout::new(entries, header_atoms);
    let all = (0..entries.len()).collect::<BTreeSet<_>>();
    if layout.rendered_chars(&all, &BTreeSet::new(), budget) <= budget {
        return BudgetSelection::everything(entries);
    }
    let mut selected = BTreeSet::new();
    let mut truncated = BTreeSet::new();
    let mut stopped_requests = BTreeSet::new();
    let fits = |selected: &BTreeSet<usize>, truncated: &BTreeSet<usize>| {
        layout.rendered_chars(selected, truncated, budget) <= budget
    };
    for entry in order.kept_first() {
        let request = order.request[entry];
        if stopped_requests.contains(&request) {
            continue;
        }
        let is_first_kept = selected.is_empty();
        selected.insert(entry);
        if fits(&selected, &truncated) {
            continue;
        }
        if layout.can_truncate(entry) {
            truncated.insert(entry);
            if fits(&selected, &truncated) {
                continue;
            }
        }
        if !is_first_kept {
            selected.remove(&entry);
            truncated.remove(&entry);
            stopped_requests.insert(request);
        }
    }
    BudgetSelection {
        selected: selected.into_iter().collect(),
        truncated,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SelectionWork {
    serialized_entries: usize,
    candidate_checks: usize,
}

/// The characters of an entry's records alone, and its [`EntryDays`].
#[derive(Clone, Copy, Debug)]
struct EntrySize {
    chars: usize,
    days: EntryDays,
}

/// The length of every part of a read, to find the read of any selection of
/// its entries without writing it.
struct CompactReadLayout<'h, 'e> {
    whole: Vec<EntrySize>,
    /// Each entry with its content truncated to its
    /// [`TRUNCATION_FLOOR_CHARS`]; `None` for a run of hidden messages, or
    /// content no longer than that.
    truncated: Vec<Option<EntrySize>>,
    conversation_chars: Vec<usize>,
    /// The handle of the sub-agent each entry names and the characters of
    /// its legend record. A conversation's run prints each legend record
    /// once.
    legends: Vec<Option<(Handle, usize)>>,
    spans: Vec<EntrySpan>,
    /// The parts each entry hides: the header totals them.
    hidden: Vec<HiddenParts<'e>>,
    /// [`ReadHeader::fixed_atoms`].
    header_atoms: &'h str,
}

impl<'h, 'e> CompactReadLayout<'h, 'e> {
    fn new(entries: &[ReadEntry<'e>], header_atoms: &'h str) -> Self {
        let whole = entries
            .iter()
            .map(|entry| EntrySize {
                chars: entry.measured_chars(),
                days: entry.days(None),
            })
            .collect::<Vec<_>>();
        let truncated = entries
            .iter()
            .zip(&whole)
            .map(|(entry, whole)| match entry {
                ReadEntry::Message { rendered, hidden } => {
                    let floor = rendered.floor_truncation()?;
                    let chars = rendered.measured_chars(hidden, Some(&floor));
                    (chars < whole.chars).then(|| EntrySize {
                        chars,
                        days: entry.days(Some(&floor)),
                    })
                }
                ReadEntry::Hidden(_) => None,
            })
            .collect();
        Self {
            whole,
            truncated,
            conversation_chars: entries
                .iter()
                .map(|entry| conversation_record(entry.conversation()).chars().count())
                .collect(),
            legends: entries
                .iter()
                .map(|entry| {
                    entry
                        .subagent()
                        .map(|subagent| (subagent.handle, subagent.legend().chars().count()))
                })
                .collect(),
            spans: entries.iter().map(EntrySpan::of).collect(),
            hidden: entries.iter().map(|entry| entry.hidden().clone()).collect(),
            header_atoms,
        }
    }

    fn can_truncate(&self, entry: usize) -> bool {
        self.truncated[entry].is_some()
    }

    /// The entry at its floor when it is in `truncated` and has one, else
    /// whole.
    fn size(&self, entry: usize, truncated: &BTreeSet<usize>) -> EntrySize {
        truncated
            .contains(&entry)
            .then(|| self.truncated[entry])
            .flatten()
            .unwrap_or(self.whole[entry])
    }

    /// The characters of the read of `selected`, those in `truncated` at
    /// their floor, every record at full size.
    fn rendered_chars(
        &self,
        selected: &BTreeSet<usize>,
        truncated: &BTreeSet<usize>,
        budget: usize,
    ) -> usize {
        let header_atoms = self.header_atoms;
        let cut = read_cut(
            self.spans.len(),
            &selected.iter().copied().collect::<Vec<_>>(),
            !truncated.is_empty(),
        );
        let omitted = omitted_message_ranges(&self.spans, selected);
        let mut hidden = HiddenParts::default();
        for &index in selected {
            hidden.add(&self.hidden[index]);
        }
        let mut chars = format!(
            "protocol agent-read cut={cut} chars={budget} omit={omitted}{header_atoms}{}\n",
            hidden.total_atoms()
        )
        .chars()
        .count();
        chars += continuation_records(&self.spans, selected)
            .iter()
            .chain(&recipes_showing_hidden_messages(&self.spans, selected))
            .map(|record| record.chars().count())
            .sum::<usize>();
        let mut last_ref = None;
        let mut legends_written = HashSet::new();
        for index in selected {
            let full_ref = self.spans[*index].full_ref.as_str();
            if last_ref != Some(full_ref) {
                chars += self.conversation_chars[*index];
                last_ref = Some(full_ref);
                legends_written.clear();
            }
            if let Some((handle, legend_chars)) = self.legends[*index]
                && legends_written.insert(handle)
            {
                chars += legend_chars;
            }
            chars += self.size(*index, truncated).chars;
        }
        chars
            + day_records_between(selected.iter().map(|&index| {
                (
                    self.spans[index].full_ref.as_str(),
                    self.size(index, truncated).days,
                )
            }))
    }
}

/// A selection that grows from a read's start or focus, one neighbouring
/// entry at a time, while the entries fit `budget`.
struct GrowingSelection<'l, 'h, 'e> {
    layout: &'l CompactReadLayout<'h, 'e>,
    budget: usize,
    /// True without a focus: an oversized entry is kept truncated.
    truncates_to_fit: bool,
    selected: BTreeSet<usize>,
    truncated: BTreeSet<usize>,
    work: SelectionWork,
}

impl GrowingSelection<'_, '_, '_> {
    fn rendered_chars(&self) -> usize {
        self.layout
            .rendered_chars(&self.selected, &self.truncated, self.budget)
    }

    /// Add `candidate` whole when it fits, or truncated to its
    /// [`TRUNCATION_FLOOR_CHARS`] when that fits and the selection truncates
    /// to fit.
    fn try_add(&mut self, candidate: usize) -> bool {
        self.work.candidate_checks += 1;
        self.selected.insert(candidate);
        if self.rendered_chars() <= self.budget {
            return true;
        }
        if self.truncates_to_fit && self.layout.can_truncate(candidate) {
            self.truncated.insert(candidate);
            if self.rendered_chars() <= self.budget {
                return true;
            }
            self.truncated.remove(&candidate);
        }
        self.selected.remove(&candidate);
        false
    }
}

fn select_for_budget(
    entries: &[ReadEntry<'_>],
    focus: Option<ProtocolFocus>,
    budget: Option<usize>,
    header_atoms: &str,
) -> BudgetSelection {
    select_for_budget_with_work(entries, focus, budget, header_atoms).0
}

/// The read's start, or its focus, then the entries next to it while they
/// fit `budget`. Without a focus, an oversized entry is kept truncated when
/// its header and its [`TRUNCATION_FLOOR_CHARS`] fit, and the selection
/// continues with the entries after it. With a focus, the entries around it
/// are shown whole or omitted.
fn select_for_budget_with_work(
    entries: &[ReadEntry<'_>],
    focus: Option<ProtocolFocus>,
    budget: Option<usize>,
    header_atoms: &str,
) -> (BudgetSelection, SelectionWork) {
    let work = SelectionWork {
        serialized_entries: entries.len(),
        candidate_checks: 0,
    };
    let Some(budget) = budget else {
        return (BudgetSelection::everything(entries), work);
    };
    let layout = CompactReadLayout::new(entries, header_atoms);
    let all = (0..entries.len()).collect::<BTreeSet<_>>();
    if layout.rendered_chars(&all, &BTreeSet::new(), budget) <= budget {
        return (BudgetSelection::everything(entries), work);
    }

    let mut selected = focused_indices(entries, focus.as_ref());
    if selected.is_empty() && !entries.is_empty() {
        selected.insert(0);
    }
    let mut growing = GrowingSelection {
        layout: &layout,
        budget,
        truncates_to_fit: focus.is_none(),
        selected,
        truncated: BTreeSet::new(),
        work,
    };
    if growing.truncates_to_fit && growing.rendered_chars() > budget {
        growing.truncated = growing
            .selected
            .iter()
            .copied()
            .filter(|&entry| layout.can_truncate(entry))
            .collect();
    }
    loop {
        let mut changed = false;
        if let Some(first) = growing.selected.first().copied()
            && first > 0
        {
            changed |= growing.try_add(first - 1);
        }
        if let Some(last) = growing.selected.last().copied()
            && last + 1 < entries.len()
        {
            changed |= growing.try_add(last + 1);
        }
        if !changed {
            break;
        }
    }

    (
        BudgetSelection {
            selected: growing.selected.into_iter().collect(),
            truncated: growing.truncated,
        },
        growing.work,
    )
}

/// `cut=`: `body` when a shown body is truncated, written before the omitted
/// ends from [`cut_marker`].
fn cut_atom(body_truncated: bool, omitted_ends: &str) -> String {
    const BODY: &str = "body";
    match (body_truncated, omitted_ends) {
        (false, _) => omitted_ends.to_string(),
        (true, "none") => BODY.to_string(),
        (true, _) => format!("{BODY}+{omitted_ends}"),
    }
}

fn read_cut(total: usize, selected: &[usize], body_truncated: bool) -> String {
    cut_atom(body_truncated, &cut_marker(total, selected))
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
    use crate::agent::test_support::{
        day_record, local_time, record_time, source, text_message, text_message_at,
    };
    use crate::agent::transcript::AgentPartSource;
    use crate::clock::TestClock;
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
            rendered: Box::new(rendered),
            hidden: HiddenParts::default(),
        }
    }

    /// An entry for each message of `transcript` that a read with default
    /// flags shows.
    fn shown_entries<'a>(
        resolved: &'a ResolvedConversation,
        transcript: &'a AgentTranscript,
    ) -> Vec<ReadEntry<'a>> {
        let links = CallLinks::of(transcript);
        transcript
            .messages
            .iter()
            .filter_map(|message| {
                shown_message(
                    resolved,
                    message,
                    &links,
                    options(),
                    CommandLines::FirstLine,
                )
            })
            .map(shown)
            .collect()
    }

    /// The records of the `selected` entries, at full size.
    fn written(entries: &[ReadEntry<'_>], selected: &[usize]) -> String {
        let mut writer = RecordWriter::new(RecordSize::Full);
        write_entries(&mut writer, entries, selected);
        writer.into_output()
    }

    fn legacy_select_for_budget(
        messages: &[ReadEntry<'_>],
        focus: Option<ProtocolFocus>,
        budget: usize,
    ) -> Vec<usize> {
        let spans = messages.iter().map(EntrySpan::of).collect::<Vec<_>>();
        let render_chars = |selected: &BTreeSet<usize>| {
            let selected_list = selected.iter().copied().collect::<Vec<_>>();
            let header = format!(
                "protocol agent-read cut={} chars={budget} omit={}\n",
                read_cut(messages.len(), &selected_list, false),
                omitted_message_ranges(&spans, selected)
            );
            let records = written(messages, &selected_list);
            let continuation = continuation_records(&spans, selected).concat();
            header.chars().count() + records.chars().count() + continuation.chars().count()
        };
        let all = (0..messages.len()).collect::<BTreeSet<_>>();
        if render_chars(&all) <= budget {
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
                if render_chars(&selected) <= budget {
                    changed = true;
                } else {
                    selected.remove(&(first - 1));
                }
            }
            if let Some(last) = selected.last().copied()
                && last + 1 < messages.len()
            {
                selected.insert(last + 1);
                if render_chars(&selected) <= budget {
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
        let messages = shown_entries(&resolved, &transcript);

        for budget in (100..=4_000).step_by(37) {
            let focus = ProtocolFocus {
                conversation_full_ref: Some(resolved.reference.full_ref()),
                range: MessageRange::single(20),
            };
            let expected = legacy_select_for_budget(&messages, Some(focus.clone()), budget);
            let (actual, work) =
                select_for_budget_with_work(&messages, Some(focus), Some(budget), "");

            assert_eq!(
                actual.selected, expected,
                "selection differs at budget {budget}"
            );
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
        let messages = shown_entries(&first, &first_transcript)
            .into_iter()
            .chain(shown_entries(&second, &second_transcript))
            .collect::<Vec<_>>();

        for budget in (100..=2_000).step_by(29) {
            let expected = legacy_select_for_budget(&messages, None, budget);
            let (actual, work) = select_for_budget_with_work(&messages, None, Some(budget), "");

            assert_eq!(
                actual.selected, expected,
                "selection differs at budget {budget}"
            );
            assert_eq!(work.serialized_entries, messages.len());
            assert!(work.candidate_checks <= messages.len().saturating_mul(2));
        }
    }

    /// Replies an hour apart from 21:00 UTC on 2026-10-14 cross a local
    /// midnight in every zone: `day` records separate some of them.
    #[test]
    fn compact_read_layout_counts_day_records_as_output_does() {
        let resolved = resolved("session.jsonl");
        let start = chrono::DateTime::parse_from_rfc3339("2026-10-14T21:00:00Z").unwrap();
        let transcript = transcript(
            (1..=40)
                .map(|ordinal| {
                    let written = start + chrono::Duration::hours(ordinal as i64);
                    text_message_at(
                        ordinal,
                        AgentMessageRole::Assistant,
                        &format!("reply {ordinal} {}", "x".repeat(ordinal % 5)),
                        &written.to_rfc3339(),
                    )
                })
                .collect(),
        );
        let messages = shown_entries(&resolved, &transcript);
        let days_written = written(&messages, &[0, 39])
            .lines()
            .filter(|line| line.starts_with("day "))
            .count();
        assert_eq!(days_written, 2);

        for budget in (100..=3_000).step_by(31) {
            let focus = ProtocolFocus {
                conversation_full_ref: None,
                range: MessageRange::single(20),
            };
            let expected = legacy_select_for_budget(&messages, Some(focus.clone()), budget);
            let (actual, _) = select_for_budget_with_work(&messages, Some(focus), Some(budget), "");

            assert_eq!(
                actual.selected, expected,
                "selection differs at budget {budget}"
            );
        }
    }

    /// ` tz=` of an output without times, on test time.
    fn tz_now() -> String {
        tz_atom(output_offset(None, TestClock::start_wall()))
    }

    /// ` tz=` the offset of the UTC time `utc`.
    fn tz_at(utc: &str) -> String {
        tz_atom(output_offset(
            TimeSpan::of_timestamp(utc),
            TestClock::start_wall(),
        ))
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
                subagent: None,
                call: None,
            }],
            None,
            None,
            options(),
        )
        .unwrap();

        assert!(output.starts_with(&format!(
            "protocol agent-read cut=none chars=6000 policy=text omit=none{} hidden-tools=1 hidden-thinking=1\n",
            tz_now()
        )));
        assert!(output.contains("project=pr_"));
        assert!(!output.contains("path="));
        assert!(output.contains("| hello\n| protocol fake\n"));
        assert!(!output.contains("pwd"));
        assert!(!output.contains("secret"));
        assert!(!output.contains("\x1b["));
    }

    #[test]
    fn read_with_tools_shows_launching_calls_prompt() {
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
                subagent: None,
                call: None,
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
                subagent: None,
                call: None,
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
        assert!(output.contains("\nm4 user\n"), "{output}");
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
                subagent: None,
                call: None,
            }],
            None,
            None,
            ProtocolOptions {
                budget: None,
                ..options()
            },
        )
        .unwrap();

        assert!(output.starts_with(&format!(
            "protocol agent-read cut=none chars=none policy=text omit=none{}\n",
            tz_now()
        )));
        assert!(output.contains("\nm1 user\n| one\n"), "{output}");
        assert!(output.contains("\nm2\n| two\n"), "{output}");
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
                subagent: None,
                call: None,
            }],
            None,
            Some(&ReadSlice::Lines(MessageLineRange { start: 2, end: 4 })),
            options(),
        )
        .unwrap();

        assert!(
            output.contains("\nm1 slice=lines=2..4 anchor=ma_"),
            "{output}"
        );
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
                subagent: None,
                call: None,
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
                subagent: None,
                call: None,
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
                subagent: None,
                call: None,
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
                subagent: None,
                call: None,
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

        assert!(output.starts_with(&format!(
            "protocol agent-read cut=body chars=400 policy=text omit=none{}\n",
            tz_now()
        )));
        assert!(output.contains("| >1: needle line 1"));
        assert!(output.chars().count() <= 400);
        assert!(
            output
                .lines()
                .any(|line| is_omitted_record(line) && line.ends_with("..20")),
            "{output}"
        );
    }

    /// m1 holds two lines of text and two calls; m2 holds the first call's
    /// result.
    fn text_and_two_calls() -> AgentTranscript {
        use AgentMessageRole::{Assistant, User};
        let at = |second: u32| format!("2026-10-14T15:41:{second:02}Z");
        let mut announcing = text_message_at(1, Assistant, "one\ntwo", &at(20));
        announcing.parts.extend([
            call_with_id("t", "Bash", "cargo test", source_at(Assistant, &at(25))),
            call_with_id("b", "Bash", "cargo build", source_at(Assistant, &at(26))),
        ]);
        transcript(vec![
            announcing,
            message(2, User, vec![result_for("t", "ok", source(User))]),
        ])
    }

    fn read_slice(transcript: &AgentTranscript, slice: ReadSlice) -> String {
        format_read(
            &[ReadRequest {
                resolved: &resolved("session.jsonl"),
                transcript,
                range: Some(MessageRange::single(1)),
                subagent: None,
                call: None,
            }],
            None,
            Some(&slice),
            ProtocolOptions {
                tools: true,
                ..options()
            },
        )
        .unwrap()
    }

    #[test]
    fn slice_prints_calls_lines_below_calls_record_and_messages_record_without_lines() {
        let output = read_slice(
            &text_and_two_calls(),
            ReadSlice::Match {
                query: "cargo test".to_owned(),
                context: 0,
            },
        );

        let lines = output.lines().skip(3).collect::<Vec<_>>();
        let at = |second: u32| {
            let time = format!("2026-10-14T15:41:{second:02}Z");
            record_time(&time, &time)
        };
        assert!(
            lines[0].starts_with(&format!(
                "m1{} slice=match=cargo%20test context=0 hits=1 anchor=ma_",
                record_time("2026-10-14T15:41:20Z", "2026-10-14T15:41:26Z")
            )),
            "{output}"
        );
        assert_eq!(
            lines[1..],
            [
                format!("m1.1{} result=m2", at(25)),
                "| >3: Bash: cargo test".to_owned()
            ],
            "{output}"
        );
    }

    #[test]
    fn line_slice_numbers_its_lines_as_message_does_across_records() {
        let output = read_slice(
            &text_and_two_calls(),
            ReadSlice::Lines(MessageLineRange { start: 2, end: 4 }),
        );

        let lines = output.lines().skip(3).collect::<Vec<_>>();
        assert!(lines[0].contains(" slice=lines=2..4 "), "{output}");
        assert_eq!(lines[1], "|  2: two", "{output}");
        assert!(lines[2].starts_with("m1.1 "), "{output}");
        assert_eq!(lines[3], "|  3: Bash: cargo test", "{output}");
        assert!(lines[4].starts_with("m1.2 "), "{output}");
        assert_eq!(lines[5], "|  4: Bash: cargo build", "{output}");
        assert_eq!(lines.len(), 6, "{output}");
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
                subagent: None,
                call: None,
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
    fn focused_oversized_message_is_truncated_inside_message() {
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
                subagent: None,
                call: None,
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
        assert!(output.contains("\nm2\n"), "{output}");
        assert!(
            output.contains(&format!(
                "\nm2 omitted lines=1..1\nread ref={}:m2 lines=1..1 tools=false tool-results=false thinking=false subagents=false\n",
                resolved.reference.canonical()
            )),
            "{output}"
        );
    }

    #[test]
    fn multiple_focused_messages_share_hard_budget() {
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
                subagent: None,
                call: None,
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
        assert!(output.contains("\nm2\n"), "{output}");
        assert!(output.contains("\nm3\n"), "{output}");
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
                subagent: None,
                call: None,
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
        let output = at_smallest_fitting_budget(|budget| {
            format_read(
                &[
                    whole_session_request(&first, &first_transcript),
                    whole_session_request(&second, &second_transcript),
                ],
                Some(ProtocolFocus {
                    conversation_full_ref: Some(second.reference.full_ref()),
                    range: MessageRange::single(2),
                }),
                None,
                with_budget(budget),
            )
            .unwrap()
        });

        assert!(output.contains("second two padding padding"), "{output}");
        assert!(!output.contains("first two padding padding"), "{output}");
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
                subagent: None,
                call: None,
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
                subagent: None,
                call: None,
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
        assert!(visible.contains("\nm2\n"), "{visible}");
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
                subagent: None,
                call: None,
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

    /// The address opening each message, call and hidden record of `output`:
    /// `m7`, `m7.2` or `m7..m9`. A truncated message's `omitted` record is
    /// left out.
    fn addresses(output: &str) -> Vec<&str> {
        records(output)
            .into_iter()
            .filter(|record| !is_omitted_record(record))
            .filter_map(|record| record.split_whitespace().next())
            .filter(|address| {
                address
                    .strip_prefix('m')
                    .is_some_and(|rest| rest.starts_with(|first: char| first.is_ascii_digit()))
            })
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
    fn run_of_messages_flags_hide_whole_shows_as_one_hidden_record() {
        let output = read(&session_with_a_tool_run(), None, options());

        let records = records(&output);
        assert_eq!(
            records[0],
            format!(
                "protocol agent-read cut=none chars=6000 policy=text omit=none{} hidden-tools=2 hidden-tool-results=2 hidden-thinking=1",
                tz_at("2026-10-14T15:41:28Z")
            )
        );
        assert_eq!(
            records[2..],
            [
                "m1 user".to_owned(),
                day_record("2026-10-14T15:41:28Z"),
                format!(
                    "m2..m5 hidden{} tools=2 tool-results=2 thinking=1",
                    record_time("2026-10-14T15:41:28Z", "2026-10-14T15:42:05Z")
                ),
                "m6".to_owned(),
            ],
            "{output}"
        );
    }

    #[test]
    fn read_with_every_flag_doesnt_report_hidden_part() {
        let output = read(&session_with_a_tool_run(), None, every_flag());

        assert!(!output.contains(" hidden-"), "{output}");
        assert!(!output.contains(" hidden "), "{output}");
        assert_eq!(
            addresses(&output),
            ["m1", "m2.1", "m3", "m4", "m4.1", "m5", "m6"],
            "{output}"
        );
    }

    #[test]
    fn shown_message_names_its_hidden_calls_and_counts_its_other_hidden_parts() {
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
                .find(|line| *line == "m1" || line.starts_with("m1 "))
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
    fn part_without_text_doesnt_count_as_hidden() {
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
    fn tool_result_has_text_when_its_read_text_is_not_blank() {
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
    fn read_and_outline_with_thinking_dont_show_thinking_part_without_text() {
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

    /// A run of hidden messages does not have a body to truncate. The budget
    /// omits m5, too short to truncate, first; ten characters less omits the
    /// run too.
    #[test]
    fn hidden_record_past_budget_is_omitted_and_continued() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            text_message(1, User, "question"),
            text_message(2, Assistant, "answer"),
            message(
                3,
                Assistant,
                vec![shell_call("Bash", "cargo test", source(Assistant))],
            ),
            message(4, User, vec![tool_result(Some(json!("ok")), source(User))]),
            text_message(5, Assistant, &"y".repeat(TRUNCATION_FLOOR_CHARS)),
        ]);
        let unbudgeted = ProtocolOptions {
            budget: None,
            ..options()
        };
        let without_m5 = read(
            &transcript,
            None,
            with_budget(read(&transcript, None, unbudgeted).chars().count() - 10),
        );
        assert!(without_m5.contains("\nm3..m4 hidden "), "{without_m5}");
        let budget = without_m5.chars().count() - 10;

        let output = read(&transcript, None, with_budget(budget));

        assert!(output.chars().count() <= budget, "{output}");
        assert!(
            output.starts_with(&format!(
                "protocol agent-read cut=tail chars={budget} policy=text omit=m3..m5{}\n",
                tz_now()
            )),
            "{output}"
        );
        assert!(
            output
                .contains(":m3..m5 tools=true tool-results=true thinking=false subagents=false\n"),
            "{output}"
        );
        assert!(output.contains("| answer\n"), "{output}");
        assert!(!output.contains("hidden"), "{output}");
    }

    /// m1 hides nine calls, nine results and nine thinking parts. The header's
    /// totals, not m1's record, decide whether m2 fits. m2 is shorter than
    /// its floor, so the selection keeps it whole or omits it: an estimate
    /// without the totals would keep it, and the over-budget step would then
    /// truncate it under `cut=body`.
    #[test]
    fn message_header_totals_push_past_budget_is_omitted_whole() {
        use AgentMessageRole::Assistant;
        let mut reply = text_message(1, Assistant, "short answer");
        for _ in 0..9 {
            reply.parts.extend([
                shell_call("Bash", "cargo test", source(Assistant)),
                tool_result(Some(json!("ok")), source(Assistant)),
                thinking("weigh it", source(Assistant)),
            ]);
        }
        let transcript = transcript(vec![reply, text_message(2, Assistant, &"x".repeat(150))]);
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
        assert!(!output.contains(OMISSION_MARKER), "{output}");
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
    /// records, decide whether m2 fits. m2 is shorter than its floor, so it
    /// cannot be truncated.
    #[test]
    fn message_window_count_pushes_past_budget_is_omitted_whole() {
        use AgentMessageRole::Assistant;
        let at = "2026-10-14T15:42:00Z";
        let transcript = transcript(vec![
            text_message_at(1, Assistant, "short answer", at),
            text_message_at(2, Assistant, &"x".repeat(150), at),
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
            unbudgeted.starts_with(&format!(
                "protocol agent-read cut=none chars=none policy=text omit=none{} messages-in-window=2\n",
                tz_at(at)
            )),
            "{unbudgeted}"
        );
        assert!(output.chars().count() <= budget, "{output}");
        assert!(output.contains(" omit=m2 "), "{output}");
        assert!(output.contains("| short answer\n"), "{output}");
        assert!(!output.contains(OMISSION_MARKER), "{output}");
    }

    /// Part of a truncated message's `omitted` record.
    const OMISSION_MARKER: &str = " omitted lines=";

    fn is_omitted_record(line: &str) -> bool {
        !line.starts_with('|') && line.contains(OMISSION_MARKER)
    }

    /// The characters each truncated content of `output` keeps: the content
    /// lines between a message's first record and its `omitted` record,
    /// joined by line breaks. The lines below its call records count too,
    /// across a `day` record between them.
    fn kept_chars_of_truncations(output: &str) -> Vec<usize> {
        let mut kept = Vec::new();
        let mut content: Vec<&str> = Vec::new();
        let mut message = None;
        for line in output.lines() {
            if is_omitted_record(line) {
                kept.push(content.join("\n").chars().count());
            } else if let Some(text) = line.strip_prefix("| ").or((line == "|").then_some("")) {
                content.push(text);
            } else if !line.starts_with("day ") {
                let address = line.split_whitespace().next().unwrap_or_default();
                let record_message = address.split('.').next();
                if record_message != message || !address.contains('.') {
                    content.clear();
                }
                message = record_message;
            }
        }
        kept
    }

    #[test]
    fn oversized_message_is_truncated_so_messages_after_it_fit() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            text_message(1, User, "question one"),
            text_message(2, Assistant, &"long output line\n".repeat(200)),
            text_message(3, User, "question two"),
            text_message(4, Assistant, "answer two"),
            text_message(5, User, "question three"),
        ]);
        let budget = 1_000;

        let output = read(&transcript, None, with_budget(budget));

        assert!(output.chars().count() <= budget, "{output}");
        assert!(output.chars().count() > budget - 40, "{output}");
        assert!(
            output.starts_with(&format!(
                "protocol agent-read cut=body chars=1000 policy=text omit=none{}\n",
                tz_now()
            )),
            "{output}"
        );
        assert!(output.contains("| long output line\n"), "{output}");
        assert!(output.contains(OMISSION_MARKER), "{output}");
        assert!(output.contains("| question three\n"), "{output}");
    }

    #[test]
    fn truncated_body_keeps_its_start_and_messages_that_dont_fit_are_omitted() {
        use AgentMessageRole::{Assistant, User};
        let mut messages = vec![
            text_message(1, User, "question"),
            text_message(2, Assistant, &"y".repeat(3_000)),
        ];
        messages.extend((3..=30).map(|ordinal| text_message(ordinal, User, "a short follow-up")));
        let transcript = transcript(messages);

        let output = read(&transcript, None, with_budget(1_200));

        assert!(output.chars().count() <= 1_200, "{output}");
        assert!(
            output.starts_with("protocol agent-read cut=body+tail "),
            "{output}"
        );
        assert!(
            kept_chars_of_truncations(&output)[0] >= TRUNCATION_FLOOR_CHARS,
            "{output}"
        );
        assert!(output.contains("\nm3 user\n"), "{output}");
        assert!(output.contains("continue read refs="), "{output}");
        assert!(!output.contains("\nm30 "), "{output}");
    }

    #[test]
    fn truncated_body_of_many_short_lines_keeps_its_floor_next_to_one_line_body() {
        use AgentMessageRole::Assistant;
        let transcript = transcript(vec![
            text_message(1, Assistant, &"a\n".repeat(1_000)),
            text_message(2, Assistant, &"b".repeat(3_000)),
        ]);

        let output = read(&transcript, None, with_budget(1_100));

        let kept = kept_chars_of_truncations(&output);
        assert!(output.chars().count() <= 1_100, "{output}");
        assert_eq!(kept.len(), 2, "{output}");
        // A truncation ending on a line break shows one character fewer.
        assert!(kept[0] + 1 >= TRUNCATION_FLOOR_CHARS, "{output}");
        assert!(kept[1] >= TRUNCATION_FLOOR_CHARS, "{output}");
    }

    /// Both bodies may be truncated. m1, the shorter, fits its share whole,
    /// and m2 receives the room m1 leaves.
    #[test]
    fn body_under_its_share_shows_whole_and_leaves_rest_to_longer_one() {
        use AgentMessageRole::Assistant;
        let resolved = resolved("session.jsonl");
        let transcript = transcript(vec![
            text_message(1, Assistant, &"c".repeat(400)),
            text_message(2, Assistant, &"d".repeat(3_000)),
        ]);
        let mut entries = shown_entries(&resolved, &transcript);
        let selected = [0, 1];
        let render = |entries: &[ReadEntry<'_>], selected: &[usize], _cut: &str, _| {
            written(entries, selected)
        };
        for entry in &mut entries {
            if let ReadEntry::Message { rendered, .. } = entry {
                rendered.truncation = Some(Truncation::empty());
            }
        }
        let room = 1_200;
        let empty_content = "|\n".len();
        let budget = render(&entries, &selected, "", RecordSize::Full)
            .chars()
            .count()
            + room
            - 2 * empty_content;

        fit_bodies(
            &mut entries,
            &selected,
            &BTreeSet::from([0, 1]),
            budget,
            &render,
            "",
        );

        let is_truncated = |entry: &ReadEntry<'_>| matches!(entry, ReadEntry::Message { rendered, .. } if rendered.truncation.is_some());
        let output = render(&entries, &selected, "", RecordSize::Full);
        assert!(!is_truncated(&entries[0]), "{output}");
        assert!(is_truncated(&entries[1]), "{output}");
        assert!(output.chars().count() <= budget, "{output}");
        assert!(output.chars().count() > budget - 10, "{output}");
    }

    #[test]
    fn two_oversized_messages_of_one_length_share_room_equally() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            text_message(1, Assistant, &"a".repeat(3_000)),
            text_message(2, User, "a short question"),
            text_message(3, Assistant, &"b".repeat(3_000)),
        ]);

        let output = read(&transcript, None, with_budget(1_500));

        let kept = kept_chars_of_truncations(&output);
        assert!(output.chars().count() <= 1_500, "{output}");
        assert!(output.contains("| a short question\n"), "{output}");
        assert_eq!(kept.len(), 2, "{output}");
        assert!(kept[0].abs_diff(kept[1]) <= 1, "{output}");
    }

    #[test]
    fn focused_read_omits_oversized_neighbour_whole() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            text_message(1, User, "the focused question"),
            text_message(2, Assistant, &"z".repeat(3_000)),
            text_message(3, User, "after"),
        ]);

        let output = format_read(
            &[ReadRequest {
                resolved: &resolved("session.jsonl"),
                transcript: &transcript,
                range: None,
                subagent: None,
                call: None,
            }],
            Some(ProtocolFocus {
                conversation_full_ref: None,
                range: MessageRange::single(1),
            }),
            None,
            with_budget(600),
        )
        .unwrap();

        assert!(output.contains(" omit=m2..m3"), "{output}");
        assert!(output.contains("| the focused question\n"), "{output}");
        assert!(!output.contains(OMISSION_MARKER), "{output}");
    }

    /// m3 has no time and m6 falls after the window: both are outside it.
    #[test]
    fn windowed_read_reports_only_hidden_parts_inside_window() {
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
        let at = |utc| record_time(utc, utc);
        assert!(
            records[0].ends_with(&format!(
                " omit=none{} messages-in-window=4 hidden-tools=2",
                tz_at("2026-10-14T15:41:00Z")
            )),
            "{output}"
        );
        assert_eq!(
            records[2..],
            [
                day_record("2026-10-14T15:41:00Z"),
                format!("m1 user{}", at("2026-10-14T15:41:00Z")),
                format!("m2 hidden{} tools=1", at("2026-10-14T15:41:10Z")),
                format!("m4 hidden{} tools=1", at("2026-10-14T15:41:20Z")),
                format!("m5{}", at("2026-10-14T15:41:30Z")),
            ],
            "{output}"
        );
    }

    #[test]
    fn focus_inside_hidden_run_keeps_run() {
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
                subagent: None,
                call: None,
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
            output.starts_with(&format!(
                "protocol agent-read cut=head+focus+tail chars=400 policy=text omit=m1,m4{} hidden-tools=1 hidden-tool-results=1\n",
                tz_now()
            )),
            "{output}"
        );
        assert!(
            output.contains("\nm2..m3 hidden tools=1 tool-results=1\n"),
            "{output}"
        );
    }

    #[test]
    fn hidden_subagent_message_counts_once_and_its_calls_stay_out_of_tools_count() {
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
            default.starts_with(&format!(
                "protocol agent-read cut=none chars=6000 policy=text omit=none{} hidden-subagents=1\n",
                tz_now()
            )),
            "{default}"
        );
        assert!(default.contains("\nm2 hidden subagents=1\n"), "{default}");
        assert!(
            with_subagents.starts_with(&format!(
                "protocol agent-read cut=none chars=6000 policy=text+subagents omit=none{} hidden-tools=1\n",
                tz_now()
            )),
            "{with_subagents}"
        );
    }

    #[test]
    fn one_message_read_shows_every_line_of_each_command() {
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
            one.contains("\nm1.1\n| Bash: cd src\n|   cargo test\nm1.2\n| Bash: cargo clippy\n"),
            "{one}"
        );
        assert!(
            several.contains("\nm1.1\n| Bash: cd src (+1 line)\n"),
            "{several}"
        );
    }

    fn read_tails(requests: &[ReadRequest<'_>], tail: usize, options: ProtocolOptions) -> String {
        let tail = NonZeroUsize::new(tail).unwrap();
        format_read_with_warnings(
            requests,
            ReadPriority::Tail(tail),
            None,
            options,
            &[],
            TestClock::start_wall(),
        )
        .unwrap()
    }

    fn read_tail(
        transcript: &AgentTranscript,
        range: Option<MessageRange>,
        tail: usize,
        options: ProtocolOptions,
    ) -> String {
        read_tails(
            &[ReadRequest {
                resolved: &resolved("session.jsonl"),
                transcript,
                range,
                subagent: None,
                call: None,
            }],
            tail,
            options,
        )
    }

    /// Replies m1 to m`count`, each at least `chars` long, one a minute from
    /// 15:41 UTC on 2026-10-14.
    fn timed_replies(count: usize, chars: usize) -> AgentTranscript {
        transcript(
            (1..=count)
                .map(|ordinal| {
                    let text = format!("reply {ordinal} ");
                    text_message_at(
                        ordinal,
                        AgentMessageRole::Assistant,
                        &text.repeat(chars.div_ceil(text.len())),
                        &format!("2026-10-14T15:{:02}:00Z", 40 + ordinal),
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn tail_doesnt_count_hidden_messages() {
        let session = session_with_a_tool_run();

        let last = read_tail(&session, None, 1, options());
        let two = read_tail(&session, None, 2, options());
        let every_flag = read_tail(&session, None, 2, every_flag());

        assert_eq!(addresses(&last), ["m6"], "{last}");
        assert_eq!(addresses(&two), ["m1", "m2..m5", "m6"], "{two}");
        assert!(two.contains("\nm2..m5 hidden "), "{two}");
        assert_eq!(addresses(&every_flag), ["m5", "m6"], "{every_flag}");
    }

    #[test]
    fn tail_holds_hidden_messages_after_its_last_shown_message() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            text_message(1, User, "Run the tests"),
            text_message(2, Assistant, "Running them now."),
            message(
                3,
                Assistant,
                vec![shell_call("Bash", "cargo test", source(Assistant))],
            ),
            message(4, User, vec![tool_result(Some(json!("ok")), source(User))]),
        ]);

        let output = read_tail(&transcript, None, 1, options());

        assert_eq!(addresses(&output), ["m2", "m3..m4"], "{output}");
        assert!(
            output.contains("\nm3..m4 hidden tools=1 tool-results=1\n"),
            "{output}"
        );
    }

    #[test]
    fn tail_longer_than_session_holds_whole_session() {
        let session = session_with_a_tool_run();

        assert_eq!(
            read_tail(&session, None, 50, options()),
            read(&session, None, options())
        );
    }

    #[test]
    fn tail_of_range_holds_last_messages_of_range() {
        let output = read_tail(
            &timed_replies(6, 10),
            Some(MessageRange { start: 2, end: 4 }),
            2,
            options(),
        );

        assert_eq!(addresses(&output), ["m3", "m4"], "{output}");
    }

    /// m6 at 15:46 falls after the window; the header counts the window's
    /// five messages, not the tail's two.
    #[test]
    fn tail_inside_window_holds_last_messages_inside_it() {
        let output = read_tail(
            &timed_replies(6, 10),
            None,
            2,
            ProtocolOptions {
                window: window_from_15_40_to_15_45(),
                ..options()
            },
        );

        assert_eq!(addresses(&output), ["m4", "m5"], "{output}");
        assert!(
            output
                .lines()
                .next()
                .unwrap()
                .contains(" messages-in-window=5"),
            "{output}"
        );
    }

    #[test]
    fn budget_keeps_end_of_tail_and_continues_with_its_start() {
        let replies = timed_replies(5, 200);
        let unbudgeted = read_tail(
            &replies,
            None,
            5,
            ProtocolOptions {
                budget: None,
                ..options()
            },
        );
        let budget = unbudgeted.chars().count() * 3 / 5;

        let output = read_tail(
            &replies,
            None,
            5,
            ProtocolOptions {
                budget: Some(budget),
                ..options()
            },
        );

        assert!(output.chars().count() <= budget, "{output}");
        assert!(
            output.starts_with(&format!(
                "protocol agent-read cut=head chars={budget} policy=text omit=m1..m3{}\n",
                tz_at("2026-10-14T15:44:00Z")
            )),
            "{output}"
        );
        assert_eq!(addresses(&output), ["m4", "m5"], "{output}");
        assert!(
            output.contains(
                ":m1..m3 tools=false tool-results=false thinking=false subagents=false\n"
            ),
            "{output}"
        );
    }

    #[test]
    fn tail_truncates_oversized_last_message_to_fit_ones_before_it() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            text_message(1, User, "run the tests"),
            text_message(2, User, "and show the output"),
            text_message(3, Assistant, &"test output line\n".repeat(200)),
        ]);

        let output = read_tail(&transcript, None, 3, with_budget(1_000));

        assert!(output.chars().count() <= 1_000, "{output}");
        assert!(
            output.starts_with(&format!(
                "protocol agent-read cut=body chars=1000 policy=text omit=none{}\n",
                tz_now()
            )),
            "{output}"
        );
        assert!(output.contains("| run the tests\n"), "{output}");
        assert!(output.contains("| and show the output\n"), "{output}");
        assert!(output.contains(OMISSION_MARKER), "{output}");
    }

    /// At the smallest budget that keeps a message, m2 truncated to its
    /// floor leaves no room for m1. m1 alone would fit a smaller budget.
    #[test]
    fn tight_budget_keeps_tails_last_message_rather_than_earlier_one() {
        use AgentMessageRole::Assistant;
        let transcript = transcript(vec![
            text_message(1, Assistant, "short reply"),
            text_message(2, Assistant, &"x".repeat(500)),
        ]);

        let (budget, output) = (200..1_000)
            .map(|budget| (budget, read_tail(&transcript, None, 2, with_budget(budget))))
            .find(|(_, output)| !addresses(output).is_empty())
            .unwrap();

        assert!(output.chars().count() <= budget, "{output}");
        assert_eq!(addresses(&output), ["m2"], "{output}");
        assert!(output.contains(OMISSION_MARKER), "{output}");
        assert!(!output.contains("short reply"), "{output}");
    }

    #[test]
    fn tail_holds_end_of_each_ref() {
        let (first, second) = (timed_replies(3, 10), timed_replies(3, 10));
        let (first_ref, second_ref) = (resolved("first.jsonl"), resolved("second.jsonl"));
        let requests = [
            whole_session_request(&first_ref, &first),
            whole_session_request(&second_ref, &second),
        ];

        let output = read_tails(&requests, 1, options());

        let records = records(&output);
        let third_reply = format!(
            "m3{}",
            record_time("2026-10-14T15:43:00Z", "2026-10-14T15:43:00Z")
        );
        assert!(records[1].starts_with("conversation "), "{output}");
        assert_eq!(records[2], day_record("2026-10-14T15:43:00Z"), "{output}");
        assert_eq!(records[3], third_reply, "{output}");
        assert!(records[4].starts_with("conversation "), "{output}");
        assert_eq!(records[5], day_record("2026-10-14T15:43:00Z"), "{output}");
        assert_eq!(records[6], third_reply, "{output}");
        assert_eq!(records.len(), 7, "{output}");
    }

    #[test]
    fn budget_takes_from_each_refs_end_in_turn() {
        let (first, second) = (timed_replies(3, 200), timed_replies(3, 200));
        let (first_ref, second_ref) = (resolved("first.jsonl"), resolved("second.jsonl"));
        let requests = [
            whole_session_request(&first_ref, &first),
            whole_session_request(&second_ref, &second),
        ];
        let unbudgeted = read_tails(
            &requests,
            3,
            ProtocolOptions {
                budget: None,
                ..options()
            },
        );
        let budget = unbudgeted.chars().count() * 2 / 3;

        let output = read_tails(
            &requests,
            3,
            ProtocolOptions {
                budget: Some(budget),
                ..options()
            },
        );

        assert!(output.chars().count() <= budget, "{output}");
        let mut messages_per_ref: Vec<Vec<&str>> = Vec::new();
        for record in records(&output) {
            if record.starts_with("conversation ") {
                messages_per_ref.push(Vec::new());
            } else if let Some(&address) = addresses(record).first() {
                messages_per_ref.last_mut().unwrap().push(address);
            }
        }
        assert_eq!(messages_per_ref.len(), 2, "{output}");
        for messages in &messages_per_ref {
            assert_eq!(messages.last(), Some(&"m3"), "{output}");
        }
        assert!(
            messages_per_ref[0]
                .len()
                .abs_diff(messages_per_ref[1].len())
                <= 1,
            "{output}"
        );
        assert!(output.contains("continue read "), "{output}");
    }

    #[test]
    fn tail_budget_keeps_each_requests_last_entry_before_any_second_last() {
        let order = TailOrder::new(&[3, 3]);

        assert_eq!(order.kept_first(), [5, 2, 4, 1, 3, 0]);
        assert_eq!(order.position_kept_last(&[1, 2, 4, 5]), Some(0));
        assert_eq!(order.position_kept_last(&[]), None);
    }

    /// The first ref holds m2..m5 of [`session_with_a_tool_run`]: default
    /// flags hide them. The second shows m1 and m6.
    #[test]
    fn each_conversation_whose_messages_are_all_hidden_ends_with_its_own_read_record() {
        let transcript = session_with_a_tool_run();
        let (first_ref, second_ref) = (resolved("first.jsonl"), resolved("second.jsonl"));
        let requests = [
            ReadRequest {
                range: Some(MessageRange { start: 2, end: 5 }),
                ..whole_session_request(&first_ref, &transcript)
            },
            whole_session_request(&second_ref, &transcript),
        ];

        let output = format_read(&requests, None, None, options()).unwrap();

        let records = records(&output);
        let first_conversation_records = records
            .iter()
            .skip(1)
            .take_while(|record| !record.contains(&second_ref.reference.canonical()))
            .collect::<Vec<_>>();
        assert_eq!(
            first_conversation_records.last().unwrap().to_string(),
            format!(
                "read ref={}:m2..m5 tools=true tool-results=true thinking=true subagents=false",
                first_ref.reference.canonical()
            ),
            "{output}"
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| record.starts_with("read "))
                .count(),
            1,
            "{output}"
        );
    }

    /// m2 to m5 hold calls, results and thinking alone; the read leaves m4
    /// out.
    #[test]
    fn read_of_hidden_messages_with_gap_names_each_run_in_its_read_record() {
        let transcript = session_with_a_tool_run();
        let resolved = resolved("session.jsonl");
        let messages = |start, end| ReadRequest {
            range: Some(MessageRange { start, end }),
            ..whole_session_request(&resolved, &transcript)
        };

        let output = format_read(&[messages(2, 3), messages(5, 5)], None, None, options()).unwrap();

        let reference = resolved.reference.canonical();
        assert!(
            output.ends_with(&format!(
                "\nread refs={reference}:m2..m3;{reference}:m5 tools=true tool-results=true thinking=false subagents=false\n"
            )),
            "{output}"
        );
    }

    /// The output of `read_with_budget` at the smallest budget it fits
    /// without being cut short.
    fn at_smallest_fitting_budget(read_with_budget: impl Fn(usize) -> String) -> String {
        (1..4_000)
            .map(read_with_budget)
            .find(|output| output.ends_with('\n'))
            .unwrap()
    }

    fn focus_on(message: usize) -> Option<ProtocolFocus> {
        Some(ProtocolFocus {
            conversation_full_ref: None,
            range: MessageRange::single(message),
        })
    }

    /// The budget keeps the focus, m1, and omits the hidden run m2..m5 and
    /// m6.
    #[test]
    fn continue_read_record_names_flags_its_omitted_messages_need() {
        let transcript = session_with_a_tool_run();
        let resolved = resolved("session.jsonl");
        let request = whole_session_request(&resolved, &transcript);

        let output = at_smallest_fitting_budget(|budget| {
            let requests = std::slice::from_ref(&request);
            format_read(requests, focus_on(1), None, with_budget(budget)).unwrap()
        });

        assert!(output.contains("\nm1 user\n"), "{output}");
        assert!(
            output.ends_with(&format!(
                "\ncontinue read refs={}:m2..m6 tools=true tool-results=true thinking=true subagents=false\n",
                resolved.reference.canonical()
            )),
            "{output}"
        );
    }

    /// The read of the scout keeps the focus, its launch m2, and omits its
    /// text m3, its call and result m4..m5, and its report m7; m6 is not
    /// the scout's. Longer texts make omitting them shorter than showing
    /// them.
    #[test]
    fn sub_agent_reads_continue_read_names_its_omitted_messages_alone_with_subagents() {
        let mut transcript = session_with_a_scout();
        for index in [2, 6] {
            if let AgentMessagePart::Text { text, .. } = &mut transcript.messages[index].parts[0] {
                *text = text.repeat(8);
            }
        }
        let resolved = resolved("session.jsonl");
        let request = ReadRequest {
            subagent: Some(1),
            ..whole_session_request(&resolved, &transcript)
        };

        let output = at_smallest_fitting_budget(|budget| {
            let requests = std::slice::from_ref(&request);
            format_read(requests, focus_on(2), None, with_budget(budget)).unwrap()
        });

        let reference = resolved.reference.canonical();
        assert!(
            output.contains("\nm2 hidden launches=s1 tools=1\n"),
            "{output}"
        );
        assert!(
            output.ends_with(&format!(
                "\ncontinue read refs={reference}:m3..m5;{reference}:m7 tools=true tool-results=true thinking=false subagents=true\n"
            )),
            "{output}"
        );
    }

    fn whole_session_request<'a>(
        resolved: &'a ResolvedConversation,
        transcript: &'a AgentTranscript,
    ) -> ReadRequest<'a> {
        ReadRequest {
            resolved,
            transcript,
            range: None,
            subagent: None,
            call: None,
        }
    }

    #[test]
    fn short_outline_emits_one_line_per_message() {
        let resolved = resolved("session.jsonl");
        let transcript = transcript(vec![
            text_message(1, AgentMessageRole::User, "one"),
            text_message(2, AgentMessageRole::Assistant, "two"),
        ]);
        let output = format_outline(&resolved, &transcript, options());

        assert!(output.contains("\nm1 user chars=3 | one\n"), "{output}");
        assert!(output.contains("\nm2 chars=3 | two\n"), "{output}");
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

        assert!(output.contains("\nseg m1..m10 chars=91 | message 1 / message 10\n"));
        assert!(output.contains("\nseg m11..m20 chars=100 | message 11 / message 20\n"));
        assert!(output.contains("\nseg m21..m21 chars=10 | message 21 / message 21\n"));
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
    fn outline_segment_shows_first_and_last_time_of_its_messages() {
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
    fn long_outline_spans_session_at_each_budget() {
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
    fn long_outline_uses_smallest_segment_size_that_fits() {
        let resolved = resolved("session.jsonl");
        let transcript = long_session(2_000);
        let budget = 6_000;
        let output = format_outline(&resolved, &transcript, with_budget(budget));
        let messages = whole_session_outline(&resolved, &transcript, with_budget(budget));
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
    fn budget_too_small_for_one_segment_continues_with_whole_session() {
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
                "continue read refs={}:m1..m2000 tools=false tool-results=false thinking=false subagents=false\n",
                resolved.reference.canonical()
            )),
            "{output}"
        );
        assert!(output.chars().count() <= 400, "{output}");
    }

    fn skipped_warning() -> AgentWarning {
        AgentWarning::skipped(Some("ch_000000000000"), "padding ".repeat(60))
    }

    fn whole_session_outline<'a>(
        resolved: &'a ResolvedConversation,
        transcript: &'a AgentTranscript,
        options: ProtocolOptions,
    ) -> OutlineMessages<'a> {
        let index = TranscriptIndex::of(transcript);
        OutlineMessages::new(
            &whole_session_request(resolved, transcript),
            transcript.messages.iter().collect(),
            &index,
            options,
            TestClock::start_wall(),
        )
    }

    /// [`format_outline_with_warnings`] of the whole of `transcript`.
    fn outline_with_warnings(
        resolved: &ResolvedConversation,
        transcript: &AgentTranscript,
        options: ProtocolOptions,
        warnings: &[AgentWarning],
    ) -> String {
        format_outline_with_warnings(
            &whole_session_request(resolved, transcript),
            options,
            warnings,
            TestClock::start_wall(),
        )
        .unwrap()
    }

    /// The budget holds the warning and the segment's length again, less
    /// than a header and a conversation record.
    #[test]
    fn warning_beside_segment_too_long_for_budget_prints_whole_in_its_place() {
        let resolved = resolved("session.jsonl");
        let transcript = long_session(2_000);
        let one_segment = whole_session_outline(&resolved, &transcript, options())
            .records(2_000)
            .remove(0)
            .line;
        let warning = crate::agent::diagnostic::format_warning(&skipped_warning());
        let budget = one_segment.chars().count() + warning.chars().count() + 150;

        let output = outline_with_warnings(
            &resolved,
            &transcript,
            with_budget(budget),
            &[skipped_warning()],
        );

        let header = output.lines().next().unwrap();
        assert!(output.chars().count() <= budget, "{output}");
        assert!(
            header.starts_with("protocol agent-outline cut=tail "),
            "{output}"
        );
        assert!(
            header.ends_with(" omitted-records=1 warnings=1 warning-records=1"),
            "{output}"
        );
        assert!(one_segment.starts_with("seg m1..m2000 "), "{one_segment}");
        assert!(!output.contains(&one_segment), "{output}");
        assert!(output.ends_with(&warning), "{output}");
    }

    /// Below the smallest budget a read fits, the read is cut short and
    /// reports `budget-too-small`.
    #[test]
    fn every_budget_read_fits_holds_its_warning_whole_and_smallest_omits_every_message() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            text_message(1, User, &"x".repeat(300)),
            text_message(2, Assistant, "answer"),
        ]);
        let warning = crate::agent::diagnostic::format_warning(&skipped_warning());
        let read_with_warning = |budget| {
            format_read_with_warnings(
                &[whole_session_request(
                    &resolved("session.jsonl"),
                    &transcript,
                )],
                ReadPriority::Start,
                None,
                with_budget(budget),
                &[skipped_warning()],
                TestClock::start_wall(),
            )
            .unwrap()
        };

        let fitting = (1..1_200)
            .map(|budget| (budget, read_with_warning(budget)))
            .filter(|(_, output)| output.ends_with('\n'))
            .collect::<Vec<_>>();

        let (smallest, smallest_output) = fitting.first().unwrap();
        assert!(*smallest > warning.chars().count(), "{smallest_output}");
        assert!(!smallest_output.contains("| x"), "{smallest_output}");
        assert!(
            smallest_output.contains("\ncontinue read "),
            "{smallest_output}"
        );
        for (budget, output) in &fitting {
            assert!(output.chars().count() <= *budget, "{budget}: {output}");
            assert!(output.ends_with(&warning), "{budget}: {output}");
        }
    }

    #[test]
    fn roomy_outline_shows_its_warning() {
        let resolved = resolved("session.jsonl");
        let transcript = long_session(2_000);

        let output = outline_with_warnings(
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

    /// Two warnings of one kind print as one record.
    #[test]
    fn truncated_outline_counts_its_warning_records_as_other_headers_do() {
        let resolved = resolved("session.jsonl");
        let warnings = [
            AgentWarning::skipped(Some("ch_000000000001"), "empty transcript"),
            AgentWarning::skipped(Some("ch_000000000002"), "empty transcript"),
        ];

        let output =
            outline_with_warnings(&resolved, &long_session(2_000), with_budget(400), &warnings);

        let header = output.lines().next().unwrap();
        assert!(
            header.starts_with("protocol agent-outline cut=tail "),
            "{output}"
        );
        assert!(
            header.ends_with(" warnings=2 warning-records=1"),
            "{output}"
        );
    }

    fn call_with_id(
        id: &str,
        name: &str,
        command: &str,
        source: AgentPartSource,
    ) -> AgentMessagePart {
        AgentMessagePart::ToolUse {
            id: id.to_owned(),
            name: name.to_owned(),
            tool: Tool::Shell,
            input: json!({"command": command}),
            source,
        }
    }

    fn result_for(id: &str, content: &str, source: AgentPartSource) -> AgentMessagePart {
        AgentMessagePart::ToolResult {
            tool_use_id: id.to_owned(),
            content: Some(json!(content)),
            source,
        }
    }

    fn records_after_conversation(output: &str) -> Vec<&str> {
        records(output).split_off(2)
    }

    #[test]
    fn record_names_what_produced_its_message_when_that_is_not_agent() {
        use AgentMessageRole::{Assistant, User};
        let mut report = text_message(4, User, "Agent \"review\" handed back its report");
        report.is_report = true;
        let transcript = transcript(vec![
            text_message(1, User, "Run the tests"),
            message(
                2,
                Assistant,
                vec![call_with_id("a", "Bash", "cargo test", source(Assistant))],
            ),
            message(3, User, vec![result_for("a", "ok", source(User))]),
            report,
            message(
                5,
                User,
                vec![result_for("gone", "stale output", source(User))],
            ),
            text_message(6, Assistant, "All pass."),
        ]);

        let output = read(&transcript, None, every_flag());

        assert_eq!(
            records_after_conversation(&output),
            [
                "m1 user",
                "m2.1 result=m3",
                "m3 call=m2.1",
                "m4 report",
                "m5 result",
                "m6"
            ],
            "{output}"
        );
    }

    /// A shell command the user ran: the call and its result in one user
    /// message.
    #[test]
    fn users_shell_command_read_with_tools_shows_user() {
        use AgentMessageRole::User;
        let transcript = transcript(vec![message(
            1,
            User,
            vec![
                call_with_id("a", "bash", "true", source(User)),
                result_for("a", "done", source(User)),
            ],
        )]);

        let output = read(
            &transcript,
            None,
            ProtocolOptions {
                tools: true,
                ..options()
            },
        );

        assert_eq!(
            records_after_conversation(&output),
            ["m1 user hidden-tool-results=1", "m1.1"],
            "{output}"
        );
    }

    fn subagent_message(
        ordinal: usize,
        role: AgentMessageRole,
        parts: Vec<AgentMessagePart>,
        label: &str,
        launch_call: &str,
    ) -> AgentMessage {
        use crate::agent::transcript::SubagentOrigin;
        use crate::log_entry::SubagentIdentity;
        AgentMessage {
            parent_tool_use_id: Some(label.to_owned()),
            subagent: Some(std::sync::Arc::new(SubagentOrigin {
                label: label.to_owned(),
                transcript: None,
                launch_call: Some(launch_call.to_owned()),
                identity: SubagentIdentity {
                    agent_id: Some(format!("{label}-id")),
                    ..SubagentIdentity::default()
                },
            })),
            ..message(ordinal, role, parts)
        }
    }

    fn agent_call(id: &str, description: &str, agent_type: &str) -> AgentMessagePart {
        AgentMessagePart::ToolUse {
            id: id.to_owned(),
            name: "Agent".to_owned(),
            tool: Tool::Agent,
            input: json!({"description": description, "subagent_type": agent_type}),
            source: source(AgentMessageRole::Assistant),
        }
    }

    /// m2 launches the sub-agent `scout`. Its messages are m3 to m5; m6 sends
    /// to it, and m7 carries its report.
    fn session_with_a_scout() -> AgentTranscript {
        use crate::history::task_notification::ReportSender;
        use AgentMessageRole::{Assistant, User};
        let mut report = text_message(7, User, "Agent \"scout\" handed back its report");
        report.is_report = true;
        report.report_has_text = true;
        report.report_sender = ReportSender {
            task_id: None,
            launch_call: Some("toolu_scout".to_owned()),
        };
        transcript(vec![
            text_message(1, User, "Survey the repo"),
            message(
                2,
                Assistant,
                vec![agent_call("toolu_scout", "scout", "Explore")],
            ),
            subagent_message(
                3,
                Assistant,
                vec![AgentMessagePart::Text {
                    text: "Reading the tree.".to_owned(),
                    source: source(Assistant),
                }],
                "Explore",
                "toolu_scout",
            ),
            subagent_message(
                4,
                Assistant,
                vec![call_with_id("toolu_ls", "Bash", "ls", source(Assistant))],
                "Explore",
                "toolu_scout",
            ),
            subagent_message(
                5,
                User,
                vec![result_for("toolu_ls", "a.rs", source(User))],
                "Explore",
                "toolu_scout",
            ),
            message(
                6,
                Assistant,
                vec![AgentMessagePart::ToolUse {
                    id: "toolu_message".to_owned(),
                    name: "SendMessage".to_owned(),
                    tool: Tool::AgentMessage,
                    input: json!({"recipient": "Explore-id", "message": "status?"}),
                    source: source(Assistant),
                }],
            ),
            report,
            text_message(8, Assistant, "Done."),
        ])
    }

    const SCOUT_LEGEND: &str = "subagent s1 type=Explore launch=m2 span=m3..m5 report=m7 : scout";

    #[test]
    fn read_names_each_sub_agent_in_legend_and_in_position_2_of_its_records() {
        let output = read(&session_with_a_scout(), None, every_flag());

        assert_eq!(
            records_after_conversation(&output),
            [
                SCOUT_LEGEND,
                "m1 user",
                "m2.1 launches=s1",
                "m3 s1",
                "m4.1 s1 result=m5",
                "m5 s1 call=m4.1",
                "m6.1 target=s1",
                "m7 report from=s1",
                "m8",
            ],
            "{output}"
        );
    }

    #[test]
    fn sub_agent_keeps_its_handle_in_read_of_part_of_session() {
        let output = read(
            &session_with_a_scout(),
            Some(MessageRange { start: 4, end: 8 }),
            every_flag(),
        );

        assert!(output.contains(&format!("\n{SCOUT_LEGEND}\n")), "{output}");
        assert!(output.contains("\nm4.1 s1 result=m5\n"), "{output}");
    }

    fn read_subagent(transcript: &AgentTranscript, handle: Handle) -> Result<String> {
        format_read(
            &[ReadRequest {
                resolved: &resolved("session.jsonl"),
                transcript,
                range: None,
                subagent: Some(handle),
                call: None,
            }],
            None,
            None,
            every_flag(),
        )
    }

    #[test]
    fn read_of_sub_agent_shows_its_launch_its_messages_and_its_report() {
        let output = read_subagent(&session_with_a_scout(), 1).unwrap();

        assert_eq!(
            records_after_conversation(&output),
            [
                SCOUT_LEGEND,
                "m2.1 launches=s1",
                "m3 s1",
                "m4.1 s1 result=m5",
                "m5 s1 call=m4.1",
                "m7 report from=s1",
            ],
            "{output}"
        );
    }

    #[test]
    fn read_of_sub_agent_shows_its_messages_without_subagents_flag() {
        let output = format_read(
            &[ReadRequest {
                resolved: &resolved("session.jsonl"),
                transcript: &session_with_a_scout(),
                range: None,
                subagent: Some(1),
                call: None,
            }],
            None,
            None,
            options(),
        )
        .unwrap();

        assert!(output.contains(" policy=text+subagents "), "{output}");
        assert!(
            output.contains("\nm3 s1\n| Reading the tree.\n"),
            "{output}"
        );
    }

    #[test]
    fn read_of_unknown_sub_agent_is_not_found() {
        let error = read_subagent(&session_with_a_scout(), 2).unwrap_err();

        assert!(
            matches!(
                &error,
                crate::error::AppError::Agent(AgentError {
                    kind: AgentErrorKind::NotFound,
                    ..
                })
            ),
            "{error}"
        );
    }

    /// Without `--tools`, m2's launching call and m6's message are hidden.
    #[test]
    fn hidden_run_names_sub_agents_its_calls_launch_and_send_to() {
        let output = read(&session_with_a_scout(), None, options());

        assert!(
            output.contains("\nm2 hidden launches=s1 tools=1\n"),
            "{output}"
        );
        assert!(
            output.contains("\nm6 hidden target=s1 tools=1\n"),
            "{output}"
        );
    }

    /// Without `--tools` and `--subagents`, m2 to m6 are hidden: the
    /// session's m2, the scout's m3 to m5, and the session's m6.
    #[test]
    fn hidden_run_ends_where_owner_of_its_messages_changes() {
        let output = read(&session_with_a_scout(), None, options());

        assert_eq!(
            records_after_conversation(&output),
            [
                SCOUT_LEGEND,
                "m1 user",
                "m2 hidden launches=s1 tools=1",
                "m3..m5 s1 hidden subagents=3",
                "m6 hidden target=s1 tools=1",
                "m7 report from=s1",
                "m8",
            ],
            "{output}"
        );
    }

    /// m3 to m5 are the sub-agent's text, call and result.
    #[test]
    fn read_whose_messages_are_all_hidden_ends_with_read_that_shows_them() {
        let output = read(
            &session_with_a_scout(),
            Some(MessageRange { start: 3, end: 5 }),
            options(),
        );

        assert!(
            output.ends_with(&format!(
                "\nm3..m5 s1 hidden subagents=3\nread ref={}:m3..m5 tools=true tool-results=true thinking=false subagents=true\n",
                resolved("session.jsonl").reference.canonical()
            )),
            "{output}"
        );
    }

    /// The sub-agent `scout` launches `checker` from its own message, m4.
    #[test]
    fn sub_agent_launched_by_another_names_it_in_its_legend() {
        use AgentMessageRole::Assistant;
        let mut transcript = session_with_a_scout();
        transcript.messages[3]
            .parts
            .push(agent_call("toolu_checker", "checker", "Plan"));
        transcript.messages.push(subagent_message(
            9,
            Assistant,
            vec![AgentMessagePart::Text {
                text: "Checking.".to_owned(),
                source: source(Assistant),
            }],
            "Plan",
            "toolu_checker",
        ));

        let output = read(&transcript, None, every_flag());

        assert!(
            output.contains("\nsubagent s2 type=Plan launch=m4 span=m9 in=s1 : checker\n"),
            "{output}"
        );
        assert!(output.contains("\nm4.2 s1 launches=s2\n"), "{output}");
        assert!(output.contains("\nm9 s2\n"), "{output}");
    }

    /// m2 launches `scout`, in the background when `in_background`; m3 is
    /// its reply, and m4 is the result of m2's call: in the foreground, the
    /// reply again.
    fn session_with_an_agent_calls_result(in_background: bool) -> AgentTranscript {
        use AgentMessageRole::{Assistant, User};
        let mut launch = agent_call("toolu_scout", "scout", "Explore");
        if let AgentMessagePart::ToolUse { input, .. } = &mut launch {
            input["run_in_background"] = json!(in_background);
        }
        let result = if in_background {
            "Running in the background"
        } else {
            "The tree holds src."
        };
        transcript(vec![
            text_message(1, User, "Survey the repo"),
            message(2, Assistant, vec![launch]),
            subagent_message(
                3,
                Assistant,
                vec![AgentMessagePart::Text {
                    text: "The tree holds src.".to_owned(),
                    source: source(Assistant),
                }],
                "Explore",
                "toolu_scout",
            ),
            message(
                4,
                User,
                vec![result_for("toolu_scout", result, source(User))],
            ),
        ])
    }

    #[test]
    fn foreground_agent_calls_result_is_its_sub_agents_report() {
        let output = read(
            &session_with_an_agent_calls_result(false),
            None,
            every_flag(),
        );

        assert_eq!(
            records_after_conversation(&output),
            [
                "subagent s1 type=Explore launch=m2 span=m3 report=m4 : scout",
                "m1 user",
                "m2.1 launches=s1 result=m4",
                "m3 s1",
                "m4 from=s1 call=m2.1",
            ],
            "{output}"
        );
    }

    #[test]
    fn background_agent_calls_result_doesnt_name_its_sub_agent() {
        let output = read(
            &session_with_an_agent_calls_result(true),
            None,
            every_flag(),
        );

        assert!(
            output.contains("\nsubagent s1 type=Explore launch=m2 span=m3 : scout\n"),
            "{output}"
        );
        assert!(output.contains("\nm4 call=m2.1\n"), "{output}");
    }

    /// m9 notes that the scout finished, after its report in m7.
    #[test]
    fn finish_notice_names_its_sub_agent_and_stays_out_of_legends_reports() {
        use crate::history::task_notification::ReportSender;
        let mut transcript = session_with_a_scout();
        let mut notice = text_message(9, AgentMessageRole::User, "Agent \"scout\" finished");
        notice.is_report = true;
        notice.report_sender = ReportSender {
            task_id: None,
            launch_call: Some("toolu_scout".to_owned()),
        };
        transcript.messages.push(notice);

        let output = read(&transcript, None, every_flag());

        assert!(output.contains(&format!("\n{SCOUT_LEGEND}\n")), "{output}");
        assert!(output.contains("\nm9 report from=s1\n"), "{output}");
    }

    fn spawn_call(id: &str) -> AgentMessagePart {
        AgentMessagePart::ToolUse {
            id: id.to_owned(),
            name: "spawn_agent".to_owned(),
            tool: Tool::Agent,
            input: json!({"description": "scout"}),
            source: source(AgentMessageRole::Assistant),
        }
    }

    /// Message `ordinal` of the Codex thread `thread` at the agent path
    /// `/root/scout`, launched by the call `launch_call`, holding `text`.
    fn codex_scout_message(
        ordinal: usize,
        text: &str,
        thread: &str,
        launch_call: &str,
    ) -> AgentMessage {
        use crate::agent::transcript::SubagentOrigin;
        use crate::log_entry::SubagentIdentity;
        AgentMessage {
            parent_tool_use_id: Some(thread.to_owned()),
            subagent: Some(std::sync::Arc::new(SubagentOrigin {
                label: thread.to_owned(),
                transcript: None,
                launch_call: Some(launch_call.to_owned()),
                identity: SubagentIdentity {
                    agent_path: Some("/root/scout".to_owned()),
                    ..SubagentIdentity::default()
                },
            })),
            ..text_message(ordinal, AgentMessageRole::Assistant, text)
        }
    }

    /// Message `ordinal`, a report a Codex sub-agent at `/root/scout`
    /// handed back.
    fn codex_scout_report(ordinal: usize) -> AgentMessage {
        use crate::history::task_notification::ReportSender;
        let mut report = text_message(
            ordinal,
            AgentMessageRole::User,
            "Agent \"scout\" handed back its report",
        );
        report.is_report = true;
        report.report_has_text = true;
        report.report_sender = ReportSender {
            task_id: Some("/root/scout".to_owned()),
            launch_call: None,
        };
        report
    }

    /// m2.1 launches `alpha` and m2.2 `beta`; beta writes m3 before alpha
    /// writes m4.
    #[test]
    fn handles_follow_order_of_calls_launching_sub_agents() {
        use AgentMessageRole::{Assistant, User};
        let text = |text: &str| {
            vec![AgentMessagePart::Text {
                text: text.to_owned(),
                source: source(Assistant),
            }]
        };
        let transcript = transcript(vec![
            text_message(1, User, "Survey the repo"),
            message(
                2,
                Assistant,
                vec![
                    agent_call("toolu_alpha", "alpha", "Explore"),
                    agent_call("toolu_beta", "beta", "Explore"),
                ],
            ),
            subagent_message(3, Assistant, text("beta reads"), "beta", "toolu_beta"),
            subagent_message(4, Assistant, text("alpha reads"), "alpha", "toolu_alpha"),
        ]);

        let output = read(&transcript, None, every_flag());

        let records = records_after_conversation(&output);
        assert_eq!(
            records[..2],
            [
                "subagent s1 type=Explore launch=m2 span=m4 : alpha",
                "subagent s2 type=Explore launch=m2 span=m3 : beta",
            ],
            "{output}"
        );
        assert!(records.contains(&"m2.1 launches=s1"), "{output}");
        assert!(records.contains(&"m2.2 launches=s2"), "{output}");
    }

    /// m2 launches the scout, m3 sends to it, and m4 is the scout's first
    /// message.
    #[test]
    fn call_sending_to_sub_agent_before_its_first_message_names_it() {
        use AgentMessageRole::{Assistant, User};
        let send = AgentMessagePart::ToolUse {
            id: "toolu_message".to_owned(),
            name: "SendMessage".to_owned(),
            tool: Tool::AgentMessage,
            input: json!({"recipient": "Explore-id", "message": "Start with src."}),
            source: source(Assistant),
        };
        let transcript = transcript(vec![
            text_message(1, User, "Survey the repo"),
            message(
                2,
                Assistant,
                vec![agent_call("toolu_scout", "scout", "Explore")],
            ),
            message(3, Assistant, vec![send]),
            subagent_message(
                4,
                Assistant,
                vec![AgentMessagePart::Text {
                    text: "Reading the tree.".to_owned(),
                    source: source(Assistant),
                }],
                "Explore",
                "toolu_scout",
            ),
        ]);

        let shown = read(&transcript, None, every_flag());
        let hidden = read(&transcript, None, options());

        assert!(shown.contains("\nm3.1 target=s1\n"), "{shown}");
        assert!(
            hidden.contains("\nm2..m3 hidden launches=s1 target=s1 tools=2\n"),
            "{hidden}"
        );
    }

    /// m2 spawns `/root/scout`, and its thread writes m3; m4 sends it more
    /// work by its thread id, as Codex's `send_input` does.
    #[test]
    fn call_sending_to_codex_thread_id_names_its_sub_agent() {
        use AgentMessageRole::{Assistant, User};
        let send_input = AgentMessagePart::ToolUse {
            id: "call_input".to_owned(),
            name: "send_input".to_owned(),
            tool: Tool::AgentMessage,
            input: json!({"recipient": "thread-scout", "message": "Read the tests too."}),
            source: source(Assistant),
        };
        let transcript = transcript(vec![
            text_message(1, User, "Survey the repo"),
            message(2, Assistant, vec![spawn_call("call_scout")]),
            codex_scout_message(3, "Reading the tree.", "thread-scout", "call_scout"),
            message(4, Assistant, vec![send_input]),
        ]);

        let output = read(&transcript, None, every_flag());

        assert!(output.contains("\nm4.1 target=s1\n"), "{output}");
    }

    /// m2 and m5 spawn `/root/scout`: the first thread writes m3 and
    /// reports in m4, the second writes m6 and reports in m7.
    #[test]
    fn report_from_task_path_spawned_again_names_later_sub_agent() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            text_message(1, User, "Survey the repo twice"),
            message(2, Assistant, vec![spawn_call("call_first")]),
            codex_scout_message(3, "First pass.", "thread-first", "call_first"),
            codex_scout_report(4),
            message(5, Assistant, vec![spawn_call("call_second")]),
            codex_scout_message(6, "Second pass.", "thread-second", "call_second"),
            codex_scout_report(7),
        ]);

        let output = read(&transcript, None, every_flag());
        let second = read_subagent(&transcript, 2).unwrap();

        let records = records_after_conversation(&output);
        assert_eq!(
            records[..2],
            [
                "subagent s1 launch=m2 span=m3 report=m4 : scout",
                "subagent s2 launch=m5 span=m6 report=m7 : scout",
            ],
            "{output}"
        );
        assert!(
            records
                .iter()
                .any(|record| record.starts_with("m7 report from=s2")),
            "{output}"
        );
        assert_eq!(addresses(&second), ["m5.1", "m6", "m7"], "{second}");
    }

    /// The user's shell command, `true` at 15:41:20, and its output at
    /// 15:41:21, in one user message.
    fn users_shell_command() -> AgentTranscript {
        use AgentMessageRole::User;
        transcript(vec![message(
            1,
            User,
            vec![
                call_with_id("a", "bash", "true", source_at(User, "2026-10-14T15:41:20Z")),
                result_for("a", "done", source_at(User, "2026-10-14T15:41:21Z")),
            ],
        )])
    }

    #[test]
    fn result_in_its_calls_message_prints_after_call_with_its_own_time() {
        let output = read(&users_shell_command(), None, every_flag());

        let at = |utc: &str| record_time(utc, utc);
        assert_eq!(
            records_after_conversation(&output)[1..],
            [
                format!(
                    "m1 user{}",
                    record_time("2026-10-14T15:41:20Z", "2026-10-14T15:41:21Z")
                ),
                format!("m1.1{}", at("2026-10-14T15:41:20Z")),
                format!("m1.1 result{}", at("2026-10-14T15:41:21Z")),
            ],
            "{output}"
        );
        assert!(
            output.ends_with(&format!(
                "| bash: true\nm1.1 result{}\n| done\n",
                at("2026-10-14T15:41:21Z")
            )),
            "{output}"
        );
    }

    #[test]
    fn result_in_its_calls_message_shows_with_tool_results_alone_and_names_its_tool() {
        let output = read(
            &users_shell_command(),
            None,
            ProtocolOptions {
                tool_results: true,
                ..options()
            },
        );

        let at = |utc: &str| record_time(utc, utc);
        assert_eq!(
            records_after_conversation(&output)[1..],
            [
                format!(
                    "m1 user{} hidden-tools=bash",
                    record_time("2026-10-14T15:41:20Z", "2026-10-14T15:41:21Z")
                ),
                format!("m1.1 result{} tool=bash", at("2026-10-14T15:41:21Z")),
            ],
            "{output}"
        );
    }

    #[test]
    fn each_result_in_its_calls_message_prints_after_its_own_call() {
        use AgentMessageRole::Assistant;
        let mut reply = text_message(1, Assistant, "I'll check both.");
        reply.parts.extend([
            call_with_id("a", "Bash", "git status", source(Assistant)),
            result_for("a", "clean", source(Assistant)),
            call_with_id("b", "Bash", "git log -1", source(Assistant)),
            result_for("b", "abc123 Fix the reader", source(Assistant)),
        ]);
        let transcript = transcript(vec![reply]);

        let output = read(&transcript, None, every_flag());

        assert!(
            output.ends_with(concat!(
                "\nm1\n",
                "| I'll check both.\n",
                "m1.1\n",
                "| Bash: git status\n",
                "m1.1 result\n",
                "| clean\n",
                "m1.2\n",
                "| Bash: git log -1\n",
                "m1.2 result\n",
                "| abc123 Fix the reader\n",
            )),
            "{output}"
        );
    }

    /// m1 holds `intro`, a command of three lines and `cargo build`.
    fn a_command_of_three_lines() -> AgentTranscript {
        use AgentMessageRole::{Assistant, User};
        let mut reply = text_message(1, Assistant, "intro");
        reply.parts.extend([
            call_with_id(
                "m",
                "Bash",
                "cd src\ncargo test\ncargo clippy",
                source(Assistant),
            ),
            call_with_id("b", "Bash", "cargo build", source(Assistant)),
        ]);
        transcript(vec![reply, text_message(2, User, "thanks")])
    }

    /// The `omitted` and `read` records of m1 in `transcript`, read with
    /// `options` over `range`, its content truncated to its first `keep`
    /// characters.
    fn omitted_records_keeping(
        transcript: &AgentTranscript,
        range: Option<MessageRange>,
        options: ProtocolOptions,
        keep: usize,
    ) -> String {
        let resolved = resolved("session.jsonl");
        let request = ReadRequest {
            resolved: &resolved,
            transcript,
            range,
            subagent: None,
            call: None,
        };
        let links = CallLinks::of(transcript);
        let one_message = range.is_some_and(|range| range.is_single());
        let commands = if one_message {
            CommandLines::All
        } else {
            CommandLines::FirstLine
        };
        let rendered = shown_message(
            request.resolved,
            &transcript.messages[0],
            &links,
            options,
            commands,
        )
        .unwrap();
        let truncation = rendered.truncation_keeping(keep);
        let mut writer = RecordWriter::new(RecordSize::Full);
        rendered.write(&HiddenParts::default(), Some(&truncation), &mut writer);
        let output = writer.into_output();
        let lines = output.lines().collect::<Vec<_>>();
        lines[lines.len() - 2..].join("\n")
    }

    #[test]
    fn omitted_record_names_omitted_lines_and_read_record_shows_them() {
        let body = (1..=50)
            .map(|line| format!("line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let transcript = transcript(vec![text_message(1, AgentMessageRole::Assistant, &body)]);
        let reference = resolved("session.jsonl").reference.canonical();

        let after_ten_lines = omitted_records_keeping(&transcript, None, options(), 80);
        let inside_the_eleventh = omitted_records_keeping(&transcript, None, options(), 83);

        let expected = format!(
            "m1 omitted lines=11..50\nread ref={reference}:m1 lines=11..50 tools=false tool-results=false thinking=false subagents=false"
        );
        assert_eq!(after_ten_lines, expected);
        assert_eq!(inside_the_eleventh, expected);
    }

    #[test]
    fn omitted_record_names_call_records_it_drops_and_its_read_repeats_flags() {
        let reference = resolved("session.jsonl").reference.canonical();

        let records = omitted_records_keeping(&a_command_of_three_lines(), None, every_flag(), 3);

        assert_eq!(
            records,
            format!(
                "m1 omitted lines=1..5 calls=m1.1..2\nread ref={reference}:m1 lines=1..5 tools=true tool-results=true thinking=true subagents=true"
            )
        );
    }

    /// A read of several messages shows the first line of a command; the
    /// one-message read the `read` record names shows every line.
    #[test]
    fn omitted_record_numbers_lines_as_one_message_read_does() {
        let transcript = a_command_of_three_lines();
        let reference = resolved("session.jsonl").reference.canonical();
        let with_tools = ProtocolOptions {
            tools: true,
            ..options()
        };
        let several = Some(MessageRange { start: 1, end: 2 });
        let intro_and_first_call = "intro\nBash: cd src (+2 lines)\n".chars().count();
        let flags = "tools=true tool-results=false thinking=false subagents=false";

        let inside_first_call = omitted_records_keeping(&transcript, several, with_tools, 8);
        let inside_second_call =
            omitted_records_keeping(&transcript, several, with_tools, intro_and_first_call + 3);
        let reread = read_slice(
            &transcript,
            ReadSlice::Lines(MessageLineRange { start: 5, end: 5 }),
        );

        assert_eq!(
            inside_first_call,
            format!("m1 omitted lines=2..5 calls=m1.2\nread ref={reference}:m1 lines=2..5 {flags}")
        );
        assert_eq!(
            inside_second_call,
            format!("m1 omitted lines=5..5\nread ref={reference}:m1 lines=5..5 {flags}")
        );
        assert!(
            reread.ends_with("\nm1.2\n|  5: Bash: cargo build\n"),
            "{reread}"
        );
    }

    /// The omitted records follow the last line the truncation keeps, below
    /// the call record it belongs to.
    #[test]
    fn omitted_records_follow_last_kept_line_of_call() {
        let transcript = a_command_of_three_lines();
        let with_tools = ProtocolOptions {
            tools: true,
            ..options()
        };
        let resolved = resolved("session.jsonl");
        let links = CallLinks::of(&transcript);
        let rendered = shown_message(
            &resolved,
            &transcript.messages[0],
            &links,
            with_tools,
            CommandLines::FirstLine,
        )
        .unwrap();
        let truncation = rendered.truncation_keeping(8);
        let mut writer = RecordWriter::new(RecordSize::Full);

        rendered.write(&HiddenParts::default(), Some(&truncation), &mut writer);

        let output = writer.into_output();
        assert!(
            output.starts_with(
                "m1\n| intro\nm1.1\n| Ba\nm1 omitted lines=2..5 calls=m1.2\nread ref="
            ),
            "{output}"
        );
    }

    #[test]
    fn message_showing_calls_alone_counts_its_hidden_thinking() {
        use AgentMessageRole::Assistant;
        let transcript = transcript(vec![message(
            1,
            Assistant,
            vec![
                thinking("check the build first", source(Assistant)),
                call_with_id("b", "Bash", "cargo build", source(Assistant)),
            ],
        )]);

        let output = read(
            &transcript,
            None,
            ProtocolOptions {
                tools: true,
                ..options()
            },
        );

        assert_eq!(
            records_after_conversation(&output),
            ["m1 hidden-thinking=1", "m1.1"],
            "{output}"
        );
        assert!(
            output.ends_with("\nm1.1\n| Bash: cargo build\n"),
            "{output}"
        );
    }

    #[test]
    fn one_message_read_of_message_showing_calls_alone_shows_its_anchor() {
        use AgentMessageRole::Assistant;
        let transcript = transcript(vec![message(
            1,
            Assistant,
            vec![call_with_id("b", "Bash", "cargo build", source(Assistant))],
        )]);
        let with_tools = ProtocolOptions {
            tools: true,
            ..options()
        };

        let one = read(&transcript, Some(MessageRange::single(1)), with_tools);
        let whole = read(&transcript, None, with_tools);

        let records = records_after_conversation(&one);
        assert!(records[0].starts_with("m1 anchor=ma_"), "{one}");
        assert_eq!(records[1..], ["m1.1"], "{one}");
        assert_eq!(records_after_conversation(&whole), ["m1.1"], "{whole}");
    }

    #[test]
    fn each_call_is_record_with_its_own_time_and_message_holding_its_result() {
        use AgentMessageRole::{Assistant, User};
        let at = |second: u32| format!("2026-10-14T15:41:{second:02}Z");
        let transcript = transcript(vec![
            message(
                1,
                Assistant,
                vec![
                    AgentMessagePart::Text {
                        text: "I'll write the script and run it.".to_owned(),
                        source: source_at(Assistant, &at(25)),
                    },
                    call_with_id(
                        "w",
                        "Write",
                        "write check.ps1",
                        source_at(Assistant, &at(28)),
                    ),
                    call_with_id(
                        "p",
                        "PowerShell",
                        "pwsh -File check.ps1",
                        source_at(Assistant, &at(30)),
                    ),
                ],
            ),
            message(
                2,
                User,
                vec![result_for("w", "written", source_at(User, &at(29)))],
            ),
            message(
                3,
                User,
                vec![result_for("p", "3 sheets", source_at(User, &at(31)))],
            ),
        ]);

        let output = read(&transcript, None, every_flag());

        let once = |second| record_time(&at(second), &at(second));
        assert_eq!(
            records_after_conversation(&output),
            [
                day_record(&at(25)),
                format!("m1{}", record_time(&at(25), &at(30))),
                format!("m1.1{} result=m2", once(28)),
                format!("m1.2{} result=m3", once(30)),
                format!("m2{} call=m1.1", once(29)),
                format!("m3{} call=m1.2", once(31)),
            ],
            "{output}"
        );
        assert!(
            output.contains(&format!(
                "\nm1.2{} result=m3\n| PowerShell: pwsh -File check.ps1\n",
                once(30)
            )),
            "{output}"
        );
    }

    /// m1 holds a `Read` call, then the `Bash` call whose result is m2.
    fn a_result_of_a_second_call() -> AgentTranscript {
        use AgentMessageRole::{Assistant, User};
        transcript(vec![
            message(
                1,
                Assistant,
                vec![
                    call_with_id("r", "Read", "cat notes.md", source(Assistant)),
                    call_with_id("b", "Bash", "cargo test", source(Assistant)),
                ],
            ),
            message(2, User, vec![result_for("b", "ok", source(User))]),
            text_message(3, Assistant, "The tests pass."),
        ])
    }

    #[test]
    fn call_number_counts_calls_read_hides_and_result_names_its_tool() {
        let transcript = a_result_of_a_second_call();

        let output = read(
            &transcript,
            None,
            ProtocolOptions {
                tool_results: true,
                ..options()
            },
        );

        assert!(output.contains("\nm2 call=m1.2 tool=Bash\n"), "{output}");
    }

    #[test]
    fn shown_call_doesnt_add_tool_to_its_result() {
        let transcript = a_result_of_a_second_call();

        let output = read(&transcript, None, every_flag());

        assert!(output.contains("\nm2 call=m1.2\n"), "{output}");
    }

    #[test]
    fn result_whose_call_is_outside_range_names_its_tool() {
        let transcript = a_result_of_a_second_call();

        let output = read(
            &transcript,
            Some(MessageRange { start: 2, end: 3 }),
            every_flag(),
        );

        assert!(output.contains("\nm2 call=m1.2 tool=Bash\n"), "{output}");
    }

    /// m1 is too long for the budget to keep beside m2, the last message.
    #[test]
    fn result_names_tool_of_call_budget_omits() {
        use AgentMessageRole::{Assistant, User};
        let mut announcing = text_message(1, Assistant, &"I'll run the tests. ".repeat(150));
        announcing
            .parts
            .push(call_with_id("b", "Bash", "cargo test", source(Assistant)));
        let transcript = transcript(vec![
            announcing,
            message(2, User, vec![result_for("b", "ok", source(User))]),
        ]);

        let output = read_tail(
            &transcript,
            None,
            2,
            ProtocolOptions {
                budget: Some(400),
                ..every_flag()
            },
        );

        assert_eq!(addresses(&output), ["m2"], "{output}");
        assert!(output.contains("\nm2 call=m1.1 tool=Bash\n"), "{output}");
    }

    /// m1's first call is too long for the budget to keep m1 whole, and its
    /// truncation drops m1.2's record: m2 names the tool of each call it
    /// answers, m1.1's too.
    #[test]
    fn result_names_every_tool_when_truncation_drops_one_of_its_calls() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            message(
                1,
                Assistant,
                vec![
                    call_with_id("e", "Edit", &"x".repeat(1_000), source(Assistant)),
                    call_with_id("b", "Bash", "cargo test", source(Assistant)),
                ],
            ),
            message(
                2,
                User,
                vec![
                    result_for("e", "edited", source(User)),
                    result_for("b", "ok", source(User)),
                ],
            ),
        ]);

        let output = read(
            &transcript,
            None,
            ProtocolOptions {
                budget: Some(600),
                ..every_flag()
            },
        );

        assert_eq!(addresses(&output), ["m1.1", "m2"], "{output}");
        assert!(
            output.contains("\nm2 call=m1.1,m1.2 tool=Edit,Bash\n"),
            "{output}"
        );
    }

    #[test]
    fn result_of_several_calls_names_their_tools_only_when_read_hides_them() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            message(
                1,
                Assistant,
                vec![
                    call_with_id("e", "Edit", "edit notes.md", source(Assistant)),
                    call_with_id("b", "Bash", "cargo test", source(Assistant)),
                ],
            ),
            message(
                2,
                User,
                vec![
                    result_for("e", "edited", source(User)),
                    result_for("b", "ok", source(User)),
                ],
            ),
        ]);
        let tool_results = ProtocolOptions {
            tool_results: true,
            ..options()
        };

        let hiding_calls = read(&transcript, None, tool_results);
        let showing_calls = read(&transcript, None, every_flag());

        assert!(
            hiding_calls.contains("\nm2 call=m1.1,m1.2 tool=Edit,Bash\n"),
            "{hiding_calls}"
        );
        assert!(
            showing_calls.contains("\nm2 call=m1.1,m1.2\n"),
            "{showing_calls}"
        );
    }

    /// m1's `exec` call is split in two parts, and m2 answers both.
    #[test]
    fn result_of_split_call_names_its_parts_as_one_range() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            message(
                1,
                Assistant,
                vec![
                    call_with_id("x", "exec", "git status", source(Assistant)),
                    call_with_id("x#2", "exec", "git diff", source(Assistant)),
                ],
            ),
            message(2, User, vec![result_for("x", "clean", source(User))]),
        ]);
        let tool_results = ProtocolOptions {
            tool_results: true,
            ..options()
        };

        let showing_calls = read(&transcript, None, every_flag());
        let hiding_calls = read(&transcript, None, tool_results);

        assert_eq!(
            records_after_conversation(&showing_calls),
            ["m1.1 result=m2", "m1.2 result=m2", "m2 call=m1.1..2"],
            "{showing_calls}"
        );
        assert!(
            hiding_calls.contains("\nm2 call=m1.1..2 tool=exec\n"),
            "{hiding_calls}"
        );
    }

    #[test]
    fn outline_with_tool_results_names_tool_of_each_call() {
        let transcript = a_result_of_a_second_call();

        let output = format_outline(
            &resolved("session.jsonl"),
            &transcript,
            ProtocolOptions {
                tool_results: true,
                ..options()
            },
        );

        assert!(
            output.contains("\nm2 call=m1.2 tool=Bash chars=2 | ok\n"),
            "{output}"
        );
    }

    #[test]
    fn call_with_hook_result_names_both_result_messages() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            message(
                1,
                Assistant,
                vec![call_with_id("a", "Bash", "cargo fmt", source(Assistant))],
            ),
            message(2, User, vec![result_for("a", "ok", source(User))]),
            message(
                3,
                User,
                vec![result_for("a", "PostToolUse hook: formatted", source(User))],
            ),
        ]);

        let output = read(&transcript, None, every_flag());

        assert!(output.contains("\nm1.1 result=m2,m3\n"), "{output}");
        assert!(output.contains("\nm3 call=m1.1\n"), "{output}");
    }

    #[test]
    fn unknown_call_id_doesnt_link_call_to_result() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            message(
                1,
                Assistant,
                vec![call_with_id("unknown", "exec", "ls", source(Assistant))],
            ),
            message(2, User, vec![result_for("unknown", "a.txt", source(User))]),
        ]);

        let output = read(&transcript, None, every_flag());

        assert_eq!(
            records_after_conversation(&output),
            ["m1.1", "m2 result"],
            "{output}"
        );
    }

    /// m2 shows its text and hides its calls; m3 and m4 answer them. m6's call
    /// and its result, m7, are hidden together.
    #[test]
    fn run_of_hidden_results_names_calls_they_answer_outside_run() {
        use AgentMessageRole::{Assistant, User};
        let mut announcing = text_message(2, Assistant, "I'll read the notes and run the tests.");
        announcing.parts.extend([
            call_with_id("r", "Read", "cat notes.md", source(Assistant)),
            call_with_id("b", "Bash", "cargo test", source(Assistant)),
        ]);
        let transcript = transcript(vec![
            text_message(1, User, "Check the notes"),
            announcing,
            message(3, User, vec![result_for("r", "notes", source(User))]),
            message(4, User, vec![result_for("b", "ok", source(User))]),
            text_message(5, Assistant, "Both done."),
            message(
                6,
                Assistant,
                vec![call_with_id("g", "Bash", "git status", source(Assistant))],
            ),
            message(7, User, vec![result_for("g", "clean", source(User))]),
        ]);

        let output = read(&transcript, None, options());

        assert!(
            output.contains("\nm3..m4 hidden tool-results=2 calls=m2.1..2\n"),
            "{output}"
        );
        assert!(
            output.ends_with("\nm6..m7 hidden tools=1 tool-results=1\n"),
            "{output}"
        );
    }

    #[test]
    fn hidden_result_below_its_printed_call_doesnt_name_call() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            message(
                1,
                Assistant,
                vec![call_with_id("b", "Bash", "cargo test", source(Assistant))],
            ),
            message(2, User, vec![result_for("b", "ok", source(User))]),
        ]);

        let output = read(
            &transcript,
            None,
            ProtocolOptions {
                tools: true,
                ..options()
            },
        );

        assert_eq!(
            records_after_conversation(&output),
            ["m1.1 result=m2", "m2 hidden tool-results=1"],
            "{output}"
        );
    }

    /// Three runs of `cargo test`, each a call and the result it printed.
    fn three_test_runs(result: &str) -> AgentTranscript {
        use AgentMessageRole::{Assistant, User};
        transcript(
            ["a", "b", "c"]
                .iter()
                .enumerate()
                .flat_map(|(index, id)| {
                    let call = 2 * index + 1;
                    [
                        message(
                            call,
                            Assistant,
                            vec![call_with_id(id, "Bash", "cargo test", source(Assistant))],
                        ),
                        message(call + 1, User, vec![result_for(id, result, source(User))]),
                    ]
                })
                .collect(),
        )
    }

    #[test]
    fn repeated_body_and_repeated_call_name_their_first_copy() {
        let output = read(&three_test_runs("12 passed"), None, every_flag());

        assert!(
            output.ends_with(concat!(
                "\nm1.1 result=m2\n",
                "| Bash: cargo test\n",
                "m2 call=m1.1\n",
                "| 12 passed\n",
                "m3.1 result=m4 same=m1.1\n",
                "m4 call=m3.1 same=m2\n",
                "m5.1 result=m6 same=m1.1\n",
                "m6 call=m5.1 same=m2\n"
            )),
            "{output}"
        );
    }

    /// m2 launches `alpha` and `beta`; each hands back the same final
    /// answer, alpha's in m3 and beta's in m4.
    #[test]
    fn sub_agents_body_doesnt_repeat_another_sub_agents_copy() {
        use AgentMessageRole::{Assistant, User};
        let answer = |ordinal, label: &str, launch: &str| {
            subagent_message(
                ordinal,
                Assistant,
                vec![AgentMessagePart::Text {
                    text: "The notes list the release steps.".to_owned(),
                    source: source(Assistant),
                }],
                label,
                launch,
            )
        };
        let transcript = transcript(vec![
            text_message(1, User, "Survey the repo"),
            message(
                2,
                Assistant,
                vec![
                    agent_call("toolu_alpha", "alpha", "Explore"),
                    agent_call("toolu_beta", "beta", "Explore"),
                ],
            ),
            answer(3, "alpha", "toolu_alpha"),
            answer(4, "beta", "toolu_beta"),
        ]);

        let output = read(&transcript, None, every_flag());

        assert!(
            output.ends_with("\nm4 s2\n| The notes list the release steps.\n"),
            "{output}"
        );
    }

    #[test]
    fn repeat_shorter_than_its_same_atom_prints_its_lines() {
        let output = read(&three_test_runs("ok"), None, every_flag());

        assert!(
            output.ends_with(concat!(
                "\nm5.1 result=m6 same=m1.1\n",
                "m6 call=m5.1\n",
                "| ok\n"
            )),
            "{output}"
        );
        assert!(!output.contains(" same=m2"), "{output}");
    }

    /// 30 replies of `ok` are as long as 30 distinct two-character replies:
    /// that read's length is their full size.
    #[test]
    fn budget_of_their_full_size_doesnt_omit_any_of_thirty_replies_of_ok() {
        let replies = |text: &dyn Fn(usize) -> String| {
            transcript(
                (1..=30)
                    .map(|ordinal| {
                        text_message(ordinal, AgentMessageRole::Assistant, &text(ordinal))
                    })
                    .collect(),
            )
        };
        let full_size = read(
            &replies(&|ordinal| format!("{ordinal:02}")),
            None,
            options(),
        )
        .chars()
        .count();

        let output = read(&replies(&|_| "ok".to_owned()), None, with_budget(full_size));

        assert!(output.contains(" omit=none"), "{output}");
        assert_eq!(output.matches("\n| ok\n").count(), 30, "{output}");
    }

    /// A tail read keeps m3 whole first, then m1 truncated: m1's copy of the
    /// body is printed first but not whole.
    #[test]
    fn truncated_copy_doesnt_become_target_of_repeat() {
        use AgentMessageRole::{Assistant, User};
        let body = "x".repeat(3_000);
        let transcript = transcript(vec![
            text_message(1, Assistant, &body),
            text_message(2, User, "a short question"),
            text_message(3, Assistant, &body),
        ]);

        let whole = read(&transcript, None, with_budget(10_000));
        let truncated = read_tail(&transcript, None, 3, with_budget(3_600));

        assert!(whole.contains("\nm3 same=m1\n"), "{whole}");
        assert_eq!(addresses(&truncated), ["m1", "m2", "m3"], "{truncated}");
        assert!(truncated.contains(OMISSION_MARKER), "{truncated}");
        assert!(
            truncated.ends_with(&format!("\nm3\n| {body}\n")),
            "{truncated}"
        );
    }

    /// The identical replies count toward the budget as the distinct ones of
    /// the same length do, though they print `same=`.
    #[test]
    fn budget_counts_repeated_body_at_full_size() {
        let replies = |text: &dyn Fn(usize) -> String| {
            transcript(
                (1..=30)
                    .map(|ordinal| {
                        text_message(ordinal, AgentMessageRole::Assistant, &text(ordinal))
                    })
                    .collect(),
            )
        };
        let identical = replies(&|_| "reply 00 with padding".to_owned());
        let distinct = replies(&|ordinal| format!("reply {ordinal:02} with padding"));
        let header_atom = |output: &str, name: &str| {
            crate::agent::test_support::atom(output.lines().next().unwrap(), name)
                .map(str::to_owned)
        };

        let identical_read = read(&identical, None, with_budget(600));
        let distinct_read = read(&distinct, None, with_budget(600));

        assert!(identical_read.contains(" same=m1\n"), "{identical_read}");
        assert_eq!(
            header_atom(&identical_read, "omit"),
            header_atom(&distinct_read, "omit")
        );
        assert!(identical_read.chars().count() < distinct_read.chars().count());
    }

    #[test]
    fn only_single_message_read_shows_anchor() {
        let transcript = timed_replies(3, 10);

        let one = read(&transcript, Some(MessageRange::single(2)), options());
        let several = read(
            &transcript,
            Some(MessageRange { start: 1, end: 2 }),
            options(),
        );

        assert!(
            records(&one)
                .iter()
                .any(|record| record.starts_with("m2 ") && record.contains(" anchor=ma_")),
            "{one}"
        );
        assert!(!several.contains("anchor="), "{several}");
    }

    /// A day apart: their local dates differ in every zone. m2's parts are
    /// written over a day.
    #[test]
    fn read_states_day_before_its_first_record_and_wherever_date_changes() {
        use AgentMessageRole::{Assistant, User};
        let mut overnight = text_message_at(
            2,
            Assistant,
            "I'll keep the job running.",
            "2026-10-15T12:00:00Z",
        );
        overnight
            .parts
            .extend(text_message_at(2, Assistant, "It finished.", "2026-10-16T12:00:05Z").parts);
        let transcript = transcript(vec![
            text_message_at(1, User, "Start the job", "2026-10-14T12:00:00Z"),
            overnight,
            text_message_at(3, User, "Thanks", "2026-10-16T12:00:10Z"),
        ]);

        let output = read(&transcript, None, options());

        assert!(
            output.starts_with(&format!(
                "protocol agent-read cut=none chars=6000 policy=text omit=none{}\n",
                tz_at("2026-10-14T12:00:00Z")
            )),
            "{output}"
        );
        assert_eq!(
            records_after_conversation(&output),
            [
                day_record("2026-10-14T12:00:00Z"),
                format!(
                    "m1 user{}",
                    record_time("2026-10-14T12:00:00Z", "2026-10-14T12:00:00Z")
                ),
                day_record("2026-10-15T12:00:00Z"),
                format!(
                    "m2{}",
                    record_time("2026-10-15T12:00:00Z", "2026-10-16T12:00:05Z")
                ),
                day_record("2026-10-16T12:00:10Z"),
                format!(
                    "m3 user{}",
                    record_time("2026-10-16T12:00:10Z", "2026-10-16T12:00:10Z")
                ),
            ],
            "{output}"
        );
    }

    /// The date of the reply after m1 in [`calls_across_midnight`].
    #[derive(Clone, Copy, Debug)]
    enum ReplyDate {
        /// A minute after midnight, the date of m1's last records.
        NextDay,
        /// Five seconds before midnight, the date of m1's first records.
        FirstDay,
    }

    /// The headers of the calls in a budget sweep's fixture.
    #[derive(Clone, Copy, Debug)]
    enum CallHeaders {
        /// `Bash: cargo build --release`, then 118 characters each.
        Varied,
        /// [`LONG_HEADER_CHARS`] characters each.
        Long,
    }

    const LONG_HEADER_CHARS: usize = 200;

    impl CallHeaders {
        /// The shell command of call `index`, from 0.
        fn command(self, index: usize) -> String {
            match (self, index) {
                (Self::Varied, 0) => "cargo build --release".to_string(),
                (Self::Varied, _) => format!("cargo test --test suite_{index} {}", "y".repeat(80)),
                (Self::Long, _) => {
                    let command = format!("cargo test --test suite_{index} ");
                    let padding = LONG_HEADER_CHARS - "Bash: ".len() - command.len();
                    format!("{command}{}", "y".repeat(padding))
                }
            }
        }
    }

    /// m1's body of `body_chars` characters is written a minute before local
    /// midnight. Its `calls` calls follow 20 seconds apart from 20 seconds
    /// before midnight, each with its result in m1 10 seconds later: the
    /// first call and its result before midnight, the others from midnight
    /// on. m2 is a short reply on `reply`'s date.
    fn calls_across_midnight(
        body_chars: usize,
        calls: usize,
        headers: CallHeaders,
        reply: ReplyDate,
    ) -> AgentTranscript {
        use AgentMessageRole::Assistant;
        use chrono::TimeZone;
        let midnight = chrono::Local
            .with_ymd_and_hms(2026, 10, 15, 0, 0, 0)
            .single()
            .unwrap();
        let at = |seconds: i64| {
            (midnight + chrono::Duration::seconds(seconds))
                .with_timezone(&chrono::Utc)
                .to_rfc3339()
        };
        let mut parts = vec![AgentMessagePart::Text {
            text: "x".repeat(body_chars),
            source: source_at(Assistant, &at(-60)),
        }];
        for index in 0..calls {
            let id = format!("t{index}");
            let written = 20 * index as i64 - 20;
            parts.push(call_with_id(
                &id,
                "Bash",
                &headers.command(index),
                source_at(Assistant, &at(written)),
            ));
            parts.push(result_for(
                &id,
                "ok",
                source_at(Assistant, &at(written + 10)),
            ));
        }
        let reply_at = match reply {
            ReplyDate::NextDay => 60,
            ReplyDate::FirstDay => -5,
        };
        transcript(vec![
            message(1, Assistant, parts),
            text_message_at(2, Assistant, "Done.", &at(reply_at)),
        ])
    }

    /// `--tools`, then `--tools --tool-results`.
    fn call_flags() -> [ProtocolOptions; 2] {
        [
            ProtocolOptions {
                tools: true,
                ..options()
            },
            ProtocolOptions {
                tools: true,
                tool_results: true,
                ..options()
            },
        ]
    }

    /// m1 holds a body of `body_chars` characters, then `calls` calls with
    /// long headers, each with its result in m1; m2 is a short reply.
    /// Nothing is timed: a call's record is shorter than its address in
    /// `calls=`.
    fn untimed_calls_after_a_body(body_chars: usize, calls: usize) -> AgentTranscript {
        use AgentMessageRole::Assistant;
        let mut parts = vec![AgentMessagePart::Text {
            text: "x".repeat(body_chars),
            source: source(Assistant),
        }];
        for index in 0..calls {
            let id = format!("t{index}");
            let command = CallHeaders::Long.command(index);
            parts.push(call_with_id(&id, "Bash", &command, source(Assistant)));
            parts.push(result_for(&id, "ok", source(Assistant)));
        }
        transcript(vec![
            message(1, Assistant, parts),
            text_message(2, Assistant, "Done."),
        ])
    }

    /// Expanding m1 past midnight adds a `day` record before m2, back on the
    /// first date.
    fn reply_back_on_the_first_date() -> AgentTranscript {
        calls_across_midnight(180, 2, CallHeaders::Varied, ReplyDate::FirstDay)
    }

    /// m1's floor holds records on both sides of midnight.
    fn floor_across_midnight() -> AgentTranscript {
        calls_across_midnight(40, 6, CallHeaders::Varied, ReplyDate::NextDay)
    }

    /// m1's floor ends before midnight, beside m2 on the next day.
    fn floor_before_midnight() -> AgentTranscript {
        calls_across_midnight(180, 2, CallHeaders::Varied, ReplyDate::NextDay)
    }

    /// m1 holds a body of 198 characters, then one call: keeping 200
    /// characters, its floor, prints the call header's first character in
    /// place of `calls=m1.1`, two characters fewer than keeping 199.
    fn floor_where_fewer_kept_characters_print_more() -> AgentTranscript {
        untimed_calls_after_a_body(198, 1)
    }

    #[test]
    fn read_fits_each_sampled_budget() {
        let [tools, tools_and_results] = call_flags();
        for (transcript, options) in [
            (reply_back_on_the_first_date(), tools),
            (floor_across_midnight(), tools_and_results),
            (floor_where_fewer_kept_characters_print_more(), tools),
        ] {
            for (budget, output) in stepped_reads(&transcript, options, 37) {
                assert!(
                    output.chars().count() <= budget,
                    "output exceeds budget {budget}: {output}"
                );
            }
        }
    }

    /// Checked where a truncation keeps more than its floor: shrinking it to
    /// its floor makes room for the message.
    #[test]
    fn larger_budget_doesnt_omit_message_smaller_one_showed() {
        let [tools, tools_and_results] = call_flags();
        for (transcript, options) in [
            (reply_back_on_the_first_date(), tools),
            // m1's floor ends on its first call header's first character.
            (
                calls_across_midnight(198, 2, CallHeaders::Long, ReplyDate::NextDay),
                tools_and_results,
            ),
        ] {
            let mut shown_by_a_smaller_budget = BTreeSet::new();
            for (budget, output) in stepped_reads(&transcript, options, 31) {
                let Some(shown) = shown_messages(&output) else {
                    continue;
                };
                if any_truncation_keeps_more_than_its_floor(&output) {
                    assert!(
                        shown.is_superset(&shown_by_a_smaller_budget),
                        "budget {budget} omits a message a smaller budget showed: {output}"
                    );
                }
                shown_by_a_smaller_budget.extend(shown);
            }
        }
    }

    #[test]
    fn read_keeps_every_floor_beside_another_message_or_once_smaller_budget_did() {
        let [tools, _] = call_flags();
        for (transcript, options) in [
            (floor_across_midnight(), tools),
            (floor_before_midnight(), tools),
            (floor_where_fewer_kept_characters_print_more(), tools),
        ] {
            let mut shown_with_every_floor = HashSet::new();
            for (budget, output) in stepped_reads(&transcript, options, 29) {
                let Some(shown) = shown_messages(&output) else {
                    continue;
                };
                let keeps_every_floor = every_truncation_keeps_its_floor(&output);
                if shown.len() > 1 {
                    assert!(
                        keeps_every_floor,
                        "budget {budget} truncates below a floor beside another message: {output}"
                    );
                }
                if shown_with_every_floor.contains(&shown) {
                    assert!(
                        keeps_every_floor,
                        "budget {budget} truncates below a floor though a smaller budget showed the same messages with every floor: {output}"
                    );
                }
                if keeps_every_floor {
                    shown_with_every_floor.insert(shown);
                }
            }
        }
    }

    /// Each budget with its read of `transcript` with `options`: every
    /// `step`th budget from [`SMALLEST_SWEPT_BUDGET`], then the unbudgeted
    /// read's length.
    fn stepped_reads(
        transcript: &AgentTranscript,
        options: ProtocolOptions,
        step: usize,
    ) -> impl Iterator<Item = (usize, String)> + '_ {
        let unbudgeted = read(
            transcript,
            None,
            ProtocolOptions {
                budget: None,
                ..options
            },
        );
        stepped_budgets(unbudgeted.chars().count(), step).map(move |budget| {
            let output = read(
                transcript,
                None,
                ProtocolOptions {
                    budget: Some(budget),
                    ..options
                },
            );
            (budget, output)
        })
    }

    const SMALLEST_SWEPT_BUDGET: usize = 300;

    fn stepped_budgets(unbudgeted_chars: usize, step: usize) -> impl Iterator<Item = usize> {
        (SMALLEST_SWEPT_BUDGET..unbudgeted_chars)
            .step_by(step)
            .chain([unbudgeted_chars])
    }

    /// The messages `output` shows records of; `None` when the read is cut
    /// short: not even the first message's records fit.
    fn shown_messages(output: &str) -> Option<BTreeSet<String>> {
        output.ends_with('\n').then(|| {
            addresses(output)
                .into_iter()
                .filter_map(|address| address.split('.').next())
                .map(str::to_owned)
                .collect()
        })
    }

    /// A truncation ending on a line break shows one character fewer.
    fn every_truncation_keeps_its_floor(output: &str) -> bool {
        kept_chars_of_truncations(output)
            .iter()
            .all(|&chars| chars + 1 >= TRUNCATION_FLOOR_CHARS)
    }

    fn any_truncation_keeps_more_than_its_floor(output: &str) -> bool {
        kept_chars_of_truncations(output)
            .iter()
            .any(|&chars| chars > TRUNCATION_FLOOR_CHARS)
    }

    /// m1's floor ends next to the start of its first call header after a
    /// body of 198 to 201 characters.
    #[test]
    fn truncation_keeps_most_characters_that_fit_each_sampled_room() {
        let resolved = resolved("session.jsonl");
        let hidden = HiddenParts::default();
        for body_chars in 198..=201 {
            for calls in [1, 2] {
                for options in call_flags() {
                    let transcript = untimed_calls_after_a_body(body_chars, calls);
                    let links = CallLinks::of(&transcript);
                    let rendered = shown_message(
                        &resolved,
                        &transcript.messages[0],
                        &links,
                        options,
                        CommandLines::FirstLine,
                    )
                    .unwrap();
                    let chars = |truncation: Option<&Truncation>| {
                        rendered.measured_chars(&hidden, truncation)
                    };
                    let chars_keeping = (0..rendered.content.text.chars().count())
                        .map(|keep| chars(Some(&rendered.truncation_keeping(keep))))
                        .collect::<Vec<_>>();
                    let rooms = sampled_rooms(&chars_keeping, chars(None));

                    for room in rooms {
                        let most = (0..chars_keeping.len())
                            .filter(|&keep| chars_keeping[keep] <= room)
                            .max();
                        let fitted = rendered.truncated_to_fit(room, chars);

                        assert_eq!(
                            fitted.map(|truncation| truncation.kept_chars),
                            most,
                            "body {body_chars}, {calls} calls, tool results {}: room {room} doesn't keep the most characters that fit",
                            options.tool_results
                        );
                    }
                }
            }
        }
    }

    /// Dropping a call header's first character from a truncation can print
    /// two characters more: the call's address in `calls=` prints in its
    /// place.
    const SAMPLED_ROOMS_EITHER_SIDE: usize = 2;

    /// The room holding no content, the room one character short of the
    /// whole, and the rooms within [`SAMPLED_ROOMS_EITHER_SIDE`] of
    /// `chars_keeping` at the floor and on both sides of each place where
    /// keeping one more character doesn't print exactly one more, such as
    /// where a call header or tool result starts to print.
    fn sampled_rooms(chars_keeping: &[usize], whole_chars: usize) -> BTreeSet<usize> {
        let empty = chars_keeping[0];
        let rooms_near_keeping = |keep: usize| {
            let room = chars_keeping[keep];
            room - SAMPLED_ROOMS_EITHER_SIDE..=room + SAMPLED_ROOMS_EITHER_SIDE
        };
        let keeps_beside_uneven_steps = (1..chars_keeping.len())
            .filter(|&keep| chars_keeping[keep] != chars_keeping[keep - 1] + 1)
            .flat_map(|keep| [keep - 1, keep]);
        std::iter::once(TRUNCATION_FLOOR_CHARS)
            .chain(keeps_beside_uneven_steps)
            .flat_map(rooms_near_keeping)
            .chain([empty, whole_chars - 1])
            .filter(|room| (empty..whole_chars).contains(room))
            .collect()
    }

    #[test]
    fn truncation_keeps_its_floor_when_keeping_fewer_characters_prints_more() {
        let transcript = floor_where_fewer_kept_characters_print_more();
        let resolved = resolved("session.jsonl");
        let links = CallLinks::of(&transcript);
        let rendered = shown_message(
            &resolved,
            &transcript.messages[0],
            &links,
            call_flags()[0],
            CommandLines::FirstLine,
        )
        .unwrap();
        let hidden = HiddenParts::default();
        let chars_keeping =
            |keep| rendered.measured_chars(&hidden, Some(&rendered.truncation_keeping(keep)));
        let room = chars_keeping(TRUNCATION_FLOOR_CHARS);

        let fitted = rendered.truncated_to_fit(room, |truncation| {
            rendered.measured_chars(&hidden, truncation)
        });

        assert!(chars_keeping(TRUNCATION_FLOOR_CHARS - 1) > room);
        assert_eq!(
            fitted.map(|truncation| truncation.kept_chars),
            Some(TRUNCATION_FLOOR_CHARS)
        );
    }

    #[test]
    fn message_expanded_past_midnight_keeps_reply_back_on_its_first_date() {
        let output = read(
            &reply_back_on_the_first_date(),
            None,
            ProtocolOptions {
                budget: Some(643),
                tools: true,
                ..options()
            },
        );

        assert!(output.chars().count() <= 643, "{output}");
        assert!(output.contains("| Done.\n"), "{output}");
    }

    #[test]
    fn floor_holding_records_on_both_sides_of_midnight_is_kept_beside_next_days_reply() {
        assert_m1_keeps_its_floor_beside_m2(&floor_across_midnight(), 662);
    }

    #[test]
    fn floor_ending_before_midnight_is_kept_beside_next_days_reply() {
        assert_m1_keeps_its_floor_beside_m2(&floor_before_midnight(), 627);
    }

    /// Asserts that `budget` is the smallest budget showing m2 in the read of
    /// `transcript` with `--tools`, and that this read fits it and shows m1
    /// truncated to its floor. A `day` record charged twice or missed
    /// truncates m1 below its floor or omits m2.
    fn assert_m1_keeps_its_floor_beside_m2(transcript: &AgentTranscript, budget: usize) {
        let read_with_budget = |budget| {
            read(
                transcript,
                None,
                ProtocolOptions {
                    budget: Some(budget),
                    tools: true,
                    ..options()
                },
            )
        };
        let output_one_below = read_with_budget(budget - 1);
        let output = read_with_budget(budget);

        assert!(
            !output_one_below.contains("| Done.\n"),
            "budget {budget} isn't the smallest showing m2; budget {} shows it: {output_one_below}",
            budget - 1
        );
        assert!(
            output.chars().count() <= budget,
            "output exceeds budget {budget}: {output}"
        );
        assert!(output.contains("| Done.\n"), "m2 is omitted: {output}");
        assert_eq!(
            kept_chars_of_truncations(&output),
            [TRUNCATION_FLOOR_CHARS],
            "m1 isn't truncated to its floor: {output}"
        );
    }

    /// January and July: in a zone with daylight saving time, one of them is
    /// in it.
    const WINTER: &str = "2026-01-14T15:00:00Z";
    const SUMMER: &str = "2026-07-14T15:00:00Z";

    /// The `day` record of the UTC time `utc` below records in the offset of
    /// the UTC time `before`.
    fn day_record_after(utc: &str, before: &str) -> String {
        let tz = tz_at(utc);
        if tz == tz_at(before) {
            day_record(utc)
        } else {
            format!("{}{tz}", day_record(utc))
        }
    }

    #[test]
    fn header_names_offset_of_first_day_record_and_later_one_its_own() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            text_message_at(1, User, "Start the job", WINTER),
            text_message_at(2, Assistant, "It finished.", SUMMER),
        ]);

        let output = read(&transcript, None, options());

        assert!(
            output.starts_with(&format!(
                "protocol agent-read cut=none chars=6000 policy=text omit=none{}\n",
                tz_at(WINTER)
            )),
            "{output}"
        );
        assert_eq!(
            records_after_conversation(&output),
            [
                day_record(WINTER),
                format!("m1 user{}", record_time(WINTER, WINTER)),
                day_record_after(SUMMER, WINTER),
                format!("m2{}", record_time(SUMMER, SUMMER)),
            ],
            "{output}"
        );
    }

    #[test]
    fn read_of_two_sessions_names_first_sessions_offset_in_its_header() {
        let winter = transcript(vec![text_message_at(
            1,
            AgentMessageRole::User,
            "Start the job",
            WINTER,
        )]);
        let summer = transcript(vec![text_message_at(
            1,
            AgentMessageRole::User,
            "Check the job",
            SUMMER,
        )]);
        let (winter_ref, summer_ref) = (resolved("winter.jsonl"), resolved("summer.jsonl"));

        let output = format_read(
            &[
                whole_session_request(&winter_ref, &winter),
                whole_session_request(&summer_ref, &summer),
            ],
            None,
            None,
            options(),
        )
        .unwrap();

        let records = records(&output);
        assert!(
            records[0].ends_with(&format!(" omit=none{}", tz_at(WINTER))),
            "{output}"
        );
        assert_eq!(records[2], day_record(WINTER), "{output}");
        assert!(records[4].starts_with("conversation "), "{output}");
        assert_eq!(records[5], day_record_after(SUMMER, WINTER), "{output}");
    }

    #[test]
    fn outline_of_summer_messages_names_their_offset_in_its_header() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            text_message_at(1, User, "Start the job", WINTER),
            text_message_at(2, User, "Check the job", SUMMER),
            text_message_at(3, Assistant, "It runs.", SUMMER),
        ]);
        let resolved = resolved("session.jsonl");
        let request = ReadRequest {
            range: Some(MessageRange { start: 2, end: 3 }),
            ..whole_session_request(&resolved, &transcript)
        };

        let output =
            format_outline_with_warnings(&request, options(), &[], TestClock::start_wall())
                .unwrap();

        let lines = output.lines().collect::<Vec<_>>();
        let tz = lines[0]
            .split_whitespace()
            .find(|field| field.starts_with("tz="));
        assert_eq!(tz, Some(tz_at(SUMMER).trim()), "{output}");
        assert_eq!(lines[2], day_record(SUMMER), "{output}");
    }

    #[test]
    fn short_outline_shows_each_message_in_read_shape_with_its_excerpt() {
        use AgentMessageRole::{Assistant, User};
        let at = |second: u32| format!("2026-10-14T15:41:{second:02}Z");
        let transcript = transcript(vec![
            text_message_at(1, User, "Run the tests", &at(20)),
            message(
                2,
                Assistant,
                vec![call_with_id(
                    "a",
                    "Bash",
                    "cargo test",
                    source_at(Assistant, &at(25)),
                )],
            ),
            message(
                3,
                User,
                vec![result_for("a", "ok", source_at(User, &at(31)))],
            ),
        ]);

        let output = format_outline(&resolved("session.jsonl"), &transcript, every_flag());

        let once = |second| record_time(&at(second), &at(second));
        assert_eq!(
            output.lines().skip(2).collect::<Vec<_>>(),
            [
                day_record(&at(20)),
                format!("m1 user{} chars=13 | Run the tests", once(20)),
                format!("m2{} chars=16 | Bash: cargo test", once(25)),
                format!("m3{} call=m2.1 tool=Bash chars=2 | ok", once(31)),
            ],
            "{output}"
        );
    }

    #[test]
    fn outline_names_each_sub_agent_in_legend_and_in_position_2_of_its_records() {
        let output = format_outline(
            &resolved("session.jsonl"),
            &session_with_a_scout(),
            every_flag(),
        );

        let records = records(&output);
        assert!(records[1].starts_with("conversation "), "{output}");
        assert_eq!(records[2], SCOUT_LEGEND, "{output}");
        assert!(
            records.contains(&"m3 s1 chars=17 | Reading the tree."),
            "{output}"
        );
        assert!(
            records
                .iter()
                .any(|record| record.starts_with("m7 report from=s1 chars=")),
            "{output}"
        );
        assert!(
            records.iter().any(|record| record.starts_with("m1 user ")),
            "{output}"
        );
    }

    /// m2 launches `alpha` and `beta`. m3 to m12 alternate between alpha's
    /// messages and beta's; m13 to m30 are the session's.
    #[test]
    fn outline_segment_names_sub_agents_whose_messages_it_covers() {
        use AgentMessageRole::{Assistant, User};
        let mut messages = vec![
            text_message(1, User, "Survey the repo"),
            message(
                2,
                Assistant,
                vec![
                    agent_call("toolu_alpha", "alpha", "Explore"),
                    agent_call("toolu_beta", "beta", "Explore"),
                ],
            ),
        ];
        messages.extend((3..=12).map(|ordinal| {
            let (label, launch) = if ordinal % 2 == 1 {
                ("alpha", "toolu_alpha")
            } else {
                ("beta", "toolu_beta")
            };
            subagent_message(
                ordinal,
                Assistant,
                vec![AgentMessagePart::Text {
                    text: format!("{label} reads part {ordinal}"),
                    source: source(Assistant),
                }],
                label,
                launch,
            )
        }));
        messages.extend((13..=30).map(|ordinal| text_message(ordinal, Assistant, "noted")));

        let output = format_outline(
            &resolved("session.jsonl"),
            &transcript(messages),
            every_flag(),
        );

        let records = records(&output);
        assert!(
            records[2].starts_with("subagent s1 type=Explore launch=m2 span=m3..m11 "),
            "{output}"
        );
        assert!(
            records[3].starts_with("subagent s2 type=Explore launch=m2 span=m4..m12 "),
            "{output}"
        );
        assert!(
            records
                .iter()
                .any(|record| record.starts_with("seg m1..m10 s1,s2 chars=")),
            "{output}"
        );
        assert!(
            records
                .iter()
                .any(|record| record.starts_with("seg m21..m30 chars=")),
            "{output}"
        );
    }

    fn outline_addresses(output: &str) -> Vec<&str> {
        output
            .lines()
            .filter(|line| line.starts_with('m'))
            .filter_map(|line| line.split_whitespace().next())
            .collect()
    }

    #[test]
    fn outline_of_range_holds_messages_of_range() {
        let transcript = session_with_a_scout();
        let resolved = resolved("session.jsonl");
        let request = ReadRequest {
            range: Some(MessageRange { start: 4, end: 7 }),
            ..whole_session_request(&resolved, &transcript)
        };

        let output =
            format_outline_with_warnings(&request, every_flag(), &[], TestClock::start_wall())
                .unwrap();

        assert_eq!(
            outline_addresses(&output),
            ["m4", "m5", "m6", "m7"],
            "{output}"
        );
    }

    #[test]
    fn outline_of_sub_agent_holds_its_launch_messages_and_report() {
        let transcript = session_with_a_scout();
        let resolved = resolved("session.jsonl");
        let request = ReadRequest {
            subagent: Some(1),
            ..whole_session_request(&resolved, &transcript)
        };

        let output =
            format_outline_with_warnings(&request, options(), &[], TestClock::start_wall())
                .unwrap();

        assert!(output.contains(" policy=text+subagents "), "{output}");
        assert!(output.contains(&format!("\n{SCOUT_LEGEND}\n")), "{output}");
        assert_eq!(outline_addresses(&output), ["m3", "m7"], "{output}");
    }

    /// The scout's outline holds its launch m2, its messages m3 to m5 and its
    /// report m7; m6 is not the scout's. Longer texts make omitting m4 to m7
    /// shorter than showing them.
    #[test]
    fn truncated_outline_of_sub_agent_continues_with_its_omitted_messages_alone() {
        let mut transcript = session_with_a_scout();
        for index in [3, 4, 6] {
            match &mut transcript.messages[index].parts[0] {
                AgentMessagePart::ToolUse { input, .. } => {
                    input["command"] = json!("ls ".repeat(30))
                }
                AgentMessagePart::ToolResult { content, .. } => {
                    *content = Some(json!("a.rs ".repeat(30)));
                }
                AgentMessagePart::Text { text, .. } => *text = text.repeat(4),
                AgentMessagePart::Thinking { .. } => {}
            }
        }
        let resolved = resolved("session.jsonl");
        let request = ReadRequest {
            subagent: Some(1),
            ..whole_session_request(&resolved, &transcript)
        };
        let outline_with_budget = |budget| {
            let options = ProtocolOptions {
                budget: Some(budget),
                ..every_flag()
            };
            format_outline_with_warnings(&request, options, &[], TestClock::start_wall()).unwrap()
        };

        let output = (100..2_000)
            .map(outline_with_budget)
            .find(|output| output.contains("\nm3 s1 "))
            .unwrap();

        let reference = resolved.reference.canonical();
        assert!(!output.contains("\nm4 s1 "), "{output}");
        assert!(
            output.ends_with(&format!(
                "\ncontinue read refs={reference}:m4..m5;{reference}:m7 tools=true tool-results=true thinking=true subagents=true\n"
            )),
            "{output}"
        );
    }

    #[test]
    fn outline_of_unknown_sub_agent_is_not_found() {
        let transcript = session_with_a_scout();
        let resolved = resolved("session.jsonl");
        let request = ReadRequest {
            subagent: Some(2),
            ..whole_session_request(&resolved, &transcript)
        };

        let error = format_outline_with_warnings(&request, options(), &[], TestClock::start_wall())
            .unwrap_err();

        assert!(
            matches!(
                &error,
                crate::error::AppError::Agent(AgentError {
                    kind: AgentErrorKind::NotFound,
                    ..
                })
            ),
            "{error}"
        );
    }

    /// m1 runs a command of two lines, m1.1, and `cargo clippy`, m1.2; m2
    /// and m3 hold their results.
    fn two_calls_and_their_results() -> AgentTranscript {
        use AgentMessageRole::{Assistant, User};
        let mut reply = text_message(1, Assistant, "Running the checks.");
        reply.parts.extend([
            call_with_id("a", "Bash", "cd src\ncargo test", source(Assistant)),
            call_with_id("b", "Bash", "cargo clippy", source(Assistant)),
        ]);
        transcript(vec![
            reply,
            message(2, User, vec![result_for("a", "ok", source(User))]),
            message(3, User, vec![result_for("b", "clean", source(User))]),
        ])
    }

    /// `agent read REF:m{ordinal}.{number}` of `transcript`.
    fn read_call(
        transcript: &AgentTranscript,
        ordinal: usize,
        number: usize,
        options: ProtocolOptions,
    ) -> Result<String> {
        let resolved = resolved("session.jsonl");
        let request = ReadRequest {
            range: Some(MessageRange::single(ordinal)),
            call: Some(number),
            ..whole_session_request(&resolved, transcript)
        };
        format_read(&[request], None, None, options)
    }

    #[test]
    fn read_of_one_call_shows_every_line_of_its_header_and_its_result() {
        let output = read_call(&two_calls_and_their_results(), 1, 1, options()).unwrap();

        assert!(
            output.starts_with(
                "protocol agent-read cut=none chars=6000 policy=text+tools+tool-results "
            ),
            "{output}"
        );
        assert!(
            output.ends_with(
                "\nm1.1 result=m2\n| Bash: cd src\n|   cargo test\nm2 call=m1.1\n| ok\n"
            ),
            "{output}"
        );
    }

    /// m1 and m3 each run a command of two lines; m2 and m4 hold their
    /// results.
    #[test]
    fn read_of_two_calls_shows_every_line_of_each_calls_header() {
        use AgentMessageRole::{Assistant, User};
        let transcript = transcript(vec![
            message(
                1,
                Assistant,
                vec![call_with_id(
                    "a",
                    "Bash",
                    "cd src\ncargo test",
                    source(Assistant),
                )],
            ),
            message(2, User, vec![result_for("a", "ok", source(User))]),
            message(
                3,
                Assistant,
                vec![call_with_id(
                    "b",
                    "Bash",
                    "cd docs\nmdbook build",
                    source(Assistant),
                )],
            ),
            message(4, User, vec![result_for("b", "built", source(User))]),
        ]);
        let resolved = resolved("session.jsonl");
        let first_call_of = |ordinal| ReadRequest {
            range: Some(MessageRange::single(ordinal)),
            call: Some(1),
            ..whole_session_request(&resolved, &transcript)
        };

        let output =
            format_read(&[first_call_of(1), first_call_of(3)], None, None, options()).unwrap();

        assert!(
            output.contains("\nm1.1 result=m2\n| Bash: cd src\n|   cargo test\n"),
            "{output}"
        );
        assert!(
            output.contains("\nm3.1 result=m4\n| Bash: cd docs\n|   mdbook build\n"),
            "{output}"
        );
    }

    /// m1 and m3 each run `cd src` then `cargo test`; m2 and m4 hold their
    /// results.
    fn one_command_run_twice() -> AgentTranscript {
        use AgentMessageRole::{Assistant, User};
        let run = |id| call_with_id(id, "Bash", "cd src\ncargo test", source(Assistant));
        transcript(vec![
            message(1, Assistant, vec![run("a")]),
            message(2, User, vec![result_for("a", "ok", source(User))]),
            message(3, Assistant, vec![run("b")]),
            message(4, User, vec![result_for("b", "ok", source(User))]),
        ])
    }

    #[test]
    fn call_read_alone_doesnt_repeat_abbreviated_copy() {
        let transcript = one_command_run_twice();
        let resolved = resolved("session.jsonl");
        let first_run = ReadRequest {
            range: Some(MessageRange { start: 1, end: 2 }),
            ..whole_session_request(&resolved, &transcript)
        };
        let second_call = ReadRequest {
            range: Some(MessageRange::single(3)),
            call: Some(1),
            ..whole_session_request(&resolved, &transcript)
        };
        let with_tools = ProtocolOptions {
            tools: true,
            ..options()
        };

        let output = format_read(&[first_run, second_call], None, None, with_tools).unwrap();

        assert!(
            output.contains("\nm1.1 result=m2\n| Bash: cd src (+1 line)\n"),
            "{output}"
        );
        assert!(
            output.contains("\nm3.1 result=m4\n| Bash: cd src\n|   cargo test\n"),
            "{output}"
        );
    }

    #[test]
    fn abbreviated_call_repeats_copy_read_alone() {
        let transcript = one_command_run_twice();
        let resolved = resolved("session.jsonl");
        let first_call = ReadRequest {
            range: Some(MessageRange::single(1)),
            call: Some(1),
            ..whole_session_request(&resolved, &transcript)
        };
        let second_run = ReadRequest {
            range: Some(MessageRange { start: 3, end: 4 }),
            ..whole_session_request(&resolved, &transcript)
        };
        let with_tools = ProtocolOptions {
            tools: true,
            ..options()
        };

        let output = format_read(&[first_call, second_run], None, None, with_tools).unwrap();

        assert!(
            output.contains("\nm1.1 result=m2\n| Bash: cd src\n|   cargo test\n"),
            "{output}"
        );
        assert!(output.contains("\nm3.1 result=m4 same=m1.1\n"), "{output}");
    }

    /// m1 holds text, a command of two lines and a long command of one line,
    /// m1.2: lines 1 to 4 of a read of m1.
    #[test]
    fn truncated_call_read_names_its_omitted_lines_as_read_of_its_message_numbers_them() {
        use AgentMessageRole::{Assistant, User};
        let long_command = format!("cargo clippy {}", "--warn unused ".repeat(60));
        let mut reply = text_message(1, Assistant, "Running the checks.");
        reply.parts.extend([
            call_with_id("a", "Bash", "cd src\ncargo test", source(Assistant)),
            call_with_id("b", "Bash", &long_command, source(Assistant)),
        ]);
        let transcript = transcript(vec![
            reply,
            message(2, User, vec![result_for("a", "ok", source(User))]),
            message(3, User, vec![result_for("b", "clean", source(User))]),
        ]);
        let resolved = resolved("session.jsonl");
        let flags = "tools=true tool-results=true thinking=false subagents=false";

        let truncated = read_call(
            &transcript,
            1,
            2,
            ProtocolOptions {
                budget: Some(500),
                ..options()
            },
        )
        .unwrap();
        let reread = format_read(
            &[ReadRequest {
                range: Some(MessageRange::single(1)),
                ..whole_session_request(&resolved, &transcript)
            }],
            None,
            Some(&ReadSlice::Lines(MessageLineRange { start: 4, end: 4 })),
            ProtocolOptions {
                tools: true,
                tool_results: true,
                ..options()
            },
        )
        .unwrap();

        let reference = resolved.reference.canonical();
        assert!(
            truncated.contains(&format!(
                "\nm1 omitted lines=4..4\nread ref={reference}:m1 lines=4..4 {flags}\n"
            )),
            "{truncated}"
        );
        assert!(
            reread.contains("\nm1.2 result=m3\n|  4: Bash: cargo clippy --warn unused "),
            "{reread}"
        );
    }

    #[test]
    fn read_of_call_past_its_messages_calls_is_out_of_range() {
        let error = read_call(&two_calls_and_their_results(), 1, 3, options()).unwrap_err();

        assert!(
            matches!(
                &error,
                crate::error::AppError::Agent(AgentError {
                    kind: AgentErrorKind::OutOfRange,
                    ..
                })
            ),
            "{error}"
        );
    }

    /// m3 is the scout's text, and m4.1 its call, answered in m5.
    #[test]
    fn read_of_one_message_or_one_call_shows_it_whoever_wrote_it() {
        let transcript = session_with_a_scout();

        let message = read(&transcript, Some(MessageRange::single(3)), options());
        let call = read_call(&transcript, 4, 1, options()).unwrap();

        assert!(message.contains(" policy=text+subagents "), "{message}");
        assert!(message.contains("\nm3 s1 anchor="), "{message}");
        assert!(message.ends_with("\n| Reading the tree.\n"), "{message}");
        assert!(
            call.ends_with("\nm4.1 s1 result=m5\n| Bash: ls\nm5 s1 call=m4.1\n| a.rs\n"),
            "{call}"
        );
    }
}
