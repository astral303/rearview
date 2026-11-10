//! The JSONL transcript format Pi and OMP share.
//!
//! Pi and OMP append to a log; OMP also rewrites the title slot at its head
//! in place. A read of a changed log resumes from its stable cursor while
//! the new entries continue the branch the log shows.

mod tools;

use super::dropped::{DroppedRecords, SkippedKinds};
use super::lines::{self, LineCursor, visit_line_spans_from};
use super::{
    EarlierTurns, RecordLine, SessionFormat, SessionHeader, SessionProjection, append_exit_code,
    append_output_note, at_own_lines, rfc3339_from_millis, trim_blank_lines,
};
use crate::agent::transcript::tool_result_text;
use crate::error::{AppError, Result};
use crate::history::provider::{ResumedWindow, Resumption, TranscriptRead, TranscriptReader};
use crate::history::{MalformedLine, ParsePermit, Source};
use crate::log_entry::{
    AssistantMessage, ContentBlock, LogEntry, TokenUsage, Tool, UserContent, UserMessage,
};
use crate::parallel::ParallelThreshold;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAX_SUPPORTED_VERSION: u64 = 3;

/// The session id `path` states in its header, or `None` when it is not a
/// Pi-family log.
///
/// The header is the first record, or the second when an OMP title slot comes
/// before it, so two lines settle it and the log itself is never parsed.
pub(crate) fn header_session_id(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut records = BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
        .filter(|line| !line.trim().is_empty())
        .take(2)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok());
    let first = records.next()?;
    let header = match first.get("type").and_then(Value::as_str) {
        Some("session") => first,
        Some("title") => records.next()?,
        _ => return None,
    };
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    header.get("id").and_then(Value::as_str).map(str::to_owned)
}

/// Every Pi-family log `depth` levels under `root` whose header states
/// `session_id`. A UUID matches in any case; an id the user chose matches as
/// typed.
///
/// An id is not unique across logs — a branch copied within a project keeps
/// the id it came from — so this returns all of them rather than the first.
pub(crate) fn sessions_with_id(
    root: &Path,
    depth: usize,
    session_id: &str,
) -> Result<Vec<PathBuf>> {
    use crate::history::provider::walk;
    Ok(
        walk::jsonl_files_at_depth(&walk::SystemLister, root, depth)?
            .into_iter()
            .filter(|path| {
                header_session_id(path)
                    .is_some_and(|stored| crate::search::session_id_matches(&stored, session_id))
            })
            .collect(),
    )
}

/// The session id the Pi-family log at `path` states in a header the reader
/// accepts, and the agent the list attributes the log to, as
/// [`AcceptedHeader::source`] decides it. `None` when the log has no such
/// header. Reads only up to the header, skipping blank and malformed lines as
/// the reader does.
pub(crate) fn read_header(path: &Path, default_source: Source) -> Result<Option<(String, Source)>> {
    let file = File::open(path)?;
    let mut records = Vec::with_capacity(2);
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            records.push(value);
            if records.len() == 2 {
                break;
            }
        }
    }
    let Some(first) = records.first() else {
        return Ok(None);
    };
    Ok(AcceptedHeader::of(first, records.get(1))
        .map(|header| (header.id.to_owned(), header.source(default_source))))
}

/// A Pi-family log's header as the reader accepts it: a `session` record
/// stating an id, a timestamp and a directory, in a supported format
/// version, either first or after an OMP title slot.
struct AcceptedHeader<'a> {
    title_slot: Option<&'a Map<String, Value>>,
    object: &'a Map<String, Value>,
    id: &'a str,
    timestamp: &'a str,
    cwd: &'a str,
    version: u64,
}

impl<'a> AcceptedHeader<'a> {
    /// The header among a log's first two records, `first` and `second`.
    fn of(first: &'a Value, second: Option<&'a Value>) -> Option<Self> {
        let title_slot = first
            .as_object()
            .filter(|object| object.get("type").and_then(Value::as_str) == Some("title"));
        let object = if title_slot.is_some() { second? } else { first }.as_object()?;
        if object.get("type").and_then(Value::as_str) != Some("session") {
            return None;
        }
        let text = |field| object.get(field).and_then(Value::as_str);
        let version = object.get("version").and_then(Value::as_u64).unwrap_or(1);
        if !(1..=MAX_SUPPORTED_VERSION).contains(&version) {
            return None;
        }
        Some(Self {
            title_slot,
            object,
            id: text("id")?,
            timestamp: text("timestamp")?,
            cwd: text("cwd")?,
            version,
        })
    }

    /// OMP for a log with a title slot, whichever directory holds it;
    /// `default_source` otherwise, since such a log does not name its agent.
    fn source(&self, default_source: Source) -> Source {
        if self.title_slot.is_some() {
            Source::Omp
        } else {
            default_source
        }
    }

    /// The title the slot states, else the one the header states; `None`
    /// when neither states one that is not empty.
    fn title(&self) -> Option<String> {
        self.title_slot
            .and_then(|slot| slot.get("title"))
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty())
            .or_else(|| self.object.get("title").and_then(Value::as_str))
            .filter(|title| !title.is_empty())
            .map(str::to_owned)
    }

    /// How many records the header and its title slot take up.
    fn record_count(&self) -> usize {
        1 + usize::from(self.title_slot.is_some())
    }
}

/// A log's header as a read takes it, from the first records.
struct LogHeader {
    id: String,
    timestamp: String,
    cwd: String,
    version: u64,
    source: Source,
    title: Option<String>,
    /// How many records the header and its title slot take up.
    record_count: usize,
    /// The end of the title slot, which the agent rewrites in place; 0
    /// without one.
    slot_end: u64,
    /// The log's identity and where its records start past the header;
    /// `None` while the header has no line ending.
    terminated: Option<(Vec<u8>, LineCursor)>,
}

impl LogHeader {
    /// The header among a log's first two records, on `first` and
    /// `second`; `None` when the reader does not accept it.
    fn of(first: &LogLine, second: Option<&LogLine>, default_source: Source) -> Option<Self> {
        let accepted = AcceptedHeader::of(
            first.record.as_ref()?,
            second.and_then(|line| line.record.as_ref()),
        )?;
        let has_slot = accepted.title_slot.is_some();
        let header_line = if has_slot { second? } else { first };
        let slot_end = if has_slot { first.end?.offset } else { 0 };
        let identity = || {
            let facts = serde_json::to_vec(&(header_line.text.as_str(), has_slot))
                .expect("a log's identity facts serialize");
            blake3::hash(&facts).as_bytes().to_vec()
        };
        Some(Self {
            id: accepted.id.to_owned(),
            timestamp: accepted.timestamp.to_owned(),
            cwd: accepted.cwd.to_owned(),
            version: accepted.version,
            source: accepted.source(default_source),
            title: accepted.title(),
            record_count: accepted.record_count(),
            slot_end,
            terminated: header_line.end.map(|end| (identity(), end)),
        })
    }

    /// The header of the log at `path`, read from its first records alone.
    fn read(path: &Path, default_source: Source) -> Result<Option<Self>> {
        let head = head_lines(BufReader::new(File::open(path)?))?;
        Ok(head
            .first()
            .and_then(|first| Self::of(first, head.get(1), default_source)))
    }

    /// The title as the session's leading entry.
    fn title_entry(&self) -> Option<(usize, LogEntry)> {
        self.title.as_ref().map(|title| {
            (
                0,
                LogEntry::CustomTitle {
                    custom_title: title.clone(),
                },
            )
        })
    }
}

/// The records a log's header can take up: an OMP title slot, then the
/// header.
const HEAD_RECORDS: usize = 2;

/// One line of a Pi-family log as a read takes it.
struct LogLine {
    number: usize,
    /// Past the line's line ending; `None` for a last line without one.
    end: Option<LineCursor>,
    /// The line as written: the header's identity hashes it.
    text: String,
    record: Option<Value>,
}

/// Hands `visit` each line of `reader`, standing at `from`, to its end, in
/// file order; returns the cursor past the last line with a line ending.
fn visit_log_lines(
    reader: impl BufRead,
    from: LineCursor,
    mut visit: impl FnMut(LogLine),
) -> std::io::Result<LineCursor> {
    let parse = |line: &str| serde_json::from_str::<Value>(line).ok();
    let read = visit_line_spans_from(
        reader,
        from,
        ParallelThreshold::DEFAULT,
        parse,
        |span, text, record| {
            visit(LogLine {
                number: span.start.line,
                end: Some(span.end),
                text,
                record,
            })
        },
    )?;
    if let Some((number, text)) = read.unterminated {
        let record = parse(&text);
        visit(LogLine {
            number,
            end: None,
            text,
            record,
        });
    }
    Ok(read.cursor)
}

/// A whole read of a Pi-family log, line by line: its first records until
/// they settle the header, then the entries after it.
enum WholeLog {
    /// Before the header settles: the records so far, at most
    /// [`HEAD_RECORDS`], the lines that hold none, and the count of the
    /// records the reader drops.
    Head {
        records: Vec<LogLine>,
        malformed_lines: Vec<usize>,
        dropped: DroppedRecords,
    },
    /// The header the reader accepts, and the entries after it.
    Entries {
        header: LogHeader,
        raw: Box<RawEntries>,
    },
    /// The first records hold no header the reader accepts.
    Rejected,
}

impl WholeLog {
    /// A read whose drops `dropped` counts.
    fn new(dropped: DroppedRecords) -> Self {
        Self::Head {
            records: Vec::with_capacity(HEAD_RECORDS),
            malformed_lines: Vec::new(),
            dropped,
        }
    }

    /// Take the log's next line, attributing a log without a title slot to
    /// `default_source`.
    fn read(&mut self, line: LogLine, default_source: Source) {
        match self {
            Self::Head {
                records,
                malformed_lines,
                dropped,
            } => {
                if line.record.is_none() {
                    malformed_lines.push(line.number);
                    return;
                }
                records.push(line);
                if records.len() == HEAD_RECORDS {
                    *self = Self::settled(
                        std::mem::take(records),
                        std::mem::take(malformed_lines),
                        std::mem::take(dropped),
                        default_source,
                    );
                }
            }
            Self::Entries { raw, .. } => raw.push_line(line),
            Self::Rejected => {}
        }
    }

