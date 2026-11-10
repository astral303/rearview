//! Resumable transcript reading: what a read of one transcript leaves for the
//! next read to resume from, and the reader each agent's storage supplies.
//! The session cache stores a [`TranscriptRead`] per transcript and never
//! interprets its values; only the reader that wrote them does.

use crate::error::Result;
use crate::history::format::RecordLine;
use crate::history::{MalformedLine, ParsePermit};
use crate::log_entry::LogEntry;
use std::path::Path;

/// What a read of one transcript leaves for the next, as the agent's reader
/// encodes it. Every value is opaque to the session cache.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptRead {
    /// Where the next read continues: past the last record read whole.
    pub cursor: Vec<u8>,
    /// Where the next read restarts deriving entries; never after `cursor`.
    pub stable_cursor: Vec<u8>,
    /// A hash of what the agent never changes in the transcript.
    pub identity: Vec<u8>,
    /// The reader's check value for the content before `cursor`.
    pub rewrite_check: Vec<u8>,
    /// The reader's state at `stable_cursor`.
    pub parser_state: Vec<u8>,
    /// The line `stable_cursor` stands at: the next read derives the
    /// transcript's entries again from this line on. `None` for a read the
    /// next one cannot resume.
    pub stable_line: Option<usize>,
}

impl TranscriptRead {
    /// The values a read through [`FullRead`] leaves: empty, so no reader
    /// resumes from them and the next read is a full read.
    pub fn full_read() -> Self {
        Self {
            cursor: Vec::new(),
            stable_cursor: Vec::new(),
            identity: Vec::new(),
            rewrite_check: Vec::new(),
            parser_state: Vec::new(),
            stable_line: None,
        }
    }
}

/// A transcript read from a stored [`TranscriptRead`]'s stable cursor to its
/// end.
#[derive(Debug)]
pub struct ResumedWindow {
    /// The line the stable cursor stood at. The window's entries replace the
    /// transcript's entries from this line on.
    pub start_line: usize,
    pub entries: Vec<(RecordLine, LogEntry)>,
    pub malformed_lines: Vec<MalformedLine>,
    /// What this read leaves for the next.
    pub read: TranscriptRead,
    /// The bytes read from the stable cursor on.
    pub bytes_read: u64,
}

/// The outcome of resuming a read of one transcript.
#[derive(Debug)]
pub enum Resumption {
    Window(ResumedWindow),
    /// The transcript cannot resume from the stored read: read it whole.
    FullRead,
    /// A record from the stable cursor on changes an entry before it, such
    /// as a hook's message naming a call read before it: read the
    /// transcript whole.
    ChangesEarlierEntry,
}

/// Reads one agent's transcripts from where an earlier read stopped. Each
/// agent's storage supplies its reader; [`FullRead`] is the default.
pub trait TranscriptReader: Sync {
    /// A hash of what the agent never changes in the transcript at `source`;
    /// empty when the reader keeps no identity.
    fn identity(&self, source: &Path) -> Result<Vec<u8>>;

    /// True when the content of `source` before `read`'s cursor may have
    /// changed since `read`.
    fn is_rewritten(&self, source: &Path, read: &TranscriptRead) -> Result<bool>;

    /// True when `source` holds what `read` read. A resumed read asks only
    /// for a source the file system cannot stamp, such as a database's
    /// session; it compares a file's stamps instead. The default: false,
    /// so the source is read.
    fn is_unchanged_since(&self, _source: &Path, _read: &TranscriptRead) -> Result<bool> {
        Ok(false)
    }

    /// The transcript at `source` from `read`'s stable cursor to its end,
    /// or [`Resumption::FullRead`] when it cannot resume from `read`. Call it
    /// once the identity matches and the content before the cursor is
    /// unchanged, as [`resume`] does. `permit` admits the parse.
    fn read_from(
        &self,
        permit: ParsePermit<'_>,
        source: &Path,
        read: &TranscriptRead,
    ) -> Result<Resumption>;

    /// The transcript at `source` read whole, as a session's sub-agent
    /// transcript; `None` when it is not one of the agent's transcripts.
    fn read_whole(
        &self,
        permit: ParsePermit<'_>,
        source: &Path,
    ) -> Result<Option<crate::history::format::SessionProjection>>;

    /// The transcript at `source`'s leading entries: those at line 0, which
    /// the agent keeps outside the transcript, such as a title, and every
    /// read derives again. `read` is what the transcript's latest read
    /// leaves for the next.
    fn leading_entries(
        &self,
        source: &Path,
        read: &TranscriptRead,
    ) -> Result<Vec<(RecordLine, LogEntry)>>;
}

/// The reader of a storage without a resumable reader: every changed
/// transcript is read whole.
pub struct FullRead;

