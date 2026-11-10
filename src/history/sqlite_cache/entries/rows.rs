//! A session's entry rows as a write stores them, built in memory before
//! any is written: its transcripts, threads, messages and entries, each
//! entry with its search texts and digest. A first write inserts them all;
//! a later write compares them with the stored rows and writes the
//! differences.

use super::digest::Digest;
use super::messages::{MessageRow, Messages};
use super::search::{self, EntrySearch};
use super::threads::{Numbering, ThreadRow};
use super::{EntryColumns, parts};
use crate::agent::message_numbering::NumberingCheckpoint;
use crate::history::TranscriptEntries;
use crate::history::format::RecordLine;
use crate::log_entry::{LogEntry, TokenUsage};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

pub(in super::super) struct SessionRows<'a> {
    pub(super) transcripts: Vec<Arc<Path>>,
    pub(super) threads: Vec<ThreadRow>,
    pub(super) messages: Vec<MessageRow<'a>>,
    /// In the session's entry order: an entry's index after
    /// `first_sequence` is its `sequence`.
    pub(super) entries: Vec<EntryRow<'a>>,
    pub(super) first_sequence: usize,
}

pub(super) struct EntryRow<'a> {
    pub(super) record: &'a RecordLine,
    pub(super) columns: EntryColumns<'a>,
    pub(super) search: EntrySearch,
    /// The index of the entry's message in [`SessionRows::messages`].
    pub(super) message: Option<usize>,
    pub(super) contributes: bool,
    pub(super) position: i64,
    /// The index of the entry's transcript in [`SessionRows::transcripts`]
    /// when its thread's origin names another one.
    pub(super) transcript: Option<usize>,
    /// The timestamp text the splice orders the entry by.
    pub(super) splice_key: Option<&'a str>,
    /// The index in [`SessionRows::transcripts`] of the sub-agent
    /// transcript a spliced turn came from.
    pub(super) child: Option<usize>,
    pub(super) digest: i64,
}

impl EntryRow<'_> {
    /// The usage the entry row stores: an entry outside every message keeps
    /// its own; a message stores its entries' usage once.
    pub(super) fn metadata_usage(&self) -> Option<&TokenUsage> {
        self.message
            .is_none()
            .then_some(self.columns.usage)
            .flatten()
    }

    /// A hash of the columns the row stores besides its place, with its
    /// parts, result items and search texts.
    fn digest(&self) -> i64 {
        let columns = &self.columns;
        let mut digest = Digest::default();
        digest.number(Some(self.record.line as i64));
        for text in [
            Some(columns.kind),
            columns.content,
            columns.text,
            columns.label,
            columns.uuid,
            columns.level,
            columns.parent_uuid,
            self.splice_key,
        ] {
            digest.text(text);
        }
        digest.number(columns.timestamp_ms);
        digest.number(columns.duration_ms.map(|duration| duration as i64));
        digest.flag(columns.flag);
        digest.usage(self.metadata_usage());
        parts::digest(&mut digest, columns.parts.blocks(), &self.search.blocks);
        digest.number(Some(self.search.derived.len() as i64));
        for derived in &self.search.derived {
            digest.number(Some(derived.place.field.code()));
            digest.number(Some(derived.place.position));
            digest.text(Some(&derived.text));
        }
        digest.finish()
    }
}

/// Where a session's rows start: the numbering of the entries before them,
/// and the sequence of their first entry.
#[derive(Default)]
pub(in super::super) struct RowsStart {
    pub(in super::super) numbering: NumberingCheckpoint,
    /// The next position in each thread, by thread number.
    pub(in super::super) positions: HashMap<i64, i64>,
    pub(in super::super) sequence: usize,
}

impl<'a> SessionRows<'a> {
    /// The rows of `entries`, the entries of a session whose sub-agent
    /// transcripts are `subagents`.
    pub(in super::super) fn of(entries: &'a TranscriptEntries, subagents: &HashSet<&Path>) -> Self {
        Self::from(&entries.entries, subagents, &RowsStart::default())
    }

    /// The rows of `entries`, a session's entries from `start` on.
    pub(in super::super) fn from(
        entries: &'a [(RecordLine, LogEntry)],
        subagents: &HashSet<&Path>,
        start: &RowsStart,
    ) -> Self {
        let mut numbering = Numbering::resumed(&start.numbering, &start.positions);
        let mut messages = Messages::resumed(&start.numbering);
        let mut rows = Vec::with_capacity(entries.len());
        for (index, (record, entry)) in entries.iter().enumerate() {
            let placement = numbering.place(record, entry);
            let columns = EntryColumns::of(entry);
            let membership = messages.add(index, &placement, entry, &columns);
            let child = record
                .transcript
                .as_deref()
                .filter(|path| subagents.contains(path))
                .and(placement.record_transcript);
            let splice_key = match (child, entry) {
                (Some(_), LogEntry::Progress { extra, .. }) => Some(
                    extra
                        .get("timestamp")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default(),
                ),
                _ => entry.timestamp(),
            };
            let search = search::entry_search(entry, columns.parts.blocks(), child.is_some());
            let mut row = EntryRow {
                record,
                columns,
                search,
                message: membership.map(|membership| membership.message),
                contributes: membership.is_some_and(|membership| membership.contributes),
                position: placement.position,
                transcript: placement.transcript,
                splice_key,
                child,
                digest: 0,
            };
            row.digest = row.digest();
            rows.push(row);
        }
        Self {
            transcripts: numbering.transcripts,
            threads: numbering.threads,
            messages: messages.rows,
            entries: rows,
            first_sequence: start.sequence,
        }
    }

    /// The sequence of the entry at `index`.
    pub(super) fn sequence_of(&self, index: usize) -> usize {
        self.first_sequence + index
    }

    /// The number of messages that take a printed number.
    pub(in super::super) fn message_count(&self) -> i64 {
        self.messages
            .iter()
            .filter(|message| message.ordinal.is_some())
            .count() as i64
    }
}