    /// The log once `records`, its first records, settle its header: the
    /// records past the header are its first entries, after
    /// `malformed_lines`, the lines among them that hold no record.
    /// `dropped` counts the drops under the header's format version.
    fn settled(
        records: Vec<LogLine>,
        malformed_lines: Vec<usize>,
        mut dropped: DroppedRecords,
        default_source: Source,
    ) -> Self {
        let Some(header) = records
            .first()
            .and_then(|first| LogHeader::of(first, records.get(1), default_source))
        else {
            return Self::Rejected;
        };
        let version = header.version;
        // The header records the session file's format, not the release that
        // wrote it.
        dropped.record_version(|| Some(format!("session format {version}")));
        let mut raw = RawEntries::after(version, header.source, None, dropped);
        raw.malformed_lines = malformed_lines;
        for line in records.into_iter().skip(header.record_count) {
            raw.push_line(line);
        }
        Self::Entries {
            header,
            raw: Box::new(raw),
        }
    }

    /// The header and the entries after it; `None` for a log without a
    /// header the reader accepts.
    fn finish(self, default_source: Source) -> Option<(LogHeader, RawEntries)> {
        let settled = match self {
            Self::Head {
                records,
                malformed_lines,
                dropped,
            } => Self::settled(records, malformed_lines, dropped, default_source),
            settled => settled,
        };
        match settled {
            Self::Entries { header, raw } => Some((header, *raw)),
            Self::Head { .. } | Self::Rejected => None,
        }
    }
}

/// The lines of `reader` up to its first [`HEAD_RECORDS`] records, which
/// hold its header; lines that hold no record are left out.
fn head_lines(mut reader: impl BufRead) -> std::io::Result<Vec<LogLine>> {
    let mut head = Vec::new();
    let mut cursor = LineCursor::START;
    while head.len() < HEAD_RECORDS {
        let mut text = String::new();
        let length = reader.read_line(&mut text)?;
        if length == 0 {
            break;
        }
        let number = cursor.line;
        cursor = LineCursor {
            offset: cursor.offset + length as u64,
            line: number + 1,
        };
        let end = text.ends_with('\n').then_some(cursor);
        let text = text.trim_end_matches(['\n', '\r']).to_owned();
        if let Ok(record) = serde_json::from_str::<Value>(&text) {
            head.push(LogLine {
                number,
                end,
                text,
                record: Some(record),
            });
        }
    }
    Ok(head)
}

#[derive(Debug)]
struct RawEntry {
    line: usize,
    /// Past the record's line ending; `None` for a last line without one.
    end: Option<LineCursor>,
    id: String,
    parent_id: Option<String>,
    /// The record projected as the log shows it; `None` for a record the
    /// reader drops.
    entry: Option<LogEntry>,
    /// The record's drops, while the dropped-records report counts and
    /// the projection dropped something; added to the report's count when
    /// the log shows the record.
    dropped: Option<Box<DroppedRecords>>,
}

/// A log's entries after its header, each with its id, the id of its
/// parent, and its record projected as it was read. A record's parsed JSON
/// is dropped once its line is projected, whichever branch the log shows.
struct RawEntries {
    version: u64,
    /// The agent the entries are projected for.
    source: Source,
    /// The id of the entry before the next one: a format 1 entry's parent.
    previous_id: Option<String>,
    entries: Vec<RawEntry>,
    /// The lines that hold no record, or a record the format rejects.
    malformed_lines: Vec<usize>,
    /// The drops of the records the log shows, once [`Self::take_shown`]
    /// adds them.
    dropped: DroppedRecords,
}

impl RawEntries {
    /// The entries of a log in format `version`, read as `source`'s, that
    /// follow the entry with id `previous_id`; `dropped` counts their drops
    /// while the report counts.
    fn after(
        version: u64,
        source: Source,
        previous_id: Option<String>,
        dropped: DroppedRecords,
    ) -> Self {
        Self {
            version,
            source,
            previous_id,
            entries: Vec::new(),
            malformed_lines: Vec::new(),
            dropped,
        }
    }

    /// Add `line`'s record; a line that holds none is malformed.
    fn push_line(&mut self, line: LogLine) {
        match line.record {
            Some(record) => self.push(line.number, line.end, record),
            None => self.malformed_lines.push(line.number),
        }
    }

    /// Add the record on line `number`, ending at `end`.
    fn push(&mut self, number: usize, end: Option<LineCursor>, record: Value) {
        let Some(object) = record.as_object() else {
            self.malformed_lines.push(number);
            return;
        };
        let id = if self.version == 1 {
            format!("v1-{number}")
        } else if let Some(id) = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            id.to_owned()
        } else {
            self.malformed_lines.push(number);
            return;
        };
        let parent_id = if self.version == 1 {
            self.previous_id.clone()
        } else {
            object
                .get("parentId")
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        self.previous_id = Some(id.clone());
        let mut dropped = self.dropped.for_one_record();
        let entry = normalize_entry(&record, &id, self.source, &mut dropped);
        self.entries.push(RawEntry {
            line: number,
            end,
            id,
            parent_id,
            entry,
            dropped: (!dropped.is_empty()).then(|| Box::new(dropped)),
        });
    }

    /// Moves out the entries `branch` shows, in file order, each at its
    /// line, and counts the drops of the records it shows.
    fn take_shown(&mut self, branch: &ShownBranch) -> Vec<(usize, LogEntry)> {
        let mut shown = Vec::with_capacity(branch.indices.len());
        for &index in branch.indices.iter().rev() {
            let raw = &mut self.entries[index];
            if let Some(dropped) = raw.dropped.take() {
                self.dropped.add_counts_of(*dropped);
            }
            if let Some(entry) = raw.entry.take() {
                shown.push((raw.line, entry));
            }
        }
        shown
    }
}

/// The branch a log shows: from its last entry back through each parent.
struct ShownBranch {
    /// Indices into the entries, from the last entry back.
    indices: Vec<usize>,
    end: BranchEnd,
}

/// Where the walk back along a branch stopped.
#[derive(Debug, Eq, PartialEq)]
enum BranchEnd {
    /// An entry without a parent, or no entry at all.
    Root,
    /// A parent the entries do not hold.
    Outside(String),
    /// An entry the walk met before.
    Cycle,
}

impl ShownBranch {
    /// The branch `entries` show. Of two entries with one id, the later is
    /// the parent.
    fn of(entries: &[RawEntry]) -> Self {
        let by_id = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id.as_str(), index))
            .collect::<HashMap<_, _>>();
        let mut indices = Vec::new();
        let mut visited = HashSet::new();
        let mut current = entries.len().checked_sub(1);
        let end = loop {
            let Some(index) = current else {
                break BranchEnd::Root;
            };
            let entry = &entries[index];
            if !visited.insert(entry.id.as_str()) {
                break BranchEnd::Cycle;
            }
            indices.push(index);
            let Some(parent) = entry.parent_id.as_deref() else {
                break BranchEnd::Root;
            };
            match by_id.get(parent) {
                Some(&parent_index) => current = Some(parent_index),
                None => break BranchEnd::Outside(parent.to_owned()),
            }
        };
        Self { indices, end }
    }

    /// True when each entry's parent comes before it in the file: the
    /// branch's entries before any line are then the branch of the last
    /// entry before it.
    fn is_in_file_order(&self) -> bool {
        self.indices.windows(2).all(|pair| pair[0] > pair[1])
    }

    /// The index of the branch's last entry with a line ending.
    fn last_terminated(&self, entries: &[RawEntry]) -> Option<usize> {
        self.indices
            .iter()
            .copied()
            .find(|&index| entries[index].end.is_some())
    }
}

/// What a read of a Pi-family log carries to the next, at the stable
/// cursor.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct LogState {
    version: u64,
    /// True for a log read as OMP's.
    is_omp: bool,
    /// [`LogHeader::slot_end`].
    slot_end: u64,
    /// The id of the shown branch's last entry before the stable cursor.
    leaf: Option<String>,
}

impl LogState {
    fn source(&self) -> Source {
        if self.is_omp { Source::Omp } else { Source::Pi }
    }
}

/// Where a read of a Pi-family log stopped, and where the next restarts
/// with which state.
struct LogReadEnd {
    cursor: LineCursor,
    stable_cursor: LineCursor,
    state: LogState,
}

impl LogReadEnd {
    /// The read's end over `entries`, the log's entries from `from` on, read
    /// to `cursor`: the stable cursor stands past the shown branch's last
    /// entry with a line ending, else stays at `from`. `None` when the
    /// branch does not continue `state`'s leaf or is not in file order.
    fn of(
        entries: &[RawEntry],
        branch: &ShownBranch,
        from: LineCursor,
        cursor: LineCursor,
        state: &LogState,
    ) -> Option<Self> {
        let continues = match &branch.end {
            _ if entries.is_empty() => true,
            BranchEnd::Root => state.leaf.is_none(),
            BranchEnd::Outside(parent) => state.leaf.as_ref().is_none_or(|leaf| leaf == parent),
            BranchEnd::Cycle => false,
        };
        if !continues || !branch.is_in_file_order() {
            return None;
        }
        let (stable_cursor, leaf) = match branch.last_terminated(entries) {
            Some(index) => (entries[index].end?, Some(entries[index].id.clone())),
            None => (from, state.leaf.clone()),
        };
        Some(Self {
            cursor,
            stable_cursor,
            state: LogState {
                leaf,
                ..state.clone()
            },
        })
    }

    /// What this read leaves for the next, for the log `file` holds, whose
    /// identity is `identity`.
    fn transcript_read(
        &self,
        identity: Vec<u8>,
        file: &mut (impl std::io::Read + Seek),
    ) -> Result<TranscriptRead> {
        Ok(TranscriptRead {
            cursor: self.cursor.encode(),
            stable_cursor: self.stable_cursor.encode(),
            identity,
            rewrite_check: lines::rewrite_check_after(file, self.state.slot_end, self.cursor)?,
            parser_state: serde_json::to_vec(&self.state).expect("a log state serializes"),
            stable_line: Some(self.stable_cursor.line),
        })
    }
}

