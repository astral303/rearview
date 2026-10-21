//! The records of an `agent read` below its header, written in order with the
//! `conversation` and `day` records they need. An output leaves out what the
//! records above it show: a body repeating one of its owner's shown whole
//! above it, or a call repeating one showing at least as much, prints `same=`
//! in place of its lines when that is shorter, and `tool=` and `calls=` name
//! only calls without a record above them. A budget estimate writes every
//! record at full size.

use crate::agent::call_links::{AnsweredCall, Call, CallAddress};
use crate::agent::protocol::escape_atom;
use crate::agent::refs::ResolvedConversation;
use crate::agent::subagents::{Handle, SubagentTag};
use crate::agent::time_span::{DayRecords, RecordTime, TimeSpan, output_offset};
use crate::agent::visibility::ContentVisibility;
use chrono::{DateTime, FixedOffset, Utc};
use serde_json::Value;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

/// The size a [`RecordWriter`] writes records at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordSize {
    /// Without what the records above show.
    Shortened,
    /// Every record whole: the size a budget estimate counts.
    Full,
}

/// The fields of a message's own record: `m1021 01:35:31 call=m1006.15`.
pub struct MessageRecord<'r> {
    pub ordinal: usize,
    /// The sub-agent the message belongs to, in position 2.
    pub subagent: Option<&'r SubagentTag>,
    /// ` user`, ` report` or ` result`; empty for the agent's own message
    /// and for a tool result naming its call in `call=`.
    pub source_word: &'static str,
    pub time: Option<TimeSpan>,
    /// The sub-agent whose report or finish notice it carries: `from=`.
    pub from: Option<Handle>,
    /// The calls in other messages its results answer: `call=`, and `tool=`
    /// when the output does not print a record of one of them above.
    pub answered: &'r [AnsweredCall<'r>],
    /// The atoms after `same=`: ` hidden-…`, ` slice=…` and ` anchor=…`.
    pub trailing_atoms: String,
}

/// The fields of a call's record: `m1006.15 01:35:07 result=m1021`.
pub struct CallRecord<'e> {
    pub address: CallAddress,
    /// The sub-agent whose message holds the call, in position 2.
    pub subagent: Option<&'e SubagentTag>,
    pub time: Option<TimeSpan>,
    /// The sub-agent the call launches: `launches=`.
    pub launches: Option<Handle>,
    /// The running sub-agent the call sends to: `target=`.
    pub target: Option<Handle>,
    /// The messages other than its own holding its result: `result=`.
    pub results: &'e [usize],
    pub name: &'e str,
    pub input: &'e Value,
}

/// The part of a call its record's lines show, ordered from least to most.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CallCopy {
    /// Part of the call's lines: a budget or a slice cut them.
    Truncated,
    /// The header a read of several messages shows, its command shortened:
    /// `Bash: cd src (+1 line)`.
    Abbreviated,
    /// Every line of the call.
    Whole,
}

/// The fields of the record of a result in its call's own message:
/// `m5.1 result 10:00:00 tool=bash`.
pub struct CallResultRecord<'r> {
    pub call: Call<'r>,
    /// The sub-agent whose message holds the result, in position 2.
    pub subagent: Option<&'r SubagentTag>,
    pub time: Option<TimeSpan>,
}

/// The record of consecutive messages hidden whole:
/// `m3..m5 s2 hidden 11:41:32..11:41:40 tool-results=2 calls=m2.1..2`.
pub struct HiddenRecord<'r> {
    pub first: usize,
    pub last: usize,
    /// The sub-agent whose messages the run holds, in position 2.
    pub subagent: Option<&'r SubagentTag>,
    pub time: Option<TimeSpan>,
    /// The sub-agents its hidden calls launch: `launches=`.
    pub launches: &'r [Handle],
    /// The running sub-agents its hidden calls send to: `target=`.
    pub targets: &'r [Handle],
    /// ` tools=3 tool-results=12`.
    pub count_atoms: String,
    /// The calls outside the run that its hidden results answer: `calls=`.
    pub answered: &'r [CallAddress],
}

