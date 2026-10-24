use crate::agent::protocol::{MessageLineRange, ProtocolOptions};
use crate::agent::refs::{AgentConversationKey, ResolvedConversation};
use crate::agent::time_span::{DayRecords, RecordTime, TimeSpan};
use crate::agent::transcript::{
    AgentMessage, AgentMessagePart, AgentMessageRole, AgentPartSource, AgentTranscript,
    agent_part_text,
};
use crate::clock::{TestClock, Zone};
use crate::history::environment::test_support::test_cache_base;
use crate::history::{Conversation, Source};
use crate::time_filter::TimeFilter;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use std::path::{Path, PathBuf};

/// Asserts that `row` counts `count` messages, as `transcript` numbers them,
/// and that each of `markers` sits in a semantic turn carrying the ordinal of
/// its transcript message.
pub fn assert_row_numbers_messages_as_transcript(
    row: &Conversation,
    transcript: &AgentTranscript,
    count: usize,
    markers: &[&str],
) {
    assert_eq!(
        (row.message_count, transcript.messages.len()),
        (count, count),
        "row and transcript message counts"
    );
    for marker in markers {
        let ordinal = transcript
            .messages
            .iter()
            .find(|message| {
                message
                    .parts
                    .iter()
                    .filter_map(agent_part_text)
                    .any(|text| text.contains(marker))
            })
            .unwrap_or_else(|| panic!("the transcript holds {marker:?}"))
            .ordinal;
        let turn = row
            .semantic_turns
            .iter()
            .position(|turn| turn.contains(marker))
            .unwrap_or_else(|| panic!("the row embeds {marker:?}"));
        assert_eq!(row.semantic_turn_ranges[turn].start, ordinal, "{marker}");
    }
}

/// [`assert_row_numbers_messages_as_transcript`] for the session at `path`,
/// read by `source`'s format and by `agent read`.
pub fn assert_row_numbers_messages_as_agent_read(
    source: Source,
    path: &Path,
    count: usize,
    markers: &[&str],
) {
    let row = source
        .provider()
        .format()
        .parse_conversation(
            path,
            crate::history::parser::RowDating::new(None, &crate::clock::TestClock::default()),
            None,
        )
        .unwrap()
        .expect("the session is listed");
    let transcript = AgentTranscript::load_owned(&test_cache_base(), source, path, &[]).unwrap();
    assert_row_numbers_messages_as_transcript(&row, &transcript, count, markers);
}

pub fn source(role: AgentMessageRole) -> AgentPartSource {
    AgentPartSource {
        role,
        timestamp: None,
        jsonl_line: 1,
        part_index: 0,
        assistant_message_id: None,
        parent_tool_use_id: None,
        tool_name: None,
    }
}

pub fn text_message(ordinal: usize, role: AgentMessageRole, text: &str) -> AgentMessage {
    AgentMessage {
        ordinal,
        role,
        is_report: false,
        report_sender: Default::default(),
        report_has_text: false,
        assistant_message_id: None,
        parent_tool_use_id: None,
        subagent: None,
        parts: vec![AgentMessagePart::Text {
            text: text.to_string(),
            source: source(role),
        }],
    }
}

/// [`text_message`] with its part written at `timestamp`.
pub fn text_message_at(
    ordinal: usize,
    role: AgentMessageRole,
    text: &str,
    timestamp: &str,
) -> AgentMessage {
    AgentMessage {
        parts: vec![AgentMessagePart::Text {
            text: text.to_string(),
            source: AgentPartSource {
                timestamp: Some(timestamp.to_owned()),
                ..source(role)
            },
        }],
        ..text_message(ordinal, role, text)
    }
}

/// When the agent outputs of tests are written: the test clock's start, in
/// the system's zone.
pub fn written_at() -> DateTime<Zone> {
    TestClock::start_wall().with_timezone(&Zone::System)
}

/// `timestamp` as `agent` output prints it in full: in local time, to the
/// second, without its offset.
pub fn local_time(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string()
}

/// The positional time of a read or outline record written from the UTC time
/// `first` to `last`, with its leading space: ` 11:41:28..11:41:30`.
pub fn record_time(first: &str, last: &str) -> String {
    let span = TimeSpan::of_timestamp(first)
        .unwrap()
        .union(TimeSpan::of_timestamp(last).unwrap());
    RecordTime::of(span, Zone::System).field()
}

/// The `day` record of the local date of the UTC time `utc`.
pub fn day_record(utc: &str) -> String {
    let day = RecordTime::of(TimeSpan::of_timestamp(utc).unwrap(), Zone::System).day();
    DayRecords::below_header(None)
        .record_before(day)
        .unwrap()
        .trim_end()
        .to_owned()
}

/// The value of a record line's `name=` atom.
pub fn atom<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix(name)?.strip_prefix('='))
}

/// A first and a last time, as instants: output that crosses an offset change
/// shows a later time at an earlier wall-clock time.
pub type RecordTimes = (DateTime<FixedOffset>, DateTime<FixedOffset>);

const DATE_TIME: &str = "%Y-%m-%dT%H:%M:%S";

/// A record line's `at=` time, and its `to=` time or else its `at=` time. A
/// time without an offset is in the offset `header`'s `tz=` names.
pub fn times(line: &str, header: &str) -> Option<RecordTimes> {
    let offset = header_offset(header);
    let time = |name| atom(line, name).map(|time| full_instant(time, offset).unwrap());
    let at = time("at")?;
    Some((at, time("to").unwrap_or(at)))
}

/// The offset a header's `tz=` names.
fn header_offset(header: &str) -> FixedOffset {
    atom(header, "tz")
        .and_then(|offset| offset.parse().ok())
        .unwrap_or_else(|| panic!("a header without a tz= offset: {header}"))
}

/// Panics when `line`'s `to=` time comes before its `at=` time, as [`times`]
/// reads them.
fn assert_times_in_order(line: &str, header: &str, output: &str) {
    if let Some((at, to)) = atom(line, "at").and_then(|_| times(line, header)) {
        assert!(at <= to, "a to= time before its at= time: {line}\n{output}");
    }
}