/// One reader of the Pi-family log, bound to the source it speaks for.
///
/// An OMP session announces itself with a leading `title` slot. Without one the
/// file is indistinguishable from a Pi session, so it is attributed to
/// `default_source` — in practice, whichever agent's root it was found under. The
/// attribution is not cosmetic: it decides whether assistant turns are labelled
/// `Pi` or `OMP`, so it has to be settled before the entries are normalized.
pub struct PiLogFormat {
    default_source: Source,
}

pub static PI_LOG: PiLogFormat = PiLogFormat {
    default_source: Source::Pi,
};

pub static OMP_LOG: PiLogFormat = PiLogFormat {
    default_source: Source::Omp,
};

impl SessionFormat for PiLogFormat {
    /// A Pi-family thread splices in under its file stem: OMP names a
    /// sub-agent transcript `<outputId>.jsonl`, after the name the parent's
    /// tool call gave the sub-agent.
    fn read_transcript(
        &self,
        _admitted: crate::history::Admitted,
        path: &Path,
        dropped: DroppedRecords,
    ) -> Result<Option<SessionProjection>> {
        let file = File::open(path)?;
        Ok(
            parse_reader(BufReader::new(file), self.default_source, dropped)?.map(
                |mut projection| {
                    projection.header.thread_label = path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .map(str::to_owned);
                    projection
                },
            ),
        )
    }
}

/// The log `reader` holds, read whole. Its title, which OMP rewrites in
/// place and every read takes again, is its leading entry.
fn parse_reader(
    mut reader: impl BufRead + Seek,
    default_source: Source,
    dropped: DroppedRecords,
) -> Result<Option<SessionProjection>> {
    let mut log = WholeLog::new(dropped);
    let cursor = visit_log_lines(&mut reader, LineCursor::START, |line| {
        log.read(line, default_source)
    })?;
    let Some((header, mut raw)) = log.finish(default_source) else {
        return Ok(None);
    };
    let version = header.version;

    let branch = ShownBranch::of(&raw.entries);
    let mut entries: Vec<(usize, LogEntry)> = header.title_entry().into_iter().collect();
    entries.extend(raw.take_shown(&branch));
    let transcript_read = match &header.terminated {
        Some((identity, records_start)) => {
            let state = LogState {
                version,
                is_omp: header.source == Source::Omp,
                slot_end: header.slot_end,
                leaf: None,
            };
            match LogReadEnd::of(&raw.entries, &branch, *records_start, cursor, &state) {
                Some(end) => end.transcript_read(identity.clone(), &mut reader)?,
                None => TranscriptRead::full_read(),
            }
        }
        None => TranscriptRead::full_read(),
    };

    Ok(Some(SessionProjection {
        source: header.source,
        header: SessionHeader {
            version,
            id: header.id,
            timestamp: header.timestamp,
            cwd: PathBuf::from(header.cwd),
            thread_label: None,
            subagent_identity: Default::default(),
        },
        title: header.title,
        entries: at_own_lines(entries),
        leaf_id: raw.entries.last().map(|entry| entry.id.clone()),
        malformed_lines: raw.malformed_lines,
        dropped: raw.dropped,
        earlier_turns: EarlierTurns::Read,
        earlier_transcripts: Vec::new(),
        transcript_read,
    }))
}

/// Resumes a read of a Pi-family log from its stable cursor: past the last
/// entry with a line ending on the branch the log shows. A read whose new
/// entries do not continue that branch changes which earlier entries show,
/// and reads the log whole. The identity is the header record, and whether
/// a title slot leads; every read takes the title again, and the rewrite
/// check leaves out the title slot, which OMP rewrites in place.
///
/// Two entries with one id are not detected: Pi and OMP give each a fresh
/// one.
impl TranscriptReader for PiLogFormat {
    fn identity(&self, source: &Path) -> Result<Vec<u8>> {
        Ok(LogHeader::read(source, self.default_source)?
            .and_then(|header| header.terminated)
            .map(|(identity, _)| identity)
            .unwrap_or_default())
    }

    fn is_rewritten(&self, source: &Path, read: &TranscriptRead) -> Result<bool> {
        let (Some(cursor), Ok(state)) = (
            LineCursor::decode(&read.cursor),
            serde_json::from_slice::<LogState>(&read.parser_state),
        ) else {
            return Ok(true);
        };
        let mut file = File::open(source)?;
        if file.metadata()?.len() < cursor.offset {
            return Ok(true);
        }
        let slot_end = LogHeader::read(source, self.default_source)?.map(|header| header.slot_end);
        if slot_end != Some(state.slot_end) {
            return Ok(true);
        }
        Ok(lines::rewrite_check_after(&mut file, state.slot_end, cursor)? != read.rewrite_check)
    }

    fn read_from(
        &self,
        permit: ParsePermit<'_>,
        source: &Path,
        read: &TranscriptRead,
    ) -> Result<Resumption> {
        permit.admit(source);
        let (Some(stable_cursor), Ok(state)) = (
            LineCursor::decode(&read.stable_cursor),
            serde_json::from_slice::<LogState>(&read.parser_state),
        ) else {
            return Ok(Resumption::FullRead);
        };
        let mut reader = BufReader::new(File::open(source)?);
        reader.seek(SeekFrom::Start(stable_cursor.offset))?;
        let mut raw = RawEntries::after(
            state.version,
            state.source(),
            state.leaf.clone(),
            DroppedRecords::default(),
        );
        let cursor = visit_log_lines(&mut reader, stable_cursor, |line| raw.push_line(line))?;
        let branch = ShownBranch::of(&raw.entries);
        let Some(end) = LogReadEnd::of(&raw.entries, &branch, stable_cursor, cursor, &state) else {
            return Ok(Resumption::ChangesEarlierEntry);
        };
        let entries = raw.take_shown(&branch);
        let bytes_read = reader
            .get_ref()
            .metadata()?
            .len()
            .saturating_sub(stable_cursor.offset);
        Ok(Resumption::Window(ResumedWindow {
            start_line: stable_cursor.line,
            entries: at_own_lines(entries),
            malformed_lines: raw
                .malformed_lines
                .into_iter()
                .map(|line_number| MalformedLine {
                    line_number,
                    detail: None,
                })
                .collect(),
            read: end.transcript_read(read.identity.clone(), reader.get_mut())?,
            bytes_read,
        }))
    }

    fn read_whole(
        &self,
        permit: ParsePermit<'_>,
        source: &Path,
    ) -> Result<Option<SessionProjection>> {
        self.parse_transcript(permit, source)
    }

    fn leading_entries(
        &self,
        source: &Path,
        _read: &TranscriptRead,
    ) -> Result<Vec<(RecordLine, LogEntry)>> {
        Ok(LogHeader::read(source, self.default_source)?
            .and_then(|header| header.title_entry())
            .map(|(line, entry)| (RecordLine::from(line), entry))
            .into_iter()
            .collect())
    }
}

fn normalize_entry(
    value: &Value,
    entry_id: &str,
    source: Source,
    dropped: &mut DroppedRecords,
) -> Option<LogEntry> {
    let object = value.as_object()?;
    let kind = object.get("type").and_then(Value::as_str);
    let entry_type = kind.unwrap_or("unknown");
    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match entry_type {
        "message" => {
            normalize_message(object.get("message")?, entry_id, timestamp, source, dropped)
        }
        "compaction" => metadata_with_usage(
            "Compaction",
            object.get("summary").and_then(Value::as_str).unwrap_or(""),
            timestamp,
            false,
            object.get("usage").and_then(pi_usage),
        ),
        "branch_summary" => metadata_with_usage(
            "Branch summary",
            object.get("summary").and_then(Value::as_str).unwrap_or(""),
            timestamp,
            false,
            object.get("usage").and_then(pi_usage),
        ),
        // Tokens spent outside the conversation, such as prompt-cache warming
        // during a long tool run. Pi adds entries of every `kind` to its
        // `/session` totals.
        "usage" => metadata_with_usage(
            "Usage",
            "",
            timestamp,
            false,
            object.get("usage").and_then(pi_usage),
        ),
        // OMP's replay starts after the latest `/clear`; rearview keeps the
        // earlier turns and marks the boundary between them.
        "reset_boundary" => metadata("/clear", "Context cleared", timestamp, true),
        "session_info" | "title_change" => Some(LogEntry::CustomTitle {
            custom_title: object
                .get(if entry_type == "title_change" {
                    "title"
                } else {
                    "name"
                })
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        }),
        "model_change" => {
            let model = object
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    format!(
                        "{}/{}",
                        object
                            .get("provider")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                        object
                            .get("modelId")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                    )
                });
            metadata("Model", &model, timestamp, false)
        }
        "custom_message" => {
            if object.get("display").and_then(Value::as_bool) == Some(false) {
                return None;
            }
            let text = content_text(object.get("content"));
            metadata(
                object
                    .get("customType")
                    .and_then(Value::as_str)
                    .unwrap_or("Custom"),
                &text,
                timestamp,
                true,
            )
        }
        _ => {
            SkippedKinds {
                field: "type",
                values: skipped_kinds(source).entry_types,
            }
            .count(kind, dropped);
            None
        }
    }
}

struct PiLogSkippedKinds {
    entry_types: &'static [&'static str],
    message_roles: &'static [&'static str],
}

static SKIPPED_FOR_PI: PiLogSkippedKinds = PiLogSkippedKinds {
    entry_types: &[
        // Settings, bookmarks and extension state, with no conversation text.
        "thinking_level_change",
        "label",
        "custom",
        // `context_edit` changes only what Pi sends the model; Pi shows the
        // original messages.
        "context_edit",
    ],
    // `system` messages hold the system prompt and tool changes; Pi does not
    // show them.
    message_roles: &["system"],
};

static SKIPPED_FOR_OMP: PiLogSkippedKinds = PiLogSkippedKinds {
    entry_types: &[
        // Settings, bookmarks and extension state, with no conversation text.
        "thinking_level_change",
        "label",
        "custom",
        // Session state OMP restores when it reopens a session, and does not show.
        "service_tier_change",
        "ttsr_injection",
        "mode_change",
        "credential_pin",
        // A sub-agent's starting context; its task also arrives as the first
        // user message.
        "session_init",
    ],
    // `developer` messages are instructions to the model; OMP does not show
    // them.
    message_roles: &["developer"],
};

fn skipped_kinds(source: Source) -> &'static PiLogSkippedKinds {
    match source {
        Source::Pi => &SKIPPED_FOR_PI,
        _ => &SKIPPED_FOR_OMP,
    }
}