/// The fields of the records naming what a truncation leaves out of a
/// message's content, and the read that shows it:
/// `m1006 omitted lines=41..230 calls=m1006.4..15`, then
/// `read ref=ch_…:m1006 lines=41..230 tools=true tool-results=false …`.
pub struct OmittedRecord<'r> {
    pub ordinal: usize,
    /// The sub-agent the message belongs to, in position 2.
    pub subagent: Option<&'r SubagentTag>,
    /// The `ref=` of the message's conversation.
    pub reference: &'r str,
    /// The first and last omitted line, in the numbering `--lines` reads.
    pub first_line: usize,
    pub last_line: usize,
    /// The calls whose records the truncation drops: `calls=`.
    pub dropped_calls: &'r [CallAddress],
    /// The truncating read's flags; the `read` record repeats them.
    pub visibility: ContentVisibility,
}

pub struct RecordWriter<'e> {
    output: String,
    /// The `ref=` of the conversation the records belong to.
    conversation: String,
    days: DayRecords,
    /// The records the output shows of each conversation, by its `ref=`;
    /// `None` at full size.
    shown: Option<HashMap<String, Shown<'e>>>,
}

impl<'e> RecordWriter<'e> {
    /// Below an output's header.
    pub fn new(size: RecordSize) -> Self {
        Self {
            output: String::new(),
            conversation: String::new(),
            days: DayRecords::below_header(None),
            shown: (size == RecordSize::Shortened).then(HashMap::new),
        }
    }

    /// For one entry alone, from `first`, the time of its first record, at
    /// full size.
    pub fn measuring(first: Option<TimeSpan>) -> Self {
        Self {
            output: String::new(),
            conversation: String::new(),
            days: DayRecords::on(first.map(|span| RecordTime::of(span).day())),
            shown: None,
        }
    }

    /// The offset the header's `tz=` names: the output's first `day`
    /// record's, or the local offset at `written_at` without a `day` record.
    pub fn header_offset(&self, written_at: DateTime<Utc>) -> FixedOffset {
        self.days
            .header_offset()
            .unwrap_or_else(|| output_offset(None, written_at))
    }

    pub fn into_output(self) -> String {
        self.output
    }

    /// Write a `conversation` record, then the legend record of each of
    /// `subagents`, when `resolved` is another conversation than the last
    /// record's.
    pub fn enter_conversation(
        &mut self,
        resolved: &ResolvedConversation,
        subagents: &[&SubagentTag],
    ) {
        let canonical = resolved.reference.canonical();
        if self.conversation != canonical {
            self.output.push_str(&conversation_record(resolved));
            for subagent in subagents {
                self.output.push_str(subagent.legend());
            }
            self.days.restart();
            if let Some(shown) = self.shown.as_mut() {
                shown.entry(canonical.clone()).or_default();
            }
            self.conversation = canonical;
        }
    }

    /// Write a message's own record and `lines` below it. `whole_body`, a
    /// body shown whole, prints `same=` in place of its lines when it repeats
    /// a body shown whole above it and the atom is the shorter.
    pub fn write_message_record(
        &mut self,
        record: &MessageRecord<'_>,
        lines: &[&str],
        whole_body: Option<&'e str>,
    ) {
        let time = self.write_day_record(record.time);
        let same = match (self.shown_mut(), whole_body) {
            (Some(shown), Some(body)) => shown.repeats.record_first_body_copy(
                owner_of(record.subagent),
                body,
                record.ordinal,
            ),
            _ => None,
        }
        .map(|first| format!(" same=m{first}"))
        .filter(|same| is_shorter_than_content(same, lines));
        let names_tools = record
            .answered
            .iter()
            .flat_map(|call| &call.parts)
            .any(|&part| !self.has_call_record(part));
        self.output.push_str(&format!(
            "{}{}{}\n",
            message_record_start(record, &time, names_tools),
            same.as_deref().unwrap_or_default(),
            record.trailing_atoms
        ));
        if same.is_none() {
            self.write_content_lines(lines);
        }
    }

    /// Write a call's record and `lines`, `copy` of the call, below it. A
    /// call not truncated prints `same=` in place of its lines when it
    /// repeats a call above it showing at least as much and the atom is the
    /// shorter.
    pub fn write_call_record(&mut self, record: &CallRecord<'e>, lines: &[&str], copy: CallCopy) {
        let time = self.write_day_record(record.time);
        let first_copy = match self.shown_mut() {
            Some(shown) => {
                shown.call_records.insert(record.address);
                shown.repeats.record_first_call_copy(record, copy)
            }
            None => None,
        };
        let same = first_copy
            .map(|first| format!(" same={first}"))
            .filter(|same| is_shorter_than_content(same, lines));
        let results = record
            .results
            .iter()
            .map(|ordinal| format!("m{ordinal}"))
            .collect::<Vec<_>>();
        self.output.push_str(&format!(
            "{}{}{time}{}{}{}{}\n",
            record.address,
            subagent_field(record.subagent),
            handle_atom("launches", record.launches),
            handle_atom("target", record.target),
            list_atom("result", &results),
            same.as_deref().unwrap_or_default()
        ));
        if same.is_none() {
            self.write_content_lines(lines);
        }
    }