/// The first and last time of the message and call records of an `agent
/// read` output.
pub fn read_times(read: &str) -> Option<RecordTimes> {
    read_records(read)
        .into_iter()
        .filter(|record| record.word != Some("hidden"))
        .filter_map(|record| record.time)
        .reduce(|(first, last), (at, to)| (first.min(at), last.max(to)))
}

/// A record of `agent read` or `agent outline` that opens with its address:
/// `m7`, a message; `m7.2`, a call; `m7..m9`, a run of hidden messages.
#[derive(Debug)]
pub struct PositionalRecord<'a> {
    pub address: &'a str,
    /// The handle in position 2, `s326`, of the sub-agent whose messages
    /// the record holds.
    pub subagent: Option<&'a str>,
    /// `user`, `report`, `result`, `hidden` or `omitted`, after the address
    /// and any handles.
    pub word: Option<&'a str>,
    /// The time in position, completed with the date of the `day` record
    /// above it.
    pub time: Option<RecordTimes>,
    /// The `key=value` atoms after the time.
    pub atoms: Vec<&'a str>,
    /// The lines below a read's record, without their `| ` frame; the
    /// excerpt after an outline record's ` | `.
    pub content: Vec<&'a str>,
}

impl PositionalRecord<'_> {
    /// The value of the record's `name=` atom.
    pub fn atom(&self, name: &str) -> Option<&str> {
        self.atoms
            .iter()
            .find_map(|field| field.strip_prefix(name)?.strip_prefix('='))
    }
}

/// The records an output may print: a read's, or an outline's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Grammar {
    Read,
    Outline,
}

impl Grammar {
    /// The atoms after the time of a `kind` record, in the order the writer
    /// prints them; `None` for a kind the grammar does not print.
    fn record_atoms(self, kind: RecordKind) -> Option<&'static [OrderedAtom]> {
        match (self, kind) {
            (Self::Read, RecordKind::Message) => Some(READ_MESSAGE),
            (Self::Read, RecordKind::Call) => Some(CALL),
            (Self::Read, RecordKind::CallResult) => Some(CALL_RESULT),
            (Self::Read, RecordKind::Hidden) => Some(HIDDEN),
            (Self::Read, RecordKind::Omitted) => Some(OMITTED),
            (Self::Outline, RecordKind::Message) => Some(OUTLINE_MESSAGE),
            (Self::Outline, _) => None,
        }
    }

    /// The header's atoms after `protocol agent-read` or `protocol
    /// agent-outline`.
    fn header(self) -> (&'static str, &'static [OrderedAtom]) {
        match self {
            Self::Read => ("agent-read", READ_HEADER),
            Self::Outline => ("agent-outline", OUTLINE_HEADER),
        }
    }
}

/// True for a value an atom accepts.
type ValueCheck = fn(&str) -> bool;

/// An atom of a record whose atoms come in a fixed order: its name, the
/// values it accepts, and whether every such record carries it.
struct OrderedAtom {
    name: &'static str,
    values: ValueCheck,
    required: bool,
}

const fn required(name: &'static str, values: ValueCheck) -> OrderedAtom {
    OrderedAtom {
        name,
        values,
        required: true,
    }
}

const fn optional(name: &'static str, values: ValueCheck) -> OrderedAtom {
    OrderedAtom {
        name,
        values,
        required: false,
    }
}

const READ_HEADER: &[OrderedAtom] = &[
    required("cut", is_read_cut),
    required("chars", is_count_or_none),
    required("policy", is_policy),
    required("omit", is_omitted_runs),
    required("tz", is_offset),
    optional("messages-in-window", is_digits),
    optional("hidden-tools", is_digits),
    optional("hidden-tool-results", is_digits),
    optional("hidden-thinking", is_digits),
    optional("hidden-subagents", is_digits),
    optional("warnings", is_digits),
    optional("warning-records", is_digits),
];

const OUTLINE_HEADER: &[OrderedAtom] = &[
    required("cut", is_outline_cut),
    required("chars", is_count_or_none),
    required("policy", is_policy),
    required("messages", is_digits),
    optional("at", is_full_time),
    optional("to", is_full_time),
    required("tz", is_offset),
    optional("messages-in-window", is_digits),
    optional("omitted-records", is_digits),
    optional("warnings", is_digits),
    optional("warning-records", is_digits),
];

const WARNING: &[OrderedAtom] = &[
    required("kind", is_atom),
    optional("ref", is_atom),
    optional("count", is_digits),
    optional("detail", is_atom),
];

const CONVERSATION: &[OrderedAtom] = &[
    required("project", is_atom),
    required("uuid", is_atom),
    required("ref", is_conversation_ref),
];

/// The atoms after a legend record's handle, before ` : ` and the name.
const LEGEND: &[OrderedAtom] = &[
    optional("type", is_atom),
    optional("launch", is_message),
    required("span", is_message_or_run),
    optional("report", is_message_list),
    optional("in", is_handle),
];

const CONTINUE_READ: &[OrderedAtom] = &[
    required("refs", is_message_runs),
    required("tools", is_flag),
    required("tool-results", is_flag),
    required("thinking", is_flag),
    required("subagents", is_flag),
];

/// A `read` record of one run of messages, with the lines a truncation
/// omitted when it follows an `omitted` record.
const READ_ONE_RUN: &[OrderedAtom] = &[
    required("ref", is_message_run),
    optional("lines", is_line_range),
    required("tools", is_flag),
    required("tool-results", is_flag),
    required("thinking", is_flag),
    required("subagents", is_flag),
];

const READ_RUNS: &[OrderedAtom] = &[
    required("refs", is_message_runs),
    required("tools", is_flag),
    required("tool-results", is_flag),
    required("thinking", is_flag),
    required("subagents", is_flag),
];

/// A message's record in a read: `m3 s1 01:50:01 call=m2.1..2 tool=Agent`.
/// `slice=match=…` is followed by `context=` and `hits=`.
const READ_MESSAGE: &[OrderedAtom] = &[
    optional("from", is_handle),
    optional("call", is_call_list),
    optional("tool", is_atom_list),
    optional("same", is_message),
    optional("hidden-tools", is_atom_list),
    optional("hidden-tool-results", is_digits),
    optional("hidden-thinking", is_digits),
    optional("slice", is_slice),
    optional("context", is_digits),
    optional("hits", is_digits),
    optional("anchor", is_atom),
];