/// `entry_id` is the record's own id, which only the `bashExecution` arm
/// needs: it pairs the call it builds with the result beside it.
fn normalize_message(
    message: &Value,
    entry_id: &str,
    timestamp: Option<String>,
    source: Source,
    dropped: &mut DroppedRecords,
) -> Option<LogEntry> {
    let object = message.as_object()?;
    let Some(role) = object.get("role").and_then(Value::as_str) else {
        dropped.unrecognised("message.role", None);
        return None;
    };
    let timestamp = timestamp.or_else(|| millis_timestamp(object.get("timestamp")));
    match role {
        "user" => Some(LogEntry::User {
            message: UserMessage {
                role: "user".to_owned(),
                content: user_content(object.get("content")),
            },
            timestamp,
            uuid: None,
            cwd: None,
            parent_tool_use_id: None,
            source_tool_use_id: None,
            usage: None,
        }),
        "assistant" => Some(LogEntry::Assistant {
            agent: Some(
                match source {
                    Source::Omp => "OMP",
                    _ => "Pi",
                }
                .to_owned(),
            ),
            message: AssistantMessage {
                role: "assistant".to_owned(),
                content: assistant_content(object.get("content"), source),
                model: object
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                usage: object.get("usage").and_then(pi_usage),
                id: None,
            },
            timestamp,
            uuid: None,
            parent_tool_use_id: None,
        }),
        "toolResult" => Some(LogEntry::User {
            message: UserMessage {
                role: "user".to_owned(),
                content: UserContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: object
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned(),
                    content: Some(tool_result_content(object)),
                    standalone_tool_name: None,
                }]),
            },
            timestamp,
            uuid: None,
            cwd: None,
            parent_tool_use_id: None,
            source_tool_use_id: None,
            usage: object.get("usage").and_then(pi_usage),
        }),
        // A command the user ran themselves: Pi records it as one row, which
        // reads as the call it was and the result it produced.
        "bashExecution" => Some(LogEntry::User {
            message: UserMessage {
                role: "user".to_owned(),
                content: UserContent::Blocks(vec![
                    ContentBlock::ToolUse {
                        id: entry_id.to_owned(),
                        name: "bash".to_owned(),
                        tool: Tool::UserShell,
                        input: json!({
                            "command": object
                                .get("command")
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        }),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: entry_id.to_owned(),
                        content: Some(json!(bash_output(object))),
                        standalone_tool_name: None,
                    },
                ]),
            },
            // The row records one command, which ran when the record was
            // written, so the run it opens is stamped like any other.
            timestamp,
            uuid: None,
            cwd: None,
            parent_tool_use_id: None,
            source_tool_use_id: None,
            usage: None,
        }),
        "custom" | "hookMessage" => {
            if object.get("display").and_then(Value::as_bool) == Some(false) {
                None
            } else {
                let label = object
                    .get("customType")
                    .and_then(Value::as_str)
                    .unwrap_or("Custom");
                let text = content_text(object.get("content"));
                metadata(label, &text, timestamp, true)
            }
        }
        _ => {
            SkippedKinds {
                field: "message.role",
                values: skipped_kinds(source).message_roles,
            }
            .count(Some(role), dropped);
            None
        }
    }
}

fn metadata(
    label: &str,
    text: &str,
    timestamp: Option<String>,
    searchable: bool,
) -> Option<LogEntry> {
    metadata_with_usage(label, text, timestamp, searchable, None)
}

fn metadata_with_usage(
    label: &str,
    text: &str,
    timestamp: Option<String>,
    searchable: bool,
    usage: Option<TokenUsage>,
) -> Option<LogEntry> {
    Some(LogEntry::PiMetadata {
        label: label.to_owned(),
        text: text.to_owned(),
        timestamp,
        searchable,
        usage,
    })
}

fn millis_timestamp(value: Option<&Value>) -> Option<String> {
    rfc3339_from_millis(value?.as_i64()?)
}

fn user_content(value: Option<&Value>) -> UserContent {
    if let Some(text) = value.and_then(Value::as_str) {
        return UserContent::String(text.to_owned());
    }
    UserContent::Blocks(content_blocks(value))
}

fn assistant_content(value: Option<&Value>, source: Source) -> Vec<ContentBlock> {
    let Some(values) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    values
        .iter()
        .filter_map(Value::as_object)
        .flat_map(|object| assistant_blocks(object, source))
        .collect()
}

fn assistant_blocks(object: &Map<String, Value>, source: Source) -> Vec<ContentBlock> {
    match block_type(object) {
        "thinking" => vec![ContentBlock::Thinking {
            thinking: string_or_empty(object, "thinking"),
            signature: string_or_empty(object, "thinkingSignature"),
        }],
        "toolCall" => tools::tool_use_blocks(
            object
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            object
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({})),
            source,
        ),
        _ => content_block(object).into_iter().collect(),
    }
}

/// The text and image blocks either role can carry.
fn content_blocks(value: Option<&Value>) -> Vec<ContentBlock> {
    let Some(values) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    values
        .iter()
        .filter_map(Value::as_object)
        .filter_map(content_block)
        .collect()
}

fn content_block(object: &Map<String, Value>) -> Option<ContentBlock> {
    match block_type(object) {
        "text" => Some(ContentBlock::Text {
            text: string_or_empty(object, "text"),
        }),
        "image" => Some(ContentBlock::Text {
            text: format!(
                "[Image: {}]",
                object
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown type")
            ),
        }),
        _ => None,
    }
}