    /// Write the record of a result in its call's own message and `lines`
    /// below it, without `call=`: its address names its call. It carries
    /// `tool=` when the output does not print the call's record above it.
    pub fn write_call_result_record(&mut self, record: &CallResultRecord<'_>, lines: &[&str]) {
        let time = self.write_day_record(record.time);
        let call = record.call;
        let tool = if self.has_call_record(call.address) {
            String::new()
        } else {
            format!(" tool={}", escape_atom(call.tool))
        };
        self.output.push_str(&format!(
            "{}{} result{time}{tool}\n",
            call.address,
            subagent_field(record.subagent)
        ));
        self.write_content_lines(lines);
    }

    /// Write the record naming what a truncation leaves out of a message's
    /// content, then the `read` record showing it. `lines=` on the `read`
    /// record maps to `--lines`.
    pub fn write_omitted_records(&mut self, record: &OmittedRecord<'_>) {
        let lines = format!("{}..{}", record.first_line, record.last_line);
        self.output.push_str(&format!(
            "m{}{} omitted lines={lines}{}\n",
            record.ordinal,
            subagent_field(record.subagent),
            calls_atom(record.dropped_calls)
        ));
        self.output.push_str(&format!(
            "read ref={}:m{} lines={lines}{}\n",
            escape_atom(record.reference),
            record.ordinal,
            record.visibility.recipe_atoms()
        ));
    }

    pub fn write_read_record(
        &mut self,
        reference: &str,
        runs: &MessageRuns,
        flags: ContentVisibility,
    ) {
        self.output.push_str(&read_record(reference, runs, flags));
    }

    /// Write a hidden run's record. `calls=` names the answered calls without
    /// a record above it.
    pub fn write_hidden_record(&mut self, record: &HiddenRecord<'_>) {
        let time = self.write_day_record(record.time);
        let unrecorded = record
            .answered
            .iter()
            .copied()
            .filter(|&call| !self.has_call_record(call))
            .collect::<Vec<_>>();
        self.output.push_str(&format!(
            "{}{} hidden{time}{}{}{}{}\n",
            ordinal_range(record.first, record.last),
            subagent_field(record.subagent),
            handles_atom("launches", record.launches),
            handles_atom("target", record.targets),
            record.count_atoms,
            calls_atom(&unrecorded)
        ));
    }

    /// The records the output shows of the current conversation; `None` at
    /// full size.
    fn shown_mut(&mut self) -> Option<&mut Shown<'e>> {
        self.shown.as_mut()?.get_mut(&self.conversation)
    }

    /// True when the output shows `call` as a record of its own; false for
    /// every call at full size.
    fn has_call_record(&self, call: CallAddress) -> bool {
        self.shown
            .as_ref()
            .and_then(|shown| shown.get(&self.conversation))
            .is_some_and(|shown| shown.call_records.contains(&call))
    }

    fn write_content_lines(&mut self, lines: &[&str]) {
        for line in lines {
            push_content_line(&mut self.output, line);
        }
    }

    /// Write the `day` record a record on `span` needs, then return its
    /// positional time; empty without a time.
    fn write_day_record(&mut self, span: Option<TimeSpan>) -> String {
        let Some(span) = span else {
            return String::new();
        };
        let time = RecordTime::of(span);
        if let Some(day) = self.days.record_before(time.day()) {
            self.output.push_str(&day);
        }
        time.field()
    }
}

/// The records an output shows of one conversation.
#[derive(Default)]
struct Shown<'e> {
    repeats: Repeats<'e>,
    /// Each call printed as a record of its own.
    call_records: HashSet<CallAddress>,
}