/// A message's record in an outline: `m12 user 14:30:12 chars=340`.
const OUTLINE_MESSAGE: &[OrderedAtom] = &[
    optional("from", is_handle),
    optional("call", is_call_list),
    optional("tool", is_atom_list),
    required("chars", is_digits),
];

/// A call's record: `m2.1 01:50:00 launches=s1 result=m4,m5`.
const CALL: &[OrderedAtom] = &[
    optional("launches", is_handle),
    optional("target", is_handle),
    optional("result", is_message_list),
    optional("same", is_call),
];

/// The record of a result in its call's own message: `m5.1 result tool=bash`.
const CALL_RESULT: &[OrderedAtom] = &[optional("tool", is_atom)];

/// The record of messages hidden whole: `m7..m8 hidden tools=1 calls=m2.3`.
const HIDDEN: &[OrderedAtom] = &[
    optional("launches", is_handle_list),
    optional("target", is_handle_list),
    optional("tools", is_digits),
    optional("tool-results", is_digits),
    optional("thinking", is_digits),
    optional("subagents", is_digits),
    optional("calls", is_call_list),
];

/// The record of what a truncation left out of a message:
/// `m1006 omitted lines=41..230 calls=m1006.4..15`.
const OMITTED: &[OrderedAtom] = &[
    required("lines", is_line_range),
    optional("calls", is_call_list),
];

/// The records of a read that open with an address, each with the content
/// lines below it. See [`positional_records`] for the lines it rejects.
pub fn read_records(output: &str) -> Vec<PositionalRecord<'_>> {
    positional_records(output, Grammar::Read)
}

/// The records of an outline that open with an address, each with the
/// excerpt after its ` | ` as its content.
pub fn outline_records(output: &str) -> Vec<PositionalRecord<'_>> {
    positional_records(output, Grammar::Outline)
}

/// The records that open with an address, each with the content lines below
/// it. Panics on a content line below no such record, a line that is neither
/// a record `grammar` defines nor content, a record that does not parse as
/// [`positional_record`] reads it, an outline's `seg` record that does not
/// parse as [`segment_record`] reads it, any other record that does not
/// parse as [`day_record_fields`] or [`unaddressed_record`] reads it, and a
/// header whose `to=` time comes before its `at=` time.
fn positional_records(output: &str, grammar: Grammar) -> Vec<PositionalRecord<'_>> {
    let mut records: Vec<PositionalRecord<'_>> = Vec::new();
    let header_prefix = format!("protocol {} ", grammar.header().0);
    let mut date = None;
    let mut offset = None;
    for line in output.lines() {
        if grammar == Grammar::Outline && line.starts_with(SEGMENT_PREFIX) {
            segment_record(line, output);
        } else if let Some(content) = line.strip_prefix("| ").or((line == "|").then_some("")) {
            records
                .last_mut()
                .unwrap_or_else(|| panic!("content below no record: {line}\n{output}"))
                .content
                .push(content);
        } else if let Some(day) = line.strip_prefix("day ") {
            let (day_date, day_offset) = day_record_fields(day)
                .unwrap_or_else(|| panic!("a day record that does not parse: {line}\n{output}"));
            date = Some(day_date);
            offset = day_offset.or(offset);
        } else if line
            .strip_prefix('m')
            .is_some_and(|rest| rest.starts_with(|second: char| second.is_ascii_digit()))
        {
            let day = date.zip(offset);
            records.push(positional_record(line, day, grammar, output));
        } else {
            unaddressed_record(line, grammar, output);
            if line.starts_with(&header_prefix) {
                offset = Some(header_offset(line));
                assert_times_in_order(line, line, output);
            } else if line.starts_with("conversation ") {
                date = None;
            }
        }
    }
    records
}

/// The fields of a `day` record: its date, `2026-11-01`, then the offset
/// from it on when it changes, `tz=-05:00`; `None` for any other fields.
fn day_record_fields(fields: &str) -> Option<(NaiveDate, Option<FixedOffset>)> {
    let (date, offset) = match fields.split_once(' ') {
        Some((date, offset)) => (date, Some(offset)),
        None => (fields, None),
    };
    let offset = match offset {
        Some(offset) => Some(offset.strip_prefix("tz=")?.parse().ok()?),
        None => None,
    };
    let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some((date, offset))
}

/// `line`, a record without an address: a `protocol` header or warning, a
/// `conversation` record, a legend record, a `continue read` record, or in a
/// read a `read` record. Panics on any other line, and on a record whose
/// atoms are not the ones its kind carries, in order, with the values each
/// accepts.
fn unaddressed_record(line: &str, grammar: Grammar, output: &str) {
    let (kind, fields) = line.split_once(' ').unwrap_or((line, ""));
    let parses = match kind {
        "protocol" => protocol_record(fields, grammar),
        "conversation" => atoms_in_order(fields, CONVERSATION),
        "subagent" => legend_record(fields),
        "continue" => fields
            .strip_prefix("read ")
            .is_some_and(|atoms| atoms_in_order(atoms, CONTINUE_READ)),
        "read" if grammar == Grammar::Read => {
            atoms_in_order(fields, READ_ONE_RUN) || atoms_in_order(fields, READ_RUNS)
        }
        _ => panic!("a line that is neither a record nor content: {line}\n{output}"),
    };
    assert!(
        parses,
        "a {kind} record that does not parse: {line}\n{output}"
    );
}

/// The fields of a `protocol` record: `grammar`'s header, or a warning.
fn protocol_record(fields: &str, grammar: Grammar) -> bool {
    let (name, atoms) = fields.split_once(' ').unwrap_or((fields, ""));
    let (header, header_atoms) = grammar.header();
    if name == header {
        atoms_in_order(atoms, header_atoms)
    } else {
        name == "agent-warning" && atoms_in_order(atoms, WARNING)
    }
}

/// The fields of a legend record: `s3 type=Explore launch=m2 span=m3..m9
/// report=m10 : scout`.
fn legend_record(fields: &str) -> bool {
    let Some((atoms, _name)) = fields.split_once(" : ") else {
        return false;
    };
    let (handle, atoms) = atoms.split_once(' ').unwrap_or((atoms, ""));
    is_handle(handle) && atoms_in_order(atoms, LEGEND)
}