fn block_type(object: &Map<String, Value>) -> &str {
    object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

fn string_or_empty(object: &Map<String, Value>, key: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn content_text(value: Option<&Value>) -> String {
    if let Some(text) = value.and_then(Value::as_str) {
        return text.to_owned();
    }
    content_blocks(value)
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_result_content(object: &Map<String, Value>) -> Value {
    let blocks = content_blocks(object.get("content"))
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut text = tool_result_text(&Value::Array(blocks)).unwrap_or_default();
    if object.get("isError").and_then(Value::as_bool) == Some(true) {
        text = format!("Error: {text}");
    }
    json!(text)
}

/// The command's output, plus the lines Pi records beside it: a non-zero exit
/// and a cancellation. The command itself is the call's input, not part of its
/// result.
fn bash_output(object: &Map<String, Value>) -> String {
    let mut text =
        trim_blank_lines(object.get("output").and_then(Value::as_str).unwrap_or("")).to_owned();
    if let Some(code) = object.get("exitCode").and_then(Value::as_i64)
        && code != 0
    {
        append_exit_code(&mut text, code);
    }
    if object.get("cancelled").and_then(Value::as_bool) == Some(true) {
        append_output_note(&mut text, "Command cancelled");
    }
    tool_result_text(&Value::String(text)).unwrap_or_default()
}

fn pi_usage(value: &Value) -> Option<TokenUsage> {
    let object = value.as_object()?;
    Some(TokenUsage {
        input_tokens: object.get("input").and_then(Value::as_u64).unwrap_or(0),
        output_tokens: object.get("output").and_then(Value::as_u64).unwrap_or(0),
        cache_creation_input_tokens: object
            .get("cacheWrite")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_input_tokens: object.get("cacheRead").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// Append a `session_info` record titling the Pi session at `path`, read
/// with `permit`, written at `renamed_at`.
pub fn append_session_rename(
    permit: crate::history::ParsePermit<'_>,
    path: &Path,
    title: &str,
    renamed_at: DateTime<Utc>,
) -> Result<()> {
    let projection = PI_LOG.parse_transcript(permit, path)?.ok_or_else(|| {
        AppError::ConfigError(format!("{} is not a valid Pi session", path.display()))
    })?;
    let mut ids = HashSet::new();
    let file = File::open(path)?;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        if let Ok(value) = serde_json::from_str::<Value>(&line)
            && let Some(id) = value.get("id").and_then(Value::as_str)
        {
            ids.insert(id.to_owned());
        }
    }
    let id = unused_record_id(&projection.header.id, renamed_at, &ids);
    let entry = json!({
        "type": "session_info",
        "id": id,
        "parentId": projection.leaf_id,
        "timestamp": renamed_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        "name": title.replace(['\r', '\n'], " ").trim(),
    });
    let mut file = OpenOptions::new().append(true).open(path)?;
    writeln!(file, "{entry}")?;
    Ok(())
}

/// Write the OMP session at `path`, read with `permit`, with a title slot and
/// a `title_change` record naming `title`, both stamped `renamed_at`.
pub fn append_omp_session_rename(
    permit: crate::history::ParsePermit<'_>,
    path: &Path,
    title: &str,
    renamed_at: DateTime<Utc>,
) -> Result<()> {
    const TITLE_SLOT_BYTES: usize = 256;

    let projection = OMP_LOG.parse_transcript(permit, path)?.ok_or_else(|| {
        AppError::ConfigError(format!("{} is not a valid OMP session", path.display()))
    })?;
    let title = title.replace(['\r', '\n'], " ").trim().to_owned();
    let timestamp = renamed_at.to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut ids = HashSet::new();
    let contents = std::fs::read_to_string(path)?;
    for line in contents.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line)
            && let Some(id) = value.get("id").and_then(Value::as_str)
        {
            ids.insert(id.to_owned());
        }
    }
    let id = unused_record_id(&projection.header.id, renamed_at, &ids);
    let entry = json!({
        "type": "title_change",
        "id": id,
        "parentId": projection.leaf_id,
        "timestamp": timestamp,
        "title": title,
        "previousTitle": projection.title,
        "source": "user",
    });
    let slot = omp_title_slot(&title, &timestamp, TITLE_SLOT_BYTES)?;
    let mut lines = contents.split_inclusive('\n');
    let first = lines.next().unwrap_or_default();
    let mut rewritten = String::new();
    if serde_json::from_str::<Value>(first.trim_end_matches('\n'))
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .as_deref()
        == Some("title")
    {
        rewritten.push_str(&slot);
    } else {
        rewritten.push_str(&slot);
        rewritten.push_str(first);
    }
    rewritten.extend(lines);
    if !rewritten.ends_with('\n') {
        rewritten.push('\n');
    }
    rewritten.push_str(&entry.to_string());
    rewritten.push('\n');

    let parent = path.parent().ok_or_else(|| {
        AppError::ConfigError(format!("{} has no parent directory", path.display()))
    })?;
    let permissions = std::fs::metadata(path)?.permissions();
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.as_file_mut().set_permissions(permissions)?;
    temp.write_all(rewritten.as_bytes())?;
    temp.as_file_mut().sync_all()?;
    temp.persist(path)
        .map_err(|error| AppError::Io(error.error))?;
    Ok(())
}

/// The first eight-hex-digit record id derived from `session_id` and
/// `written_at` that `taken` does not hold.
fn unused_record_id(
    session_id: &str,
    written_at: DateTime<Utc>,
    taken: &HashSet<String>,
) -> String {
    let nanos = written_at.timestamp_nanos_opt().unwrap_or(0);
    (0u64..)
        .map(|counter| {
            let seed = format!("{session_id}:{nanos}:{counter}");
            blake3::hash(seed.as_bytes()).to_hex()[..8].to_owned()
        })
        .find(|id| !taken.contains(id))
        .expect("unbounded ID sequence has a free value")
}

fn omp_title_slot(title: &str, timestamp: &str, bytes: usize) -> Result<String> {
    let mut title = title.to_owned();
    loop {
        let empty = json!({
            "type": "title",
            "v": 1,
            "title": title,
            "source": "user",
            "updatedAt": timestamp,
            "pad": "",
        });
        let base = format!("{empty}\n");
        if base.len() <= bytes {
            let padding = " ".repeat(bytes - base.len());
            let line = json!({
                "type": "title",
                "v": 1,
                "title": title,
                "source": "user",
                "updatedAt": timestamp,
                "pad": padding,
            });
            let serialized = format!("{line}\n");
            if serialized.len() == bytes {
                return Ok(serialized);
            }
        }
        if title.pop().is_none() {
            return Err(AppError::ConfigError(
                "OMP title slot metadata exceeds 256 bytes".to_owned(),
            ));
        }
    }
}

/// Fixtures for a session directory Pi and OMP share.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    /// The OMP session fixture copied twice into `directory`, as written and
    /// without its title slot, so both state one header id and the list shows
    /// the first under OMP and the second under Pi.
    pub(crate) fn write_titled_and_untitled(directory: &Path) -> (PathBuf, PathBuf) {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/v3.jsonl");
        std::fs::create_dir_all(directory).unwrap();
        let titled = directory.join("titled.jsonl");
        std::fs::copy(&fixture, &titled).unwrap();
        let untitled = directory.join("untitled.jsonl");
        copy_without_title_slot(&fixture, &untitled);
        (titled, untitled)
    }

    fn copy_without_title_slot(fixture: &Path, destination: &Path) {
        let contents = std::fs::read_to_string(fixture).unwrap();
        let (title_slot, rest) = contents.split_once('\n').unwrap();
        assert!(
            title_slot.contains(r#""type":"title""#),
            "{fixture:?} has no title slot"
        );
        std::fs::write(destination, rest).unwrap();
    }
}

#[cfg(test)]
mod tests {

    fn omp_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/v3.jsonl")
    }

    #[test]
    fn header_states_session_id_whatever_file_is_called() {
        assert_eq!(
            header_session_id(&fixture("v1.jsonl")).as_deref(),
            Some("custom_v1_id")
        );
    }

    /// OMP writes a title slot ahead of the header, so the id is on the second
    /// record rather than the first.
    #[test]
    fn title_slot_ahead_of_header_does_not_hide_session_id() {
        assert_eq!(
            header_session_id(&omp_fixture()).as_deref(),
            Some("omp_session_custom_id")
        );
    }

    #[test]
    fn file_that_is_not_pi_family_log_states_no_session_id() {
        let directory = tempfile::tempdir().unwrap();
        let stray = directory.path().join("stray.jsonl");
        std::fs::write(
            &stray,
            "{\"type\":\"user\",\"message\":{}}
",
        )
        .unwrap();

        assert_eq!(header_session_id(&stray), None);
        assert_eq!(
            header_session_id(&directory.path().join("absent.jsonl")),
            None
        );
    }

    /// The header attributes every Pi-family fixture as the full parse does:
    /// by every registered format in turn, as a shared session directory
    /// reads a log, and by each agent's own reader, as its own tree does.
    #[test]
    fn header_attributes_log_as_its_parse_does() {
        let fixtures = [
            fixture("v1.jsonl"),
            fixture("v2.jsonl"),
            fixture("v3-branched.jsonl"),
            omp_fixture(),
            omp_fixture().with_file_name("subagent.jsonl"),
        ];
        let attribution = |projection: Option<SessionProjection>| {
            projection.map(|projection| (projection.header.id, projection.source))
        };
        for path in fixtures {
            let shown = path.display();
            assert_eq!(
                read_header(&path, Source::Pi).unwrap(),
                attribution(
                    crate::history::format::parse_transcript(crate::history::FOR_FIXTURES, &path)
                        .unwrap()
                ),
                "registry: {shown}"
            );
            assert_eq!(
                read_header(&path, Source::Pi).unwrap(),
                attribution(
                    PI_LOG
                        .parse_transcript(crate::history::FOR_FIXTURES, &path)
                        .unwrap()
                ),
                "Pi's reader: {shown}"
            );
            assert_eq!(
                read_header(&path, Source::Omp).unwrap(),
                attribution(
                    OMP_LOG
                        .parse_transcript(crate::history::FOR_FIXTURES, &path)
                        .unwrap()
                ),
                "OMP's reader: {shown}"
            );
        }
    }

    /// Two logs in one project can state the same id — a branch keeps the id
    /// it was copied from — so every match comes back.
    #[test]
    fn every_log_stating_id_is_found() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let first = project.join("session.jsonl");
        let copy = project.join("branch.jsonl");
        std::fs::copy(fixture("v1.jsonl"), &first).unwrap();
        std::fs::copy(fixture("v1.jsonl"), &copy).unwrap();

        let mut found = sessions_with_id(directory.path(), 1, "custom_v1_id").unwrap();
        found.sort();

        assert_eq!(found, vec![copy, first]);
        assert!(
            sessions_with_id(directory.path(), 1, "no_such_id")
                .unwrap()
                .is_empty()
        );
    }

    /// Pi generates a UUID unless the user chose an id, so a UUID is found in
    /// any case while a chosen id is found as typed.
    #[test]
    fn uuid_in_header_is_found_in_any_case_and_chosen_id_as_typed() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let generated = project.join("generated.jsonl");
        std::fs::write(
            &generated,
            "{\"type\":\"session\",\"version\":3,\"id\":\"01a05936-420b-7f5a-bb7a-1cb44cb94841\",\"timestamp\":\"2026-08-31T19:05:23.211Z\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        std::fs::copy(fixture("v1.jsonl"), project.join("chosen.jsonl")).unwrap();

        assert_eq!(
            sessions_with_id(directory.path(), 1, "01A05936-420B-7F5A-BB7A-1CB44CB94841").unwrap(),
            vec![generated]
        );
        assert!(
            sessions_with_id(directory.path(), 1, "CUSTOM_V1_ID")
                .unwrap()
                .is_empty()
        );
    }
    use super::*;
    use crate::agent::transcript::AgentTranscript;
    use crate::clock::TestClock;
    use crate::history::format::dropped::DropOutcome;
    use crate::history::{Source, parser::process_conversation_file};

    /// The same session with OMP's `title` slot in front. The slot attributes
    /// the session to OMP.
    fn with_omp_title_slot(session: &str) -> String {
        format!("{}\n{session}", r#"{"type":"title","v":1,"title":"Named"}"#)
    }

    /// Reads `content` with `Source::Pi` as the default, then asserts the
    /// source the reader attributes it to and the kinds it drops, each counted
    /// once under `session format 3`, in the report's order.
    fn assert_counted_drops(content: &str, source: Source, drops: &[(&str, DropOutcome)]) {
        let projection = parse_reader(
            std::io::Cursor::new(content),
            Source::Pi,
            DroppedRecords::counting(),
        )
        .unwrap()
        .expect("a session");

        assert_eq!(projection.source, source);
        let expected = drops
            .iter()
            .map(|&(kind, outcome)| (Some("session format 3"), kind, outcome, 1))
            .collect::<Vec<_>>();
        assert_eq!(
            projection.dropped.counts().collect::<Vec<_>>(),
            expected,
            "{source:?}"
        );
    }

    #[test]
    fn counting_names_skipped_and_unrecognised_entry_types_and_roles_for_pi_and_omp() {
        let session = [
            r#"{"type":"session","version":3,"id":"s","timestamp":"2024-01-01T00:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"teleport","id":"b","parentId":"a"}"#,
            r#"{"type":"label","id":"c","parentId":"b"}"#,
            r#"{"type":"message","id":"d","parentId":"c","message":{"role":"oracle","content":"x"}}"#,
        ]
        .join("\n");

        for (content, source) in [
            (session.clone(), Source::Pi),
            (with_omp_title_slot(&session), Source::Omp),
        ] {
            assert_counted_drops(
                &content,
                source,
                &[
                    ("message.role=oracle", DropOutcome::Unrecognised),
                    ("type=label", DropOutcome::Skipped),
                    ("type=teleport", DropOutcome::Unrecognised),
                ],
            );
        }
    }

    /// The header settles from the first two records, past a line between
    /// them that holds no record; that line is malformed, in file order with
    /// a malformed entry after it.
    #[test]
    fn malformed_line_between_title_slot_and_header_leaves_the_header() {
        let content = [
            r#"{"type":"title","v":1,"title":"Named"}"#,
            "not a record",
            r#"{"type":"session","version":3,"id":"s","timestamp":"2024-01-01T00:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"message","message":{"role":"user","content":"without an id"}}"#,
        ]
        .join("\n");

        let projection = parse_reader(
            std::io::Cursor::new(content),
            Source::Pi,
            DroppedRecords::default(),
        )
        .unwrap()
        .expect("a session");

        assert_eq!(
            (
                projection.source,
                projection.header.id.as_str(),
                projection.title.as_deref(),
                projection.malformed_lines.clone(),
            ),
            (Source::Omp, "s", Some("Named"), vec![2, 5])
        );
        assert_eq!(projection.entries.len(), 2, "the title, then the prompt");
    }

    /// Every record is projected as it is read, on whichever branch; the
    /// report counts the drops of the branch the log shows.
    #[test]
    fn counting_leaves_out_records_on_an_abandoned_branch() {
        let session = [
            r#"{"type":"session","version":3,"id":"s","timestamp":"2024-01-01T00:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"teleport","id":"abandoned","parentId":"a"}"#,
            r#"{"type":"label","id":"shown","parentId":"a"}"#,
        ]
        .join("\n");

        for (content, source) in [
            (session.clone(), Source::Pi),
            (with_omp_title_slot(&session), Source::Omp),
        ] {
            assert_counted_drops(&content, source, &[("type=label", DropOutcome::Skipped)]);
        }
    }

    #[test]
    fn kinds_pi_review_decided_against_count_as_skipped_for_pi_only() {
        let session = [
            r#"{"type":"session","version":3,"id":"s","timestamp":"2024-01-01T00:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"label","id":"b","parentId":"a"}"#,
            r#"{"type":"context_edit","id":"c","parentId":"b"}"#,
            r#"{"type":"message","id":"d","parentId":"c","message":{"role":"system","content":"prompt"}}"#,
        ]
        .join("\n");

        for (content, source, decided_outcome) in [
            (session.clone(), Source::Pi, DropOutcome::Skipped),
            (
                with_omp_title_slot(&session),
                Source::Omp,
                DropOutcome::Unrecognised,
            ),
        ] {
            assert_counted_drops(
                &content,
                source,
                &[
                    ("message.role=system", decided_outcome),
                    ("type=context_edit", decided_outcome),
                    ("type=label", DropOutcome::Skipped),
                ],
            );
        }
    }

    #[test]
    fn kinds_omp_review_decided_against_count_as_skipped_for_omp_only_and_model_usage_stays_unrecognised()
     {
        let session = [
            r#"{"type":"session","version":3,"id":"s","timestamp":"2024-01-01T00:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"mode_change","id":"b","parentId":"a","mode":"plan"}"#,
            r#"{"type":"session_init","id":"c","parentId":"b","task":"review"}"#,
            r#"{"type":"model_usage","id":"d","parentId":"c","usage":{"input":1,"output":2}}"#,
            r#"{"type":"message","id":"e","parentId":"d","message":{"role":"developer","content":"rules"}}"#,
        ]
        .join("\n");

        for (content, source, decided_outcome) in [
            (session.clone(), Source::Pi, DropOutcome::Unrecognised),
            (
                with_omp_title_slot(&session),
                Source::Omp,
                DropOutcome::Skipped,
            ),
        ] {
            assert_counted_drops(
                &content,
                source,
                &[
                    ("message.role=developer", decided_outcome),
                    ("type=mode_change", decided_outcome),
                    ("type=model_usage", DropOutcome::Unrecognised),
                    ("type=session_init", decided_outcome),
                ],
            );
        }
    }

    #[test]
    fn omp_clear_shows_context_cleared_row_between_turns_before_and_after_it() {
        let session = [
            r#"{"type":"session","version":3,"id":"s","timestamp":"2024-01-01T00:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"a","parentId":null,"timestamp":"2024-01-01T00:00:01Z","message":{"role":"user","content":"before"}}"#,
            r#"{"type":"reset_boundary","id":"b","parentId":"a","timestamp":"2024-01-01T00:00:02Z"}"#,
            r#"{"type":"message","id":"c","parentId":"b","timestamp":"2024-01-01T00:00:03Z","message":{"role":"user","content":"after"}}"#,
        ]
        .join("\n");
        let projection = parse_reader(
            std::io::Cursor::new(with_omp_title_slot(&session)),
            Source::Pi,
            DroppedRecords::counting(),
        )
        .unwrap()
        .expect("a session");

        let entries = projection
            .entries
            .iter()
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();
        assert!(
            matches!(
                entries.as_slice(),
                [
                    LogEntry::CustomTitle { .. },
                    LogEntry::User {
                        message: UserMessage {
                            content: UserContent::String(before),
                            ..
                        },
                        ..
                    },
                    LogEntry::PiMetadata {
                        label,
                        text,
                        timestamp: Some(timestamp),
                        searchable: true,
                        ..
                    },
                    LogEntry::User {
                        message: UserMessage {
                            content: UserContent::String(after),
                            ..
                        },
                        ..
                    },
                ] if before == "before"
                    && label == "/clear"
                    && text == "Context cleared"
                    && timestamp == "2024-01-01T00:00:02Z"
                    && after == "after"
            ),
            "{entries:#?}"
        );
        assert_eq!(projection.dropped.counts().count(), 0);
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pi")
            .join(name)
    }

    /// The blocks and timestamp of the first user entry whose blocks satisfy
    /// `wanted`.
    fn first_user_entry_with(
        projection: &SessionProjection,
        wanted: impl Fn(&[ContentBlock]) -> bool,
    ) -> Option<(&[ContentBlock], Option<&str>)> {
        projection
            .entries
            .iter()
            .find_map(|(_, entry)| match entry {
                LogEntry::User {
                    message, timestamp, ..
                } => match &message.content {
                    UserContent::Blocks(blocks) if wanted(blocks) => {
                        Some((blocks.as_slice(), timestamp.as_deref()))
                    }
                    _ => None,
                },
                _ => None,
            })
    }

    #[test]
    fn parses_versions_one_through_three_without_rewriting() {
        for (name, version, id) in [
            ("v1.jsonl", 1, "custom_v1_id"),
            ("v2.jsonl", 2, "custom_id_with_underscores"),
            (
                "v3-branched.jsonl",
                3,
                "01912345-6789-7abc-8def-0123456789ab",
            ),
        ] {
            let before = std::fs::read(fixture(name)).unwrap();
            let projection = PI_LOG
                .parse_transcript(crate::history::FOR_FIXTURES, &fixture(name))
                .unwrap()
                .unwrap();
            assert_eq!(projection.header.version, version);
            assert_eq!(projection.header.id, id);
            assert_eq!(std::fs::read(fixture(name)).unwrap(), before);
        }
    }

    #[test]
    fn projects_only_active_branch_and_sanitizes_images() {
        let projection = PI_LOG
            .parse_transcript(crate::history::FOR_FIXTURES, &fixture("v3-branched.jsonl"))
            .unwrap()
            .unwrap();
        let json = projection
            .entries
            .iter()
            .map(|(_, entry)| serde_json::to_string(entry).unwrap())
            .collect::<Vec<_>>()
            .join(" ");

        assert!(json.contains("active root question"));
        assert!(json.contains("branch summary searchable"));
        assert!(json.contains("compaction summary searchable"));
        assert!(json.contains("tool output searchable"));
        assert!(json.contains("bash output searchable"));
        assert!(json.contains("[Image: image/jpeg]"));
        assert!(!json.contains("ABANDONED_BRANCH_SENTINEL"));
        assert!(!json.contains("ABANDONED_ANSWER_SENTINEL"));
        assert!(!json.contains("HIDDEN_CUSTOM_SENTINEL"));
        assert!(!json.contains("BASE64_SECRET"));
    }

    /// Pi records a command the user ran as one row holding the command and
    /// what it printed. Both halves are the user's, so they pair as a call
    /// and its result under the user's own label.
    #[test]
    fn user_run_command_reads_as_call_and_its_result() {
        let projection = PI_LOG
            .parse_transcript(crate::history::FOR_FIXTURES, &fixture("v3-branched.jsonl"))
            .unwrap()
            .unwrap();

        let (blocks, timestamp) = first_user_entry_with(&projection, |blocks| {
            blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
        })
        .expect("the bash execution renders as a user entry holding a call");

        assert_eq!(
            timestamp,
            Some("2024-03-01T00:07:00.000Z"),
            "the entry opens a run, which is stamped from the record"
        );

        let [
            ContentBlock::ToolUse {
                id,
                name,
                tool,
                input,
            },
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            },
        ] = blocks
        else {
            panic!("expected a call and its result, got {blocks:?}");
        };
        assert_eq!(*tool, Tool::UserShell);
        assert_eq!(name, "bash");
        assert_eq!(input, &json!({ "command": "false" }));
        assert_eq!(id, "ran-a-command", "the call takes the record's own id");
        assert_eq!(tool_use_id, id, "the result answers that call");

        let result = content.as_ref().unwrap().as_str().unwrap();
        assert!(result.contains("bash output searchable"));
        assert!(result.contains("Exit code: 1"));
        assert!(
            !result.contains("$ false"),
            "the command is the call's input, not part of its result: {result:?}"
        );
    }

    /// Pi writes a `toolResult` record when the result arrives, so the
    /// projected result carries that record's stamp and ends the run.
    #[test]
    fn tool_result_is_stamped_from_its_record() {
        let projection = PI_LOG
            .parse_transcript(crate::history::FOR_FIXTURES, &fixture("v3-branched.jsonl"))
            .unwrap()
            .unwrap();

        let (_, timestamp) = first_user_entry_with(&projection, |blocks| {
            blocks
                .iter()
                .all(|block| matches!(block, ContentBlock::ToolResult { .. }))
        })
        .expect("the tool result renders as a user entry holding only the result");

        assert_eq!(timestamp, Some("2024-03-01T00:06:00.000Z"));
    }

    /// A cancelled command and a failed one each say so in the result, where
    /// the command's own output ends.
    #[test]
    fn command_that_failed_or_was_cancelled_says_so_in_its_result() {
        let row = |extra: &str| {
            let object = serde_json::from_str::<Value>(&format!(
                r#"{{"command":"sleep 5","output":"partial"{extra}}}"#
            ))
            .unwrap();
            bash_output(object.as_object().unwrap())
        };

        assert_eq!(row(""), "partial");
        assert_eq!(row(r#","exitCode":0"#), "partial");
        assert_eq!(row(r#","exitCode":130"#), "partial\nExit code: 130");
        assert_eq!(row(r#","cancelled":true"#), "partial\nCommand cancelled");
    }

    #[test]
    fn extracts_pi_conversation_metadata_from_active_dialogue() {
        let conversation = process_conversation_file(fixture("v3-branched.jsonl"), None, None)
            .unwrap()
            .unwrap();

        assert_eq!(conversation.source, Source::Pi);
        assert_eq!(
            conversation.session_id,
            "01912345-6789-7abc-8def-0123456789ab"
        );
        assert_eq!(conversation.custom_title.as_deref(), Some("Final Pi title"));
        assert_eq!(conversation.model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(conversation.total_tokens, 190);
        // The session's last activity is the command the user ran at 00:07,
        // which the reader now stamps, rather than the answer before it.
        assert_eq!(conversation.duration_minutes, Some(6));
        assert_eq!(conversation.timestamp.timestamp(), 1_709_251_620);
        for searchable in [
            "tool output searchable",
            "bash output searchable",
            "visible custom searchable",
        ] {
            assert!(
                conversation.full_text.contains(searchable),
                "missing {searchable:?} from {:?}",
                conversation.full_text
            );
        }
        for hidden_metadata in [
            "compaction summary searchable",
            "branch summary searchable",
            "custom state searchable",
        ] {
            assert!(!conversation.full_text.contains(hidden_metadata));
        }
        assert!(!conversation.full_text.contains("HIDDEN_CUSTOM_SENTINEL"));
        assert!(!conversation.full_text.contains("ABANDONED_BRANCH_SENTINEL"));
        assert!(
            !conversation
                .preview
                .contains("compaction summary searchable")
        );
    }

    #[test]
    fn semantic_ranges_and_agent_ordinals_share_pi_projection() {
        let path = fixture("v3-branched.jsonl");
        let conversation = process_conversation_file(path.clone(), None, None)
            .unwrap()
            .unwrap();
        let transcript = AgentTranscript::load(path).unwrap();

        assert_eq!(conversation.message_count, 5);
        assert_eq!(transcript.messages.len(), 5);
        assert_eq!(
            conversation.semantic_turn_ranges,
            [1, 2, 5]
                .into_iter()
                .map(crate::agent::refs::MessageRange::single)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            transcript
                .messages
                .iter()
                .map(|message| message.ordinal)
                .collect::<Vec<_>>(),
            (1..=5).collect::<Vec<_>>()
        );
    }

    #[test]
    fn malformed_lines_and_parent_chains_are_bounded_to_leaf_suffix() {
        let content = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"root\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"lost root\"}}\n",
            "malformed\n",
            "{\"type\":\"branch_summary\",\"id\":\"leaf\",\"parentId\":\"missing\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"summary\":\"leaf suffix\"}\n"
        );
        let projection = parse_reader(
            std::io::Cursor::new(content),
            Source::Pi,
            DroppedRecords::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(projection.malformed_lines, vec![3]);
        assert_eq!(projection.entries.len(), 1);
        assert!(matches!(
            &projection.entries[0].1,
            LogEntry::PiMetadata { text, .. } if text == "leaf suffix"
        ));
    }

    #[test]
    fn rejects_header_only_and_invalid_headers_as_conversations() {
        let invalid = std::io::Cursor::new("{\"type\":\"user\",\"id\":\"x\"}\n");
        assert!(
            parse_reader(invalid, Source::Pi, DroppedRecords::default())
                .unwrap()
                .is_none()
        );

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("header.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        assert!(
            process_conversation_file(path, None, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn empty_session_name_clears_active_title() {
        let content = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n",
            "{\"type\":\"session_info\",\"id\":\"n1\",\"parentId\":\"u\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"name\":\"title\"}\n",
            "{\"type\":\"session_info\",\"id\":\"n2\",\"parentId\":\"n1\",\"timestamp\":\"2024-01-01T00:00:03Z\",\"name\":\"\"}\n"
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cleared.jsonl");
        std::fs::write(&path, content).unwrap();
        let conversation = process_conversation_file(path, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(conversation.custom_title, None);
    }

    #[test]
    fn skips_v2_and_v3_entries_without_ids_before_resolving_leaf() {
        let content = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"root\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n",
            "{\"type\":\"message\",\"parentId\":\"root\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"message\":{\"role\":\"user\",\"content\":\"invalid leaf\"}}\n"
        );
        let projection = parse_reader(
            std::io::Cursor::new(content),
            Source::Pi,
            DroppedRecords::default(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(projection.leaf_id.as_deref(), Some("root"));
        assert_eq!(projection.malformed_lines, vec![3]);
        assert_eq!(projection.entries.len(), 1);
    }

    #[test]
    fn migrates_v2_hook_messages_in_memory_and_counts_non_assistant_usage() {
        let content = concat!(
            "{\"type\":\"session\",\"version\":2,\"id\":\"s\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n",
            "{\"type\":\"message\",\"id\":\"h\",\"parentId\":\"u\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"message\":{\"role\":\"hookMessage\",\"customType\":\"extension notice\",\"content\":\"visible hook\",\"display\":true}}\n",
            "{\"type\":\"compaction\",\"id\":\"c\",\"parentId\":\"h\",\"timestamp\":\"2024-01-01T00:00:03Z\",\"summary\":\"summary\",\"firstKeptEntryId\":\"u\",\"tokensBefore\":1,\"usage\":{\"input\":2,\"output\":3,\"cacheRead\":5,\"cacheWrite\":7}}\n",
            "{\"type\":\"message\",\"id\":\"t\",\"parentId\":\"c\",\"timestamp\":\"2024-01-01T00:00:04Z\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"call\",\"content\":[],\"usage\":{\"input\":11,\"output\":13,\"cacheRead\":17,\"cacheWrite\":19}}}\n"
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("usage.jsonl");
        std::fs::write(&path, content).unwrap();

        let conversation = process_conversation_file(path, None, None)
            .unwrap()
            .unwrap();
        assert!(conversation.full_text.contains("visible hook"));
        assert_eq!(conversation.total_tokens, 77);
        let projection = parse_reader(
            std::io::Cursor::new(content),
            Source::Pi,
            DroppedRecords::default(),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(
            &projection.entries[1].1,
            LogEntry::PiMetadata { label, .. } if label == "extension notice"
        ));
    }

    #[test]
    fn usage_entries_of_every_kind_add_to_token_count() {
        let content = [
            r#"{"type":"session","version":3,"id":"s","timestamp":"2024-01-01T00:00:00Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"u","parentId":null,"timestamp":"2024-01-01T00:00:01Z","message":{"role":"user","content":"question"}}"#,
            r#"{"type":"message","id":"a","parentId":"u","timestamp":"2024-01-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"answer"}],"usage":{"input":1,"output":2,"cacheRead":3,"cacheWrite":4}}}"#,
            r#"{"type":"usage","id":"w","parentId":"a","timestamp":"2024-01-01T00:00:03Z","kind":"cache_warm","provider":"anthropic","model":"m","usage":{"input":10,"output":0,"cacheRead":100,"cacheWrite":0,"totalTokens":110}}"#,
            r#"{"type":"usage","id":"x","parentId":"w","timestamp":"2024-01-01T00:00:04Z","kind":"another_kind","provider":"anthropic","model":"m","usage":{"input":5,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":5}}"#,
        ]
        .join("\n");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("usage.jsonl");
        std::fs::write(&path, &content).unwrap();

        let conversation = process_conversation_file(path, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(conversation.total_tokens, 10 + 110 + 5);

        let projection = parse_reader(
            std::io::Cursor::new(content),
            Source::Pi,
            DroppedRecords::counting(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            projection.dropped.counts().count(),
            0,
            "the report doesn't list a usage entry"
        );
    }

    #[test]
    fn rename_appends_native_session_info_to_leaf() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::copy(fixture("v3-branched.jsonl"), &path).unwrap();

        append_session_rename(
            crate::history::FOR_FIXTURES,
            &path,
            "renamed\nPi",
            TestClock::start_wall(),
        )
        .unwrap();
        let last = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .last()
            .map(str::to_owned)
            .unwrap();
        let value: Value = serde_json::from_str(&last).unwrap();
        assert_eq!(value["type"], "session_info");
        assert_eq!(value["parentId"], "name2");
        assert_eq!(value["name"], "renamed Pi");
        assert_eq!(value["timestamp"], "2026-07-01T12:00:00.000Z");
        assert_eq!(value["id"].as_str().unwrap().len(), 8);
    }

    #[test]
    fn parses_omp_title_slot_and_active_branch() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/v3.jsonl");
        let projection = PI_LOG
            .parse_transcript(crate::history::FOR_FIXTURES, &path)
            .unwrap()
            .unwrap();
        assert_eq!(projection.source, Source::Omp);
        assert_eq!(projection.header.id, "omp_session_custom_id");
        assert_eq!(projection.title.as_deref(), Some("OMP fixture title"));
        let conversation = process_conversation_file(path, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(conversation.source, Source::Omp);
        assert_eq!(
            conversation.custom_title.as_deref(),
            Some("Updated OMP title")
        );
        assert_eq!(conversation.model.as_deref(), Some("openai/gpt-5.2"));
        assert!(conversation.full_text.contains("OMP active question"));
        assert!(!conversation.full_text.contains("OMP_ABANDONED_SENTINEL"));
    }

    #[test]
    fn omp_rename_updates_title_slot_and_appends_audit_entry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/v3.jsonl"),
            &path,
        )
        .unwrap();

        append_omp_session_rename(
            crate::history::FOR_FIXTURES,
            &path,
            "renamed\nOMP",
            TestClock::start_wall(),
        )
        .unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let first_line = contents.split_inclusive('\n').next().unwrap();
        assert_eq!(first_line.len(), 256);
        let slot: Value = serde_json::from_str(first_line).unwrap();
        assert_eq!(slot["type"], "title");
        assert_eq!(slot["title"], "renamed OMP");
        assert_eq!(slot["updatedAt"], "2026-07-01T12:00:00.000Z");
        let last: Value = serde_json::from_str(contents.lines().last().unwrap()).unwrap();
        assert_eq!(last["type"], "title_change");
        assert_eq!(last["parentId"], "a2");
        assert_eq!(last["title"], "renamed OMP");
        assert_eq!(last["timestamp"], "2026-07-01T12:00:00.000Z");
        let reparsed = OMP_LOG
            .parse_transcript(crate::history::FOR_FIXTURES, &path)
            .unwrap()
            .unwrap();
        assert_eq!(reparsed.title.as_deref(), Some("renamed OMP"));
    }

    /// A nested output id is dot-qualified, `Parent.Child.jsonl`, and the
    /// whole of it is the label; only the extension comes off.
    #[test]
    fn threads_label_is_its_file_stem() {
        let directory = tempfile::tempdir().unwrap();
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/subagent.jsonl");
        for (name, label) in [
            ("worker.jsonl", "worker"),
            ("worker.reviewer.jsonl", "worker.reviewer"),
        ] {
            let path = directory.path().join(name);
            std::fs::copy(&source, &path).unwrap();

            let projection = OMP_LOG
                .parse_transcript(crate::history::FOR_FIXTURES, &path)
                .unwrap()
                .unwrap();

            assert_eq!(projection.header.thread_label(), label);
            assert_eq!(projection.header.id, "omp_subagent_id");
        }
    }

    mod resumed_reads {
        use super::*;
        use crate::history::format::lines::record_splits;
        use crate::history::provider::contract_tests::every_record_sessions;
        use crate::history::provider::transcript_reads::resume;

        fn read_whole(format: &PiLogFormat, path: &Path) -> Option<SessionProjection> {
            format
                .parse_transcript(crate::history::FOR_FIXTURES, path)
                .unwrap()
        }

        /// `entries` as text a test compares.
        fn listed(entries: &[(RecordLine, LogEntry)]) -> Vec<String> {
            entries
                .iter()
                .map(|(record, entry)| format!("{} {entry:?}", record.line))
                .collect()
        }

        /// Asserts that `prefix`, a read of a log's beginning, up to the
        /// window's first line, and then `window`, the read resumed from it
        /// over the whole log, hold what `full`, one read of the whole log,
        /// holds, and leave what it leaves for the next read.
        fn assert_window_completes(
            name: &str,
            prefix: &SessionProjection,
            window: &ResumedWindow,
            full: &SessionProjection,
        ) {
            let kept = prefix
                .entries
                .iter()
                .filter(|(record, _)| record.line < window.start_line)
                .chain(&window.entries)
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(
                listed(&kept),
                listed(&full.entries),
                "{name}: the prefix and the window hold the full read's entries"
            );
            let malformed = prefix
                .malformed_lines
                .iter()
                .filter(|line| **line < window.start_line)
                .copied()
                .chain(window.malformed_lines.iter().map(|line| line.line_number))
                .collect::<Vec<_>>();
            assert_eq!(
                malformed, full.malformed_lines,
                "{name}: the prefix and the window hold the full read's malformed lines"
            );
            assert_eq!(
                window.read, full.transcript_read,
                "{name}: the window leaves what the full read leaves"
            );
        }

        /// A Pi log whose second branch leaves the first, with a malformed
        /// line and an entry without an id inside the shown branch.
        fn branch_with_malformed_lines() -> String {
            [
                r#"{"type":"session","version":3,"id":"s","timestamp":"2024-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
                r#"{"type":"message","id":"u1","parentId":null,"message":{"role":"user","content":"first question"}}"#,
                r#"{"type":"message","id":"a1","parentId":"u1","message":{"role":"assistant","content":[{"type":"text","text":"first answer"}]}}"#,
                "{not json",
                r#"{"type":"message","parentId":"a1","message":{"role":"user","content":"no id"}}"#,
                r#"{"type":"message","id":"u2","parentId":"u1","message":{"role":"user","content":"second branch"}}"#,
                r#"{"type":"message","id":"a2","parentId":"u2","message":{"role":"assistant","content":[{"type":"text","text":"second answer"}]}}"#,
            ]
            .join("\n")
                + "\n"
        }

        /// Every Pi and OMP fixture, the every-record sessions' logs and a
        /// branching log with malformed lines, each copied under `directory`
        /// with the reader that reads it.
        fn logs(directory: &Path) -> Vec<(&'static PiLogFormat, PathBuf)> {
            let mut logs: Vec<(&'static PiLogFormat, PathBuf)> = Vec::new();
            let mut copy = |format, path: &Path| {
                let copy = directory.join(format!("copy-{}.jsonl", logs.len()));
                std::fs::copy(path, &copy).unwrap();
                logs.push((format, copy));
            };
            // Each every-record session keeps its logs while it lives.
            for session in every_record_sessions() {
                let format = match session.row.source {
                    Source::Pi => &PI_LOG,
                    Source::Omp => &OMP_LOG,
                    _ => continue,
                };
                for path in std::iter::once(&session.row.path).chain(&session.row.subagents) {
                    copy(format, path);
                }
            }
            for name in ["v1.jsonl", "v2.jsonl", "v3-branched.jsonl"] {
                copy(&PI_LOG, &fixture(name));
            }
            copy(&OMP_LOG, &omp_fixture());
            copy(&OMP_LOG, &omp_fixture().with_file_name("subagent.jsonl"));
            let branching = directory.join("branching.jsonl");
            std::fs::write(&branching, branch_with_malformed_lines()).unwrap();
            logs.push((&PI_LOG, branching));
            logs
        }

        #[test]
        fn prefix_and_resumed_read_equal_full_read_at_every_split_of_every_log() {
            let directory = tempfile::tempdir().unwrap();
            let mut branch_switches = 0;
            for (format, path) in logs(directory.path()) {
                let bytes = std::fs::read(&path).unwrap();
                let full = read_whole(format, &path).unwrap();
                for split in record_splits(&bytes) {
                    let name = format!("{} at {split}", path.display());
                    std::fs::write(&path, &bytes[..split]).unwrap();
                    let prefix = read_whole(format, &path);
                    std::fs::write(&path, &bytes).unwrap();
                    let Some(prefix) = prefix else {
                        continue;
                    };

                    match resume(
                        format,
                        crate::history::FOR_FIXTURES,
                        &path,
                        &prefix.transcript_read,
                    )
                    .unwrap()
                    {
                        Resumption::Window(window) => assert_window_completes(
                            &format!("{name} from line {}", window.start_line),
                            &prefix,
                            &window,
                            &full,
                        ),
                        Resumption::ChangesEarlierEntry => {
                            let shown = listed(&full.entries);
                            assert!(
                                listed(&prefix.entries)
                                    .iter()
                                    .any(|entry| !shown.contains(entry)),
                                "{name}: only a log whose new entries hide an entry the \
                                 prefix shows reads whole"
                            );
                            branch_switches += 1;
                        }
                        Resumption::FullRead => assert_eq!(
                            prefix.transcript_read,
                            TranscriptRead::full_read(),
                            "{name}: only a log whose header has no line ending yet reads whole"
                        ),
                    }
                }
            }
            assert!(
                branch_switches > 0,
                "a split before a branch switch reads whole"
            );
        }

        #[test]
        fn branch_switch_reads_whole() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("session.jsonl");
            let bytes = std::fs::read_to_string(fixture("v3-branched.jsonl")).unwrap();
            let through_abandoned_answer = bytes.split_inclusive('\n').take(5).collect::<String>();
            std::fs::write(&path, &through_abandoned_answer).unwrap();
            let prefix = read_whole(&PI_LOG, &path).unwrap();
            std::fs::write(&path, &bytes).unwrap();

            let resumed = resume(
                &PI_LOG,
                crate::history::FOR_FIXTURES,
                &path,
                &prefix.transcript_read,
            )
            .unwrap();

            assert!(
                matches!(resumed, Resumption::ChangesEarlierEntry),
                "the branch summary returns to the root answer: {resumed:?}"
            );
        }

        /// An OMP log whose title slot OMP's own rename wrote: 256 bytes.
        fn renamed_omp_log(directory: &Path) -> PathBuf {
            let path = directory.join("session.jsonl");
            std::fs::copy(omp_fixture(), &path).unwrap();
            append_omp_session_rename(
                crate::history::FOR_FIXTURES,
                &path,
                "First name",
                TestClock::start_wall(),
            )
            .unwrap();
            path
        }

        #[test]
        fn title_slot_rewritten_in_place_resumes_with_new_title() {
            let directory = tempfile::tempdir().unwrap();
            let path = renamed_omp_log(directory.path());
            let before = read_whole(&OMP_LOG, &path).unwrap();

            append_omp_session_rename(
                crate::history::FOR_FIXTURES,
                &path,
                "Second name",
                TestClock::start_wall(),
            )
            .unwrap();
            let full = read_whole(&OMP_LOG, &path).unwrap();
            let resumed = resume(
                &OMP_LOG,
                crate::history::FOR_FIXTURES,
                &path,
                &before.transcript_read,
            )
            .unwrap();

            let Resumption::Window(window) = resumed else {
                panic!("a title slot rewritten in place resumes: {resumed:?}");
            };
            assert_eq!(
                listed(&OMP_LOG.leading_entries(&path, &window.read).unwrap()),
                listed(&full.entries[..1]),
                "the leading entry is the slot's new title"
            );
            let mut resumed_entries = before.entries;
            resumed_entries.retain(|(record, _)| record.line < window.start_line);
            resumed_entries[0] = full.entries[0].clone();
            resumed_entries.extend(window.entries);
            assert_eq!(listed(&resumed_entries), listed(&full.entries));
        }

        #[test]
        fn title_slot_of_another_length_or_another_header_reads_whole() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("session.jsonl");
            std::fs::copy(omp_fixture(), &path).unwrap();
            let read = read_whole(&OMP_LOG, &path).unwrap().transcript_read;

            append_omp_session_rename(
                crate::history::FOR_FIXTURES,
                &path,
                "A name in a 256-byte slot",
                TestClock::start_wall(),
            )
            .unwrap();
            assert_eq!(
                OMP_LOG.identity(&path).unwrap(),
                read.identity,
                "the title slot is not part of the identity"
            );
            assert!(
                matches!(
                    resume(&OMP_LOG, crate::history::FOR_FIXTURES, &path, &read).unwrap(),
                    Resumption::FullRead
                ),
                "a slot of another length moves every record"
            );

            let path = renamed_omp_log(directory.path());
            let read = read_whole(&OMP_LOG, &path).unwrap().transcript_read;
            let bytes = std::fs::read_to_string(&path).unwrap();
            std::fs::write(
                &path,
                bytes.replace("/tmp/omp-project", "/tmp/omp-elsewhere"),
            )
            .unwrap();
            assert_ne!(
                OMP_LOG.identity(&path).unwrap(),
                read.identity,
                "the header record is the identity"
            );
        }
    }
}