/// The first copy shown whole of each message body and each call of each
/// owner, the session or one sub-agent: the target of a later copy's
/// `same=` from the same owner.
#[derive(Default)]
struct Repeats<'e> {
    bodies: HashMap<(Owner, &'e str), usize>,
    calls: HashMap<(Owner, &'e str, String), FirstCallCopy>,
}

#[derive(Clone, Copy)]
struct FirstCallCopy {
    address: CallAddress,
    copy: CallCopy,
}

/// The sub-agent whose messages a record holds; `None` for the session.
type Owner = Option<Handle>;

fn owner_of(subagent: Option<&SubagentTag>) -> Owner {
    subagent.map(|subagent| subagent.handle)
}

impl<'e> Repeats<'e> {
    /// Record message `ordinal`'s body as `owner`'s first copy of `body`, or
    /// return the message an earlier copy of `owner`'s is in.
    fn record_first_body_copy(
        &mut self,
        owner: Owner,
        body: &'e str,
        ordinal: usize,
    ) -> Option<usize> {
        match self.bodies.entry((owner, body)) {
            Entry::Occupied(first) => Some(*first.get()),
            Entry::Vacant(first) => {
                first.insert(ordinal);
                None
            }
        }
    }

    /// Record `call`, shown as `copy`, as its owner's first copy of its name
    /// and input, or return the address of an earlier copy of that owner's
    /// showing at least as much. A copy showing more replaces the first, so
    /// a call shown whole never names an abbreviated copy.
    fn record_first_call_copy(
        &mut self,
        call: &CallRecord<'e>,
        copy: CallCopy,
    ) -> Option<CallAddress> {
        if copy == CallCopy::Truncated {
            return None;
        }
        let key = (owner_of(call.subagent), call.name, call.input.to_string());
        let this_copy = FirstCallCopy {
            address: call.address,
            copy,
        };
        match self.calls.entry(key) {
            Entry::Occupied(mut first) if first.get().copy < copy => {
                first.insert(this_copy);
                None
            }
            Entry::Occupied(first) => Some(first.get().address),
            Entry::Vacant(first) => {
                first.insert(this_copy);
                None
            }
        }
    }
}

/// `m1021 user 01:35:31 call=m1006.15 tool=PowerShell`: the start of a
/// message's record, in a read or an outline. `time` is the positional time,
/// with its leading space. `names_tools` adds the answered calls' tools.
pub(crate) fn message_record_start(
    record: &MessageRecord<'_>,
    time: &str,
    names_tools: bool,
) -> String {
    let calls = record
        .answered
        .iter()
        .flat_map(|call| address_ranges(&call.parts))
        .collect::<Vec<_>>();
    let tools = record
        .answered
        .iter()
        .filter(|_| names_tools)
        .map(|call| escape_atom(call.tool))
        .collect::<Vec<_>>();
    format!(
        "m{}{}{}{time}{}{}{}",
        record.ordinal,
        subagent_field(record.subagent),
        record.source_word,
        handle_atom("from", record.from),
        list_atom("call", &calls),
        list_atom("tool", &tools)
    )
}

/// ` s326`: position 2 of a record of the sub-agent's; empty for the
/// session's own records.
fn subagent_field(subagent: Option<&SubagentTag>) -> String {
    handles_field(subagent.map(|subagent| subagent.handle).as_slice())
}

/// ` s326,s327`: the handles after the address of a record covering these
/// sub-agents' messages; empty for none.
pub(crate) fn handles_field(handles: &[Handle]) -> String {
    if handles.is_empty() {
        return String::new();
    }
    let handles = handles
        .iter()
        .map(|handle| format!("s{handle}"))
        .collect::<Vec<_>>();
    format!(" {}", handles.join(","))
}

/// ` name=s326`; empty without a sub-agent.
fn handle_atom(name: &str, handle: Option<Handle>) -> String {
    handles_atom(name, handle.as_slice())
}

/// ` name=s326,s327`; empty without sub-agents.
fn handles_atom(name: &str, handles: &[Handle]) -> String {
    let handles = handles
        .iter()
        .map(|handle| format!("s{handle}"))
        .collect::<Vec<_>>();
    list_atom(name, &handles)
}

/// `| line`, or `|` alone for an empty line.
fn push_content_line(output: &mut String, line: &str) {
    if line.is_empty() {
        output.push_str("|\n");
    } else {
        output.push_str("| ");
        output.push_str(line);
        output.push('\n');
    }
}

/// True when `atom` is shorter than `lines` as content lines: `same=` in
/// their place shortens the output, never lengthens it.
fn is_shorter_than_content(atom: &str, lines: &[&str]) -> bool {
    let mut content = String::new();
    for line in lines {
        push_content_line(&mut content, line);
    }
    atom.chars().count() < content.chars().count()
}

/// Runs of consecutive messages, each its first and last ordinal, in order.
/// The refs of a `read` or `continue read` record name each run, so a
/// selection with gaps, such as a sub-agent's messages, keeps them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MessageRuns(Vec<(usize, usize)>);

impl MessageRuns {
    /// Add messages `first` to `last`, after every run held; they extend the
    /// last run when they follow it.
    pub(crate) fn push(&mut self, first: usize, last: usize) {
        match self.0.last_mut() {
            Some((_, run_last)) if *run_last + 1 == first => *run_last = last,
            _ => self.0.push((first, last)),
        }
    }

    pub(crate) fn between(&self, first: usize, last: usize) -> Self {
        let mut runs = Self::default();
        for &(run_first, run_last) in &self.0 {
            if run_last >= first && run_first <= last {
                runs.push(run_first.max(first), run_last.min(last));
            }
        }
        runs
    }

    /// `ch_…:m3..m5;ch_…:m9`: a ref per run of `reference`'s messages.
    fn refs(&self, reference: &str) -> String {
        self.0
            .iter()
            .map(|&(first, last)| escape_atom(&message_range_ref(reference, first, last)))
            .collect::<Vec<_>>()
            .join(";")
    }
}

/// `read ref=ch_…:m7..m9 tools=true tool-results=false thinking=false
/// subagents=true`, or `read refs=ch_…:m3..m5;ch_…:m9 …` for several runs:
/// the read showing `runs` of the messages at `reference` with `flags`.
pub(crate) fn read_record(reference: &str, runs: &MessageRuns, flags: ContentVisibility) -> String {
    let key = if runs.0.len() == 1 { "ref" } else { "refs" };
    format!(
        "read {key}={}{}\n",
        runs.refs(reference),
        flags.recipe_atoms()
    )
}

/// `continue read refs=ch_…:m3..m5;ch_…:m9 tools=true tool-results=false
/// thinking=false subagents=false`: the read showing `runs` of the messages
/// at `reference` that a budget omitted, with `flags`.
pub(crate) fn continue_read_record(
    reference: &str,
    runs: &MessageRuns,
    flags: ContentVisibility,
) -> String {
    format!(
        "continue read refs={}{}\n",
        runs.refs(reference),
        flags.recipe_atoms()
    )
}

/// `ch_…:m7..m9`: the ref of messages `start` to `end` of the conversation at
/// `reference`.
pub(crate) fn message_range_ref(reference: &str, start: usize, end: usize) -> String {
    format!("{reference}:{}", ordinal_range(start, end))
}

pub(crate) fn conversation_record(resolved: &ResolvedConversation) -> String {
    format!(
        "conversation project={} uuid={} ref={}\n",
        escape_atom(&resolved.key.project_id()),
        escape_atom(&resolved.reference.uuid()),
        escape_atom(&resolved.reference.canonical())
    )
}

/// ` name=a,b`; empty without values.
fn list_atom(name: &str, values: &[String]) -> String {
    if values.is_empty() {
        String::new()
    } else {
        format!(" {name}={}", values.join(","))
    }
}

/// ` calls=m1006.1..3,m1009.2`: `calls` as [`address_ranges`]. Empty
/// without calls.
fn calls_atom(calls: &[CallAddress]) -> String {
    list_atom("calls", &address_ranges(calls))
}

/// `m1006.1..3`, `m1009.2`: `calls`, in order, each message's consecutive
/// calls as one range.
fn address_ranges(calls: &[CallAddress]) -> Vec<String> {
    let mut groups: Vec<(usize, usize, usize)> = Vec::new();
    for call in calls {
        match groups.last_mut() {
            Some((ordinal, _, last)) if *ordinal == call.ordinal && *last + 1 == call.number => {
                *last = call.number;
            }
            _ => groups.push((call.ordinal, call.number, call.number)),
        }
    }
    groups
        .iter()
        .map(|&(ordinal, first, last)| {
            if first == last {
                format!("m{ordinal}.{first}")
            } else {
                format!("m{ordinal}.{first}..{last}")
            }
        })
        .collect()
}

/// `m7`, or `m7..m9`.
pub(crate) fn ordinal_range(start: usize, end: usize) -> String {
    if start == end {
        format!("m{start}")
    } else {
        format!("m{start}..m{end}")
    }
}