/// True when `fields`, separated by single spaces, are `atoms` in order,
/// each optional one present or not, each value one its atom accepts.
fn atoms_in_order(fields: &str, atoms: &[OrderedAtom]) -> bool {
    first_atom_mismatch(fields.split_terminator(' '), atoms).is_none()
}

/// A way a record's fields fail to be its atoms in order.
enum AtomMismatch<'a> {
    /// A field holding a value its atom does not accept.
    Value(&'a str),
    /// The name of a required atom the fields lack.
    Missing(&'static str),
    /// A field that is not the next atom the order allows.
    OutOfPlace(&'a str),
}

/// The first way `fields` fail to be `atoms` in order, each optional one
/// present or not; `None` when they are.
fn first_atom_mismatch<'a>(
    fields: impl Iterator<Item = &'a str>,
    atoms: &[OrderedAtom],
) -> Option<AtomMismatch<'a>> {
    let mut fields = fields.peekable();
    for atom in atoms {
        let field = fields.next_if(|field| {
            field
                .split_once('=')
                .is_some_and(|(name, _)| name == atom.name)
        });
        match field {
            Some(field) if !(atom.values)(&field[atom.name.len() + 1..]) => {
                return Some(AtomMismatch::Value(field));
            }
            None if atom.required => return Some(AtomMismatch::Missing(atom.name)),
            _ => {}
        }
    }
    fields.next().map(AtomMismatch::OutOfPlace)
}

/// An outline's `seg` record: the run of messages it covers, `m1..m20`, and
/// the handles of the sub-agents whose messages it holds.
#[derive(Debug)]
pub struct SegmentRecord<'a> {
    pub address: &'a str,
    pub subagents: Vec<&'a str>,
}

const SEGMENT_PREFIX: &str = "seg ";

/// The `seg` records of an outline. See [`segment_record`] for the segments
/// it rejects.
pub fn segment_records(output: &str) -> Vec<SegmentRecord<'_>> {
    output
        .lines()
        .filter(|line| line.starts_with(SEGMENT_PREFIX))
        .map(|line| segment_record(line, output))
        .collect()
}

/// `line`, a segment: `seg m1..m20 s3,s4 chars=900 at=… to=… | first /
/// last`, the handles and times present when the outline has them. Panics
/// on an address that is not a run of messages, a missing or malformed
/// `chars=`, a time that does not parse, a `to=` without `at=` or before
/// it, any other token, and an excerpt without its first and last snippet.
fn segment_record<'a>(line: &'a str, output: &str) -> SegmentRecord<'a> {
    let line_fields = &line[SEGMENT_PREFIX.len()..];
    let (fields, excerpt) = match line_fields.split_once(" | ") {
        Some((fields, excerpt)) => (fields, Some(excerpt)),
        None => (line_fields, None),
    };
    let mut fields = fields.split(' ').peekable();
    let address = fields.next().unwrap();
    assert!(
        matches!(AddressKind::of(address), Some(AddressKind::Run)),
        "a segment without a run of messages: {line}\n{output}"
    );
    let subagents = fields
        .next_if(|field| field.split(',').all(is_handle))
        .map(|handles| handles.split(',').collect())
        .unwrap_or_default();
    let chars = fields.next().and_then(|field| field.strip_prefix("chars="));
    assert!(
        chars.is_some_and(is_digits),
        "a segment without chars=: {line}\n{output}"
    );
    let mut reads_time = |name: &str| {
        let Some(field) = fields.next_if(|field| field.starts_with(name)) else {
            return false;
        };
        assert!(
            full_time(&field[name.len()..]).is_some(),
            "a segment time that does not parse: {line}\n{output}"
        );
        true
    };
    if reads_time("at=") {
        reads_time("to=");
        assert_times_in_order(line, outline_header(output), output);
    }
    let rest = fields.collect::<Vec<_>>();
    assert!(
        rest.is_empty(),
        "a token after a segment's fields: {rest:?} in {line}\n{output}"
    );
    assert!(
        excerpt.is_some_and(|excerpt| excerpt.contains(" / ")),
        "a segment without its first and last snippet: {line}\n{output}"
    );
    SegmentRecord { address, subagents }
}

fn outline_header(output: &str) -> &str {
    let prefix = format!("protocol {} ", Grammar::Outline.header().0);
    output
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("an outline without its header:\n{output}"))
}

/// `line`, a record opening with an address, below a `day` record stating
/// `day`'s date and offset. Panics on an address that is not `m7`, `m7.2` or
/// `m7..m9`; a word the address may not carry; a kind of record `grammar`
/// does not print; a positional time that does not parse, ends before it
/// starts or stands below no `day` record; atoms after it other than its
/// kind's, in order, each holding a value its atom accepts; and an excerpt
/// after ` | ` on a read's record.
fn positional_record<'a>(
    line: &'a str,
    day: Option<(NaiveDate, FixedOffset)>,
    grammar: Grammar,
    output: &str,
) -> PositionalRecord<'a> {
    let (fields, excerpt) = match line.split_once(" | ") {
        Some((fields, excerpt)) => {
            assert_eq!(
                grammar,
                Grammar::Outline,
                "an excerpt on a read's record: {line}\n{output}"
            );
            (fields, Some(excerpt))
        }
        None => (line, None),
    };
    let mut fields = fields.split_whitespace().peekable();
    let address = fields.next().unwrap();
    let address_kind = AddressKind::of(address)
        .unwrap_or_else(|| panic!("a record without an address: {line}\n{output}"));
    let subagent = fields.next_if(|field| is_handle(field));
    let word = fields.next_if(|field| {
        field.starts_with(|first: char| first.is_ascii_lowercase()) && !field.contains('=')
    });
    let kind = RecordKind::of(address_kind, word)
        .unwrap_or_else(|| panic!("a word the record may not carry: {line}\n{output}"));
    let kind_atoms = grammar.record_atoms(kind).unwrap_or_else(|| {
        panic!("a {kind:?} record the {grammar:?} grammar does not print: {line}\n{output}")
    });
    let time = fields
        .next_if(|field| field.starts_with(|first: char| first.is_ascii_digit()))
        .map(|time| {
            let (date, offset) =
                day.unwrap_or_else(|| panic!("a time below no day record: {line}\n{output}"));
            let (first, last) = positional_time(time, date, offset);
            assert!(
                first <= last,
                "a time span ending before it starts: {line}\n{output}"
            );
            (first, last)
        });
    let atoms = fields.collect::<Vec<_>>();
    match first_atom_mismatch(atoms.iter().copied(), kind_atoms) {
        None => {}
        Some(AtomMismatch::Value(atom)) => {
            panic!("an atom value that does not parse: {atom} in {line}\n{output}")
        }
        Some(AtomMismatch::Missing(name)) => {
            panic!("a {kind:?} record without {name}=: {line}\n{output}")
        }
        Some(AtomMismatch::OutOfPlace(atom)) => panic!(
            "a token that is not an atom of a {kind:?} record in its place: {atom} in {line}\n{output}"
        ),
    }
    PositionalRecord {
        address,
        subagent,
        word,
        time,
        atoms,
        content: excerpt.into_iter().collect(),
    }
}