impl TranscriptReader for FullRead {
    fn identity(&self, _source: &Path) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }

    fn is_rewritten(&self, _source: &Path, _read: &TranscriptRead) -> Result<bool> {
        Ok(true)
    }

    fn read_from(
        &self,
        _permit: ParsePermit<'_>,
        _source: &Path,
        _read: &TranscriptRead,
    ) -> Result<Resumption> {
        Ok(Resumption::FullRead)
    }

    fn read_whole(
        &self,
        _permit: ParsePermit<'_>,
        _source: &Path,
    ) -> Result<Option<crate::history::format::SessionProjection>> {
        Ok(None)
    }

    fn leading_entries(
        &self,
        _source: &Path,
        _read: &TranscriptRead,
    ) -> Result<Vec<(RecordLine, LogEntry)>> {
        Ok(Vec::new())
    }
}

/// One sub-agent thread of a session, as its thread linker reads it.
pub struct LinkedThread<'a> {
    pub transcript: &'a Path,
    pub label: &'a str,
    pub identity: &'a crate::log_entry::SubagentIdentity,
    pub started: &'a str,
}

/// One transcript's entries a resumed read derived again: from
/// `start_line` on, they replace what the ledger holds of that transcript.
pub struct LinkWindow<'a> {
    /// `None` for the session's own transcript.
    pub transcript: Option<&'a Path>,
    pub start_line: usize,
    pub entries: &'a [(RecordLine, LogEntry)],
}

/// What a resumed read changes in how a session's threads link.
pub struct LinkUpdate {
    pub ledger: Vec<u8>,
    /// The call that launched each thread, in the order of the threads the
    /// update was given.
    pub launch_calls: Vec<Option<String>>,
    /// The threads whose entries the update changes outside their windows,
    /// such as a reply a new report folds: each is read whole and folded
    /// with [`ThreadLinker::fold`].
    pub rereads: Vec<std::path::PathBuf>,
}

/// Links a session's sub-agent threads to each other and to the session,
/// keeping a ledger of what links them so a resumed read relinks from its
/// windows alone. Each agent's storage supplies its linker; [`NoLinks`] is
/// the default.
pub trait ThreadLinker: Sync {
    /// Link `threads`, read whole, given the session's own entries, and
    /// return the ledger.
    fn link(
        &self,
        session: &[(RecordLine, LogEntry)],
        threads: &mut [crate::history::format::splice::SubagentThread],
    ) -> Vec<u8>;

    /// The links once `windows` replace what `ledger` holds of their
    /// transcripts, for a session whose threads are `threads`.
    fn update(
        &self,
        ledger: &[u8],
        windows: &[LinkWindow<'_>],
        threads: &[LinkedThread<'_>],
    ) -> LinkUpdate;

    /// Rewrite `thread`, read whole, as `ledger` links it among `threads`.
    fn fold(
        &self,
        ledger: &[u8],
        thread: &mut crate::history::format::splice::SubagentThread,
        threads: &[LinkedThread<'_>],
    );

    /// `ledger` with each transcript it names renamed by `rename`; `None`
    /// when `rename` cannot rename one of them. The cache stores a ledger
    /// with its transcripts named below the session's roots, and reads it
    /// back in this run's spelling of them.
    fn with_transcripts_renamed(
        &self,
        ledger: &[u8],
        rename: &dyn Fn(&Path) -> Option<std::path::PathBuf>,
    ) -> Option<Vec<u8>>;
}

/// The linker of a storage whose threads link to nothing.
pub struct NoLinks;

impl ThreadLinker for NoLinks {
    fn link(
        &self,
        _session: &[(RecordLine, LogEntry)],
        _threads: &mut [crate::history::format::splice::SubagentThread],
    ) -> Vec<u8> {
        Vec::new()
    }

    fn update(
        &self,
        ledger: &[u8],
        _windows: &[LinkWindow<'_>],
        threads: &[LinkedThread<'_>],
    ) -> LinkUpdate {
        LinkUpdate {
            ledger: ledger.to_vec(),
            launch_calls: vec![None; threads.len()],
            rereads: Vec::new(),
        }
    }

    fn fold(
        &self,
        _ledger: &[u8],
        _thread: &mut crate::history::format::splice::SubagentThread,
        _threads: &[LinkedThread<'_>],
    ) {
    }

    fn with_transcripts_renamed(
        &self,
        ledger: &[u8],
        _rename: &dyn Fn(&Path) -> Option<std::path::PathBuf>,
    ) -> Option<Vec<u8>> {
        Some(ledger.to_vec())
    }
}

/// The transcript at `source`, changed since `stored`, read on from
/// `stored` through `reader`: a full read when its identity differs or the
/// content before the cursor changed. `permit` admits the parse.
pub fn resume(
    reader: &dyn TranscriptReader,
    permit: ParsePermit<'_>,
    source: &Path,
    stored: &TranscriptRead,
) -> Result<Resumption> {
    if reader.identity(source)? != stored.identity || reader.is_rewritten(source, stored)? {
        return Ok(Resumption::FullRead);
    }
    reader.read_from(permit, source, stored)
}