/// True for `s326`: the handle position 2 of a record holds.
fn is_handle(field: &str) -> bool {
    field.strip_prefix('s').is_some_and(is_digits)
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_handle_list(value: &str) -> bool {
    value.split(',').all(is_handle)
}

fn is_count_or_none(value: &str) -> bool {
    value == "none" || is_digits(value)
}

fn is_flag(value: &str) -> bool {
    matches!(value, "true" | "false")
}

/// True for an offset as `tz=` names it: `-04:00`.
fn is_offset(value: &str) -> bool {
    value.parse::<chrono::FixedOffset>().is_ok()
}

fn is_full_time(value: &str) -> bool {
    full_time(value).is_some()
}

/// True for a value [`escape_atom`](crate::agent::protocol::escape_atom)
/// writes: unreserved characters and `%XX` escapes.
fn is_atom(value: &str) -> bool {
    let is_unreserved = |text: &str| {
        text.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._~:/+-".contains(&byte))
    };
    let mut parts = value.split('%');
    let unescaped_start = parts.next().unwrap_or_default();
    !value.is_empty()
        && is_unreserved(unescaped_start)
        && parts.all(|part| {
            part.len() >= 2
                && part.as_bytes()[..2].iter().all(u8::is_ascii_hexdigit)
                && is_unreserved(&part[2..])
        })
}

fn is_atom_list(value: &str) -> bool {
    value.split(',').all(is_atom)
}

/// True for `text`, then `+` and each part shown: `text+tools+subagents`.
fn is_policy(value: &str) -> bool {
    let mut parts = value.split('+');
    parts.next() == Some("text")
        && parts.all(|part| matches!(part, "tools" | "tool-results" | "thinking" | "subagents"))
}

/// True for `none`, or the parts of a read a budget cut joined by `+`:
/// `body+tail`.
fn is_read_cut(value: &str) -> bool {
    value == "none"
        || value
            .split('+')
            .all(|part| matches!(part, "body" | "head" | "focus" | "tail"))
}

fn is_outline_cut(value: &str) -> bool {
    matches!(value, "none" | "tail")
}

/// True for `ch_` and hex digits: a conversation's `ref=`.
fn is_conversation_ref(value: &str) -> bool {
    value.strip_prefix("ch_").is_some_and(|digest| {
        !digest.is_empty() && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn is_message(value: &str) -> bool {
    matches!(AddressKind::of(value), Some(AddressKind::Message))
}

fn is_message_or_run(value: &str) -> bool {
    matches!(
        AddressKind::of(value),
        Some(AddressKind::Message | AddressKind::Run)
    )
}

fn is_call(value: &str) -> bool {
    matches!(AddressKind::of(value), Some(AddressKind::Call))
}

fn is_message_list(value: &str) -> bool {
    value.split(',').all(is_message)
}

/// True for calls, each `m7.2`, or consecutive calls of one message in
/// order, `m7.2..4`, separated by commas.
fn is_call_list(value: &str) -> bool {
    value.split(',').all(|calls| match calls.split_once("..") {
        Some((first, last_number)) => {
            is_call(first)
                && first
                    .split_once('.')
                    .is_some_and(|(_, first_number)| is_ordered(first_number, last_number))
        }
        None => is_call(calls),
    })
}

/// True for 1-based numbers `first` and `last` in digits, `first` not above
/// `last`.
fn is_ordered(first: &str, last: &str) -> bool {
    one_based(first)
        .zip(one_based(last))
        .is_some_and(|(first, last)| first <= last)
}

/// The number `digits` writes, when it is 1 or more: addresses count
/// messages and calls from 1, as the ref parser does.
fn one_based(digits: &str) -> Option<usize> {
    is_digits(digits)
        .then(|| digits.parse::<usize>().ok())
        .flatten()
        .filter(|&number| number > 0)
}

/// True for `ch_…:m7` or `ch_…:m7..m9`: messages of a conversation.
fn is_message_run(value: &str) -> bool {
    value.split_once(':').is_some_and(|(conversation, run)| {
        is_conversation_ref(conversation) && is_message_or_run(run)
    })
}

/// True for message runs, as [`is_message_run`] reads them, separated by
/// `;`.
fn is_message_runs(value: &str) -> bool {
    value.split(';').all(is_message_run)
}

/// True for `none`, or a header's omitted runs separated by commas, each
/// qualified with its conversation when the read holds several:
/// `m3..m5,m9` or `ch_…:m3..m5,ch_…:m9`.
fn is_omitted_runs(value: &str) -> bool {
    value == "none"
        || value
            .split(',')
            .all(|run| is_message_or_run(run) || is_message_run(run))
}

/// True for `2..9`: lines `--lines` accepts.
fn is_line_range(value: &str) -> bool {
    value.parse::<MessageLineRange>().is_ok()
}

/// True for `lines=2..4` or `match=QUERY`.
fn is_slice(value: &str) -> bool {
    value.strip_prefix("lines=").is_some_and(is_line_range)
        || value.strip_prefix("match=").is_some_and(is_atom)
}

/// The target of a record's address.
#[derive(Clone, Copy, Debug)]
enum AddressKind {
    /// `m7`.
    Message,
    /// `m7.2`.
    Call,
    /// `m7..m9`.
    Run,
}

impl AddressKind {
    /// The kind of `address`; `None` for text that is not an address, a
    /// message or call numbered 0, and a run ending before it starts.
    fn of(address: &str) -> Option<Self> {
        let is_ordinal = |text: &str| text.strip_prefix('m').and_then(one_based).is_some();
        if let Some((first, last)) = address.split_once("..") {
            let is_run = first
                .strip_prefix('m')
                .zip(last.strip_prefix('m'))
                .is_some_and(|(first, last)| is_ordered(first, last));
            return is_run.then_some(Self::Run);
        }
        match address.split_once('.') {
            Some((message, number)) => {
                (is_ordinal(message) && one_based(number).is_some()).then_some(Self::Call)
            }
            None => is_ordinal(address).then_some(Self::Message),
        }
    }
}

/// The kind of a record opening with an address, by that address and the
/// word after it; each kind has its own atoms.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordKind {
    /// `m7`, after it a source word or none.
    Message,
    /// `m7.2`.
    Call,
    /// `m7.2 result`: the result in the call's own message.
    CallResult,
    /// `m7 hidden` or `m7..m9 hidden`.
    Hidden,
    /// `m7 omitted`.
    Omitted,
}

impl RecordKind {
    /// `None` for a word the address may not carry.
    fn of(address: AddressKind, word: Option<&str>) -> Option<Self> {
        match (address, word) {
            (AddressKind::Message, None | Some("user" | "report" | "result")) => {
                Some(Self::Message)
            }
            (AddressKind::Message | AddressKind::Run, Some("hidden")) => Some(Self::Hidden),
            (AddressKind::Message, Some("omitted")) => Some(Self::Omitted),
            (AddressKind::Call, None) => Some(Self::Call),
            (AddressKind::Call, Some("result")) => Some(Self::CallResult),
            _ => None,
        }
    }
}

/// `hh:mm:ss`, `hh:mm:ss..hh:mm:ss` or `hh:mm:ss..YYYY-MM-DDThh:mm:ss` on
/// `date` in `offset`; a full end carries its own offset when it is in
/// another.
fn positional_time(time: &str, date: NaiveDate, offset: FixedOffset) -> RecordTimes {
    let clock = |clock: &str| {
        let local = date.and_time(NaiveTime::parse_from_str(clock, "%H:%M:%S").unwrap());
        instant(local, offset)
    };
    let (first, last) = time.split_once("..").unwrap_or((time, time));
    (
        clock(first),
        full_instant(last, offset).unwrap_or_else(|| clock(last)),
    )
}

/// `YYYY-MM-DDThh:mm:ss`, then the offset after it when it has one; `None`
/// for text that does not open with a date and time. Panics on a malformed
/// offset.
fn full_time(time: &str) -> Option<(NaiveDateTime, Option<FixedOffset>)> {
    let (local, offset) = NaiveDateTime::parse_and_remainder(time, DATE_TIME).ok()?;
    let offset = (!offset.is_empty()).then(|| {
        offset
            .parse()
            .unwrap_or_else(|_| panic!("a full time with a malformed offset: {time}"))
    });
    Some((local, offset))
}

/// A full time as [`full_time`] reads it, in its own offset or else in
/// `offset`.
fn full_instant(time: &str, offset: FixedOffset) -> Option<DateTime<FixedOffset>> {
    let (local, own_offset) = full_time(time)?;
    Some(instant(local, own_offset.unwrap_or(offset)))
}

fn instant(local: NaiveDateTime, offset: FixedOffset) -> DateTime<FixedOffset> {
    local
        .and_local_timezone(offset)
        .single()
        .expect("a fixed offset maps each local time to one instant")
}

/// Every part shown, without a budget or a time window.
pub const EVERY_PART: ProtocolOptions = ProtocolOptions {
    budget: None,
    tools: true,
    tool_results: true,
    thinking: true,
    subagents: true,
    window: TimeFilter {
        after: None,
        before: None,
    },
};

/// `row` resolved under the project `project`.
pub fn resolved_conversation(row: &Conversation) -> ResolvedConversation {
    let file_name = row.path.file_name().unwrap().to_string_lossy();
    let key = AgentConversationKey::new("project", file_name, row.path.clone());
    ResolvedConversation {
        reference: key.conversation_ref(),
        key,
    }
}

pub fn user_jsonl_line(text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "timestamp": "2024-01-01T00:00:00Z",
        "message": {"role": "user", "content": text}
    })
    .to_string()
}

pub fn assistant_jsonl_line(text: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "timestamp": "2024-01-01T00:00:01Z",
        "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
    })
    .to_string()
}

pub fn transcript(messages: Vec<AgentMessage>, path: &str) -> AgentTranscript {
    AgentTranscript {
        path: PathBuf::from(path),
        messages,
        malformed_lines: Vec::new(),
        summary: None,
        custom_title: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "protocol agent-read cut=none chars=none policy=text omit=none tz=-04:00\nconversation project=pr_0 uuid=u ref=ch_0\nday 2026-10-14\n";
    const OUTLINE_HEADER: &str = "protocol agent-outline cut=none chars=none policy=text messages=30 tz=-04:00\nconversation project=pr_0 uuid=u ref=ch_0\nday 2026-10-14\n";

    #[test]
    fn read_parses_into_its_records_and_their_content() {
        let output = format!(
            "{HEADER}m1 user 11:41:25\n| Run the tests\nm1.1 11:41:26 result=m2\n| Bash: cargo test\nm2 11:41:30 call=m1.1\n| ok\nm3..m4 hidden tools=1\n"
        );

        let records = read_records(&output);

        let addresses = records
            .iter()
            .map(|record| (record.address, record.word, record.content.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            addresses,
            [
                ("m1", Some("user"), vec!["Run the tests"]),
                ("m1.1", None, vec!["Bash: cargo test"]),
                ("m2", None, vec!["ok"]),
                ("m3..m4", Some("hidden"), vec![]),
            ]
        );
        assert_eq!(records[2].atom("call"), Some("m1.1"));
    }

    #[test]
    fn truncated_messages_omitted_record_parses_and_its_read_record_is_not_content() {
        let output = format!(
            "{HEADER}m1 11:41:25\n| first line\nm1 omitted lines=2..9 calls=m1.1\nread ref=ch_0:m1 lines=2..9 tools=true tool-results=false thinking=false subagents=false\n"
        );

        let records = read_records(&output);

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].content, ["first line"]);
        assert_eq!(
            (
                records[1].word,
                records[1].atom("lines"),
                records[1].atom("calls")
            ),
            (Some("omitted"), Some("2..9"), Some("m1.1"))
        );
        assert!(records[1].content.is_empty());
    }

    fn rfc_3339(time: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(time).unwrap()
    }

    #[test]
    fn spans_end_in_another_offset_parses_as_instant_it_shows() {
        let output = "protocol agent-read cut=none chars=none policy=text omit=none tz=-04:00\nconversation project=pr_0 uuid=u ref=ch_0\nday 2026-11-01\nm1 01:50:00..2026-11-01T01:10:00-05:00\n| first line\n";

        let records = read_records(output);

        assert_eq!(
            records[0].time,
            Some((
                rfc_3339("2026-11-01T01:50:00-04:00"),
                rfc_3339("2026-11-01T01:10:00-05:00")
            ))
        );
    }

    /// Clocks fall back from 02:00 at -04:00 to 01:00 at -05:00: m2's
    /// wall-clock time is earlier than m1's.
    #[test]
    fn reads_first_and_last_time_are_its_earliest_and_latest_instants_across_offset_change() {
        let output = "protocol agent-read cut=none chars=none policy=text omit=none tz=-04:00\nconversation project=pr_0 uuid=u ref=ch_0\nday 2026-11-01\nm1 01:50:00\n| before\nday 2026-11-01 tz=-05:00\nm2 01:10:00\n| after\n";

        assert_eq!(
            read_times(output),
            Some((
                rfc_3339("2026-11-01T01:50:00-04:00"),
                rfc_3339("2026-11-01T01:10:00-05:00")
            ))
        );
    }

    #[test]
    fn full_time_without_offset_is_in_headers_offset() {
        let header = "protocol agent-outline cut=none chars=none policy=text messages=2 at=2026-11-01T01:50:00 to=2026-11-01T01:10:00-05:00 tz=-04:00";

        assert_eq!(
            times(header, header),
            Some((
                rfc_3339("2026-11-01T01:50:00-04:00"),
                rfc_3339("2026-11-01T01:10:00-05:00")
            ))
        );
    }

    #[test]
    #[should_panic(expected = "an excerpt on a read's record")]
    fn read_record_with_excerpt_doesnt_parse() {
        read_records(&format!("{HEADER}m1 user | Run the tests\n"));
    }

    #[test]
    #[should_panic(expected = "a word the record may not carry")]
    fn record_with_unknown_word_doesnt_parse() {
        read_records(&format!("{HEADER}m2 not-a-record\n"));
    }

    #[test]
    #[should_panic(expected = "a token that is not an atom")]
    fn record_with_unknown_atom_doesnt_parse() {
        read_records(&format!("{HEADER}m2 11:41:30 role=user\n"));
    }

    #[test]
    #[should_panic(expected = "a word the record may not carry")]
    fn call_record_with_source_word_doesnt_parse() {
        read_records(&format!("{HEADER}m1.1 user\n"));
    }

    #[test]
    #[should_panic(expected = "a record without an address")]
    fn record_with_malformed_address_doesnt_parse() {
        read_records(&format!("{HEADER}m1.x 11:41:30\n"));
    }

    #[test]
    fn read_of_every_record_kind_parses() {
        let output = concat!(
            "protocol agent-read cut=body+tail chars=900 policy=text+tools+subagents omit=m9 tz=-04:00 hidden-tool-results=2 warnings=1 warning-records=1\n",
            "conversation project=pr_0 uuid=u ref=ch_0a1\n",
            "subagent s1 type=Explore launch=m2 span=m3..m5 report=m6 : scout\n",
            "day 2026-11-01\n",
            "m2.1 01:50:00 launches=s1 result=m4,m5 same=m1.1\n",
            "| Agent (Explore): scout\n",
            "m3 s1 01:50:01 call=m2.1..2 tool=Agent hidden-tools=Read%20File anchor=ma_1\n",
            "day 2026-11-01 tz=-05:00\n",
            "m6 report 01:10:00 from=s1 slice=match=cargo%20test context=1 hits=2\n",
            "m7..m8 hidden 01:10:01..01:10:05 tools=1 calls=m2.3\n",
            "read refs=ch_0a1:m7;ch_0a1:m8 tools=true tool-results=true thinking=false subagents=false\n",
            "continue read refs=ch_0a1:m9 tools=true tool-results=false thinking=false subagents=true\n",
            "protocol agent-warning kind=skipped ref=ch_c detail=empty%20transcript\n",
        );

        let records = read_records(output);

        assert_eq!(records.len(), 4);
    }

    #[test]
    #[should_panic(expected = "a read record that does not parse")]
    fn read_record_without_its_ref_and_flags_doesnt_parse() {
        read_records(&format!("{HEADER}read garbage\n"));
    }

    #[test]
    #[should_panic(expected = "a continue record that does not parse")]
    fn continue_record_without_its_refs_and_flags_doesnt_parse() {
        read_records(&format!("{HEADER}continue nonsense\n"));
    }

    #[test]
    #[should_panic(expected = "an atom value that does not parse: same=garbage")]
    fn same_atom_without_address_doesnt_parse() {
        read_records(&format!("{HEADER}m1 same=garbage\n"));
    }

    #[test]
    #[should_panic(
        expected = "a token that is not an atom of a Message record in its place: result=m2"
    )]
    fn calls_result_atom_on_message_record_doesnt_parse() {
        read_records(&format!("{HEADER}m1 result=m2\n"));
    }

    #[test]
    fn calls_result_atom_on_call_record_parses() {
        let output = format!("{HEADER}m1.1 result=m2\n");

        let records = read_records(&output);

        assert_eq!(records[0].atom("result"), Some("m2"));
    }

    #[test]
    #[should_panic(
        expected = "a token that is not an atom of a Message record in its place: call=m1.1"
    )]
    fn message_record_with_its_atoms_out_of_order_doesnt_parse() {
        read_records(&format!("{HEADER}m2 anchor=ma_1 call=m1.1\n"));
    }

    /// `--lines` rejects `9..2`: the read this record names would fail.
    #[test]
    #[should_panic(expected = "a read record that does not parse")]
    fn read_record_with_lines_ending_before_they_start_doesnt_parse() {
        read_records(&format!(
            "{HEADER}read ref=ch_0:m1 lines=9..2 tools=true tool-results=false thinking=false subagents=false\n"
        ));
    }

    #[test]
    #[should_panic(expected = "a record without an address: m5..m3 hidden")]
    fn run_of_messages_ending_before_it_starts_doesnt_parse() {
        read_records(&format!("{HEADER}m5..m3 hidden tools=1\n"));
    }

    #[test]
    #[should_panic(expected = "an atom value that does not parse: calls=m2.4..2")]
    fn run_of_calls_ending_before_it_starts_doesnt_parse() {
        read_records(&format!("{HEADER}m3..m4 hidden tools=1 calls=m2.4..2\n"));
    }

    #[test]
    #[should_panic(expected = "a time span ending before it starts")]
    fn time_span_ending_before_it_starts_doesnt_parse() {
        read_records(&format!("{HEADER}m1 11:41:30..11:41:25\n"));
    }

    #[test]
    #[should_panic(expected = "a record without an address: m0 11:41:25")]
    fn message_numbered_zero_doesnt_parse() {
        read_records(&format!("{HEADER}m0 11:41:25\n"));
    }

    #[test]
    #[should_panic(expected = "a record without an address: m1.0 11:41:25")]
    fn call_numbered_zero_doesnt_parse() {
        read_records(&format!("{HEADER}m1.0 11:41:25\n"));
    }

    /// The ref parser rejects `m0`: the read this record names would fail.
    #[test]
    #[should_panic(expected = "a read record that does not parse")]
    fn read_record_of_message_zero_doesnt_parse() {
        read_records(&format!(
            "{HEADER}read ref=ch_0:m0 tools=false tool-results=false thinking=false subagents=false\n"
        ));
    }

    #[test]
    #[should_panic(expected = "a to= time before its at= time")]
    fn outline_header_ending_before_it_starts_doesnt_parse() {
        outline_records(
            "protocol agent-outline cut=none chars=none policy=text messages=2 at=2026-10-14T12:00:02 to=2026-10-14T12:00:01 tz=-04:00\n",
        );
    }

    #[test]
    #[should_panic(expected = "a Call record the Outline grammar does not print")]
    fn outline_with_call_record_doesnt_parse() {
        outline_records(&format!(
            "{OUTLINE_HEADER}m1.1 11:41:25 chars=9 | Bash: ls\n"
        ));
    }

    #[test]
    #[should_panic(expected = "a protocol record that does not parse")]
    fn header_with_token_after_its_atoms_doesnt_parse() {
        read_records(
            "protocol agent-read cut=none chars=none policy=text omit=none tz=-04:00 bogus\n",
        );
    }

    #[test]
    #[should_panic(expected = "a conversation record that does not parse")]
    fn conversation_record_without_its_ref_doesnt_parse() {
        read_records("conversation project=pr_0 uuid=u\n");
    }

    #[test]
    #[should_panic(expected = "a subagent record that does not parse")]
    fn legend_record_without_its_span_doesnt_parse() {
        read_records(&format!("{HEADER}subagent s1 type=Explore : scout\n"));
    }

    #[test]
    #[should_panic(expected = "a day record that does not parse")]
    fn day_record_with_token_after_its_date_doesnt_parse() {
        read_records(&format!("{HEADER}day 2026-10-15 bogus\n"));
    }

    #[test]
    #[should_panic(expected = "a protocol record that does not parse")]
    fn outline_with_reads_header_doesnt_parse() {
        outline_records(HEADER);
    }

    #[test]
    fn outline_segment_parses_into_its_run_and_its_handles() {
        let output = format!(
            "{OUTLINE_HEADER}seg m1..m20 s3,s4 chars=900 at=2026-10-14T11:41:25 to=2026-11-02T10:00:00-05:00 | first / last\nseg m21..m30 chars=10 | a / b\n"
        );

        let segments = segment_records(&output);

        let parsed = segments
            .iter()
            .map(|segment| (segment.address, segment.subagents.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            parsed,
            [("m1..m20", vec!["s3", "s4"]), ("m21..m30", vec![])]
        );
    }

    #[test]
    #[should_panic(expected = "a segment without chars=")]
    fn segment_with_token_in_place_of_its_chars_doesnt_parse() {
        segment_records(&format!("{OUTLINE_HEADER}seg m1..m20 s3,s4 bogus\n"));
    }

    #[test]
    #[should_panic(expected = "a token after a segment's fields")]
    fn segment_with_token_after_its_fields_doesnt_parse() {
        segment_records(&format!(
            "{OUTLINE_HEADER}seg m1..m20 s3,s4 chars=900 bogus | first / last\n"
        ));
    }

    #[test]
    #[should_panic(expected = "a segment time that does not parse")]
    fn segment_with_malformed_time_doesnt_parse() {
        segment_records(&format!(
            "{OUTLINE_HEADER}seg m1..m20 chars=900 at=yesterday | first / last\n"
        ));
    }

    #[test]
    #[should_panic(expected = "a to= time before its at= time")]
    fn segment_ending_before_it_starts_doesnt_parse() {
        segment_records(&format!(
            "{OUTLINE_HEADER}seg m1..m20 chars=900 at=2026-10-14T12:00:02 to=2026-10-14T12:00:01 | first / last\n"
        ));
    }

    #[test]
    #[should_panic(expected = "a segment without its first and last snippet")]
    fn segment_without_its_excerpt_doesnt_parse() {
        segment_records(&format!("{OUTLINE_HEADER}seg m1..m20 chars=900\n"));
    }

    #[test]
    #[should_panic(expected = "a token after a segment's fields")]
    fn outline_with_end_time_on_segment_without_start_doesnt_parse() {
        outline_records(&format!(
            "{OUTLINE_HEADER}seg m1..m20 chars=900 to=2026-10-14T11:41:25 | first / last\n"
        ));
    }

    #[test]
    fn outline_record_keeps_its_excerpt_as_its_content() {
        let output = format!("{OUTLINE_HEADER}m1 user 11:41:25 chars=13 | Run the tests\n");

        let records = outline_records(&output);

        assert_eq!(records[0].content, ["Run the tests"]);
        assert_eq!(records[0].atom("chars"), Some("13"));
    }
}
