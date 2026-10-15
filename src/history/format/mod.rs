//! Transcript formats: turning a session file on disk into the normalized
//! [`LogEntry`] stream the rest of the application renders, searches and indexes.
//!
//! A format answers two questions about a file — *is this mine* and *what does it
//! say*. Where the file was found, how it is cached and how the session is resumed
//! belong to the [`SessionProvider`](super::provider::SessionProvider) instead.

pub mod claude;
pub mod codex;
pub mod dropped;
pub mod grok;
pub mod kimi;
mod lines;
pub mod opencode;
pub mod pi_log;
pub(crate) mod splice;

use super::provider::Capability;
use super::{Conversation, Source, TranscriptEntries, parser, provider};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::{AppError, Result};
use crate::log_entry::{LogEntry, SubagentIdentity};
use dropped::DroppedRecords;
use rayon::prelude::*;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// The session-level facts a transcript states about itself before its first
/// message.
#[derive(Clone, Debug)]
pub struct SessionHeader {
    #[allow(dead_code)]
    pub version: u64,
    pub id: String,
    pub timestamp: String,
    pub cwd: PathBuf,
    /// The label a sub-agent thread splices in under, when the format knows
    /// a better name than the id: the part of a Kimi id that differs from
    /// the session's, since the viewer shows a label's first characters, or
    /// the output id a Pi-family file is named after.
    pub thread_label: Option<String>,
    /// Who the thread is when it runs as a sub-agent, as its header records
    /// it; empty for a format that records none.
    pub subagent_identity: SubagentIdentity,
}

impl SessionHeader {
    /// The label a sub-agent thread splices in under: `thread_label`, or
    /// the id when the format set none.
    pub fn thread_label(&self) -> &str {
        self.thread_label.as_deref().unwrap_or(&self.id)
    }
}

/// The record an entry was read from: its line, and the transcript holding it
/// when that is not the session's own. A sub-agent's turn spliced into the
/// session comes from the sub-agent's transcript, and a record a Codex thread
/// inherits comes from the earlier rollout it continues. A line number alone
/// repeats across those transcripts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct RecordLine {
    /// `None` for the session's own transcript.
    pub(crate) transcript: Option<Arc<Path>>,
    pub(crate) line: usize,
}

impl From<usize> for RecordLine {
    /// A line of the session's own transcript.
    fn from(line: usize) -> Self {
        Self {
            transcript: None,
            line,
        }
    }
}

/// `entries`, read from one transcript, at their lines in it.
pub(crate) fn at_own_lines(entries: Vec<(usize, LogEntry)>) -> Vec<(RecordLine, LogEntry)> {
    entries
        .into_iter()
        .map(|(line, entry)| (line.into(), entry))
        .collect()
}

/// One transcript, normalized. Each entry keeps the file line it came from so
/// parse errors and viewer positions can still name a place in the original
/// file; an entry synthesized from outside the transcript — a title read from
/// a sidecar — carries line 0.
#[derive(Clone, Debug)]
pub struct SessionProjection {
    pub source: Source,
    pub header: SessionHeader,
    pub title: Option<String>,
    pub entries: Vec<(RecordLine, LogEntry)>,
    pub leaf_id: Option<String>,
    pub malformed_lines: Vec<usize>,
    /// The records the reader dropped, counted only when the parse was asked
    /// to count them.
    pub dropped: DroppedRecords,
    pub earlier_turns: EarlierTurns,
}

/// The outcome of reading the earlier transcripts a transcript continues.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EarlierTurns {
    /// Every earlier transcript was read, or the transcript continues none.
    Read,
    /// An earlier transcript is missing, compressed or permanently
    /// unreadable: the conversation lacks the turns before it.
    Missing,
    /// A transient error kept an earlier transcript from being read: the
    /// conversation lacks the turns before it until a read succeeds.
    TransientError,
}

impl EarlierTurns {
    /// True when the conversation lacks earlier turns: the row's
    /// `is_missing_earlier_turns`.
    pub(crate) fn is_missing(self) -> bool {
        self != Self::Read
    }

    /// True when a read may yet succeed: the row's `has_transient_read_error`.
    pub(crate) fn is_transient_error(self) -> bool {
        self == Self::TransientError
    }
}

pub trait SessionFormat: Sync {
    /// Parse `path`, or `None` when the file is not a transcript in this format.
    ///
    /// Not recognizing a file is an ordinary outcome rather than an error: session
    /// roots overlap, and a session directory override can hold another agent's
    /// transcripts.
    fn parse_transcript(&self, path: &Path) -> Result<Option<SessionProjection>> {
        self.parse_transcript_counting(path, DroppedRecords::default())
    }

    /// [`Self::parse_transcript`], counting into `dropped` the records the
    /// reader drops when `dropped` is counting; the projection carries them.
    fn parse_transcript_counting(
        &self,
        path: &Path,
        dropped: DroppedRecords,
    ) -> Result<Option<SessionProjection>>;

    /// The row the list shows for the transcript at `path`, its sub-agent
    /// transcripts not merged in, or `None` when the file is not a transcript
    /// in this format or holds no conversation.
    fn parse_conversation(
        &self,
        path: &Path,
        modified: Option<SystemTime>,
        debug_level: Option<DebugLevel>,
    ) -> Result<Option<Conversation>> {
        Ok(self.parse_transcript(path)?.and_then(|projection| {
            parser::conversation_from_projection(
                path.to_path_buf(),
                &projection,
                modified,
                debug_level,
            )
        }))
    }

    /// The entries the viewer and the agent CLI read for the session at
    /// `path`, with the sub-agent transcripts at `subagents` spliced in, or
    /// `None` when the file is not a transcript in this format.
    fn session_entries(
        &self,
        path: &Path,
        subagents: &[PathBuf],
    ) -> Result<Option<TranscriptEntries>> {
        Ok(view_projection(self, path, subagents)?.map(TranscriptEntries::of_projection))
    }
}

/// The view of a session: `path` parsed as `format`, with the sub-agent
/// transcripts at `subagents` parsed the same way and spliced into the entry
/// stream as `Progress` entries, the record Claude keeps for a sub-agent
/// turn. A session with sub-agent transcripts parses them alongside its own
/// on the rayon pool.
///
/// `subagents` come from the session's row.
pub fn view_projection(
    format: &(impl SessionFormat + ?Sized),
    path: &Path,
    subagents: &[PathBuf],
) -> Result<Option<SessionProjection>> {
    let (projection, threads) = if subagents.is_empty() {
        (format.parse_transcript(path), Vec::new())
    } else {
        rayon::join(
            || format.parse_transcript(path),
            || subagent_threads(format, subagents),
        )
    };
    Ok(projection?.map(|projection| spliced(projection, threads)))
}

fn spliced(
    mut projection: SessionProjection,
    threads: Vec<splice::SubagentThread>,
) -> SessionProjection {
    projection.entries = splice::splice_threads(projection.entries, threads);
    projection
}

/// Each sub-agent transcript at `subagents` that `format` reads, in order,
/// under its header's thread label. The view has no debug channel, so a read
/// failure leaves the thread out without a report; the load reports it when
/// the row is built.
fn subagent_threads(
    format: &(impl SessionFormat + ?Sized),
    subagents: &[PathBuf],
) -> Vec<splice::SubagentThread> {
    subagents
        .par_iter()
        .filter_map(|subagent| {
            let thread = format.parse_transcript(subagent).ok().flatten()?;
            Some(splice::SubagentThread::of_projection(subagent, thread))
        })
        .collect()
}

/// The warn-level report for a sub-agent transcript left out of its session
/// because it could not be read, for every provider's parse of one.
pub(crate) fn report_unreadable_subagent(
    debug_level: Option<DebugLevel>,
    source: Source,
    session_id: &str,
    subagent: &Path,
    error: &AppError,
) {
    debug::warn(
        debug_level,
        &format!(
            "Failed to parse {} sub-agent transcript {} of session {session_id}: {error}",
            source.list_label(),
            subagent.display(),
        ),
    );
}

/// [`SessionFormat::session_entries`] for a bare file (`--render`, a direct
/// path), read by the format of the agent its projection names, with the
/// sub-agent transcripts [`bare_file_subagents`] names. `None` when no
/// registered format recognizes the file.
///
/// That agent can differ from the first format to recognize the file: Pi's
/// format recognizes a log with an OMP title slot and names OMP.
pub fn sniffed_session_entries(path: &Path) -> Result<Option<TranscriptEntries>> {
    let Some(projection) = parse_transcript(path)? else {
        return Ok(None);
    };
    let source = projection.source;
    let subagents = bare_file_subagents(source, &projection.header.id, path);
    source.provider().format().session_entries(path, &subagents)
}

/// [`SessionFormat::parse_conversation`] for a bare file, read by the format
/// of the agent its projection names.
#[cfg(test)]
pub fn sniffed_conversation(
    path: &Path,
    modified: Option<SystemTime>,
    debug_level: Option<DebugLevel>,
) -> Result<Option<Conversation>> {
    let Some(projection) = parse_transcript(path)? else {
        return Ok(None);
    };
    projection
        .source
        .provider()
        .format()
        .parse_conversation(path, modified, debug_level)
}

/// The sub-agent transcripts of a bare file: [`stored_session_subagents`],
/// when the file is the session its agent stores under `session_id`. A copy
/// outside the agent's tree has none. Skips the stored check for an agent
/// declaring `subagents` Not Capable: the check can read every log's header.
fn bare_file_subagents(source: Source, session_id: &str, path: &Path) -> Vec<PathBuf> {
    if !source.provider().subagents().is_capable()
        || !provider::is_stored_session(source, session_id, path)
    {
        return Vec::new();
    }
    stored_session_subagents(source, path)
}

/// The sub-agent transcripts `source`'s agent names for the session it stores
/// at `path`; none for an agent declaring `subagents` Not Capable. A listing
/// that fails names none without a report: `--render` and `rearview <file>`
/// have no debug channel.
pub fn stored_session_subagents(source: Source, path: &Path) -> Vec<PathBuf> {
    match source.provider().subagents() {
        Capability::Capable(finder) => finder.subagent_transcripts(path).unwrap_or_default(),
        Capability::NotCapable(_) => Vec::new(),
    }
}

/// True when two paths name one file, however each was spelled. A locator
/// that is not a file compares as written.
pub(crate) fn same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// `path` parsed by the first registered format that recognizes it, or
/// `None` when no registered format does. Registration order settles files
/// more than one format can read; today that is the Pi-family log, which Pi
/// and OMP share.
pub fn parse_transcript(path: &Path) -> Result<Option<SessionProjection>> {
    for provider in provider::providers() {
        match provider.format().parse_transcript(path) {
            Ok(Some(projection)) => return Ok(Some(projection)),
            Ok(None) => {}
            Err(error) if is_not_a_file(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

/// Whether a format failed because `path` cannot be a file at all, rather
/// than because a file it opened misbehaved.
///
/// A provider whose sessions live in a container addresses them as locators
/// under the container file, and a user can hand such a locator to the scan —
/// `--show-path` prints them. A file-reading format that tries to open one
/// fails before reading a byte; that is "not mine", not a failure of the
/// scan. Errors from a path a format could open still propagate.
fn is_not_a_file(error: &AppError) -> bool {
    matches!(
        error,
        AppError::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            )
    )
}

/// Parse `path` as the format `source` owns, yielding `None` when the file is not
/// `source`'s transcript.
///
/// Ownership is stricter than "parses". Pi and OMP share one wire format, and a
/// transcript carrying no OMP title slot reads equally well as either — so asking
/// on behalf of a source both attributes the transcript to it and rejects files
/// that announce a different one.
///
/// A file that cannot be read is an error rather than a `None`, so that a caller
/// guarding a destructive operation cannot read "unreadable" as "not yours".
pub fn parse_owned_transcript(source: Source, path: &Path) -> Result<Option<SessionProjection>> {
    Ok(source
        .provider()
        .format()
        .parse_transcript(path)?
        .filter(|projection| projection.source == source))
}

/// The non-empty `text` fields of a content-block array, in order. Blocks
/// without one — encrypted reasoning, images — fall away here.
pub(crate) fn block_texts(content: Option<&Value>) -> Vec<String> {
    let Some(blocks) = content.and_then(Value::as_array) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `text` without the lines at either end that hold only whitespace. The lines
/// that remain keep their indentation, which a character-wise `trim` would take
/// off the first one. A `\r` closing the last remaining line is dropped with
/// its terminator; interior `\r` stay.
pub(crate) fn trim_blank_lines(text: &str) -> &str {
    let mut offset = 0;
    let mut content: Option<(usize, usize)> = None;
    for line in text.split_inclusive('\n') {
        let line_end = offset + line.len();
        let body = line.strip_suffix('\n').unwrap_or(line);
        let body = body.strip_suffix('\r').unwrap_or(body);
        if !body.trim().is_empty() {
            let start = content.map_or(offset, |(start, _)| start);
            content = Some((start, offset + body.len()));
        }
        offset = line_end;
    }
    content.map_or("", |(start, end)| &text[start..end])
}

/// Milliseconds since the epoch as RFC 3339 to the millisecond.
pub(crate) fn rfc3339_from_millis(millis: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(millis)
        .map(|timestamp| timestamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// Append `note` below what a command printed, on a line of its own. An empty
/// output leaves the note on the first line rather than under a blank one.
pub(crate) fn append_output_note(output: &mut String, note: &str) {
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(note);
}

/// Append the line a reader adds below the output of a command that failed.
/// Every reader of a command the user ran prints this one term, so a session
/// reads the same whichever agent recorded it.
pub(crate) fn append_exit_code(output: &mut String, code: i64) {
    append_output_note(output, &format!("Exit code: {code}"));
}

/// Move the value under `from` to `to`, for a key the provider names
/// differently from the canonical tool input. An absent `from` changes
/// nothing.
pub(crate) fn rename_key(arguments: &mut Map<String, Value>, from: &str, to: &str) {
    if let Some(value) = arguments.remove(from) {
        arguments.insert(to.to_owned(), value);
    }
}

/// Refuse `path` unless it is a transcript `source` owns, so one agent cannot
/// delete or rewrite another's session file.
///
/// Ownership is established by parsing alone — there is no cheap path-shape
/// pre-check, because what a locator looks like is the provider's own
/// business. A file that exists but cannot be read fails as the read error it
/// is, named with its path — never as a missing session.
pub fn require_owned_transcript(source: Source, path: &Path) -> Result<()> {
    let not_found = || AppError::SessionNotFound(path.display().to_string());
    match parse_owned_transcript(source, path) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(not_found()),
        Err(AppError::Io(error)) => Err(AppError::Io(std::io::Error::new(
            error.kind(),
            format!("{}: {error}", path.display()),
        ))),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pi")
            .join(name)
    }

    fn owns(source: Source, path: &Path) -> bool {
        parse_owned_transcript(source, path).unwrap().is_some()
    }

    #[test]
    fn milliseconds_since_the_epoch_read_as_rfc3339_to_the_millisecond() {
        assert_eq!(
            rfc3339_from_millis(1_777_000_000_123).as_deref(),
            Some("2026-04-24T03:06:40.123Z")
        );
    }

    #[test]
    fn milliseconds_past_the_calendars_range_dont_have_a_timestamp() {
        assert_eq!(rfc3339_from_millis(i64::MAX), None);
    }

    #[test]
    fn a_transcript_without_a_title_slot_belongs_to_whichever_source_asks() {
        let path = fixture("v3-branched.jsonl");
        assert_eq!(
            parse_owned_transcript(Source::Pi, &path)
                .unwrap()
                .map(|projection| projection.source),
            Some(Source::Pi)
        );
        assert_eq!(
            parse_owned_transcript(Source::Omp, &path)
                .unwrap()
                .map(|projection| projection.source),
            Some(Source::Omp)
        );
    }

    #[test]
    fn an_omp_title_slot_keeps_pi_from_claiming_the_transcript() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("titled.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"title","v":1,"title":"Named"}"#,
                "\n",
                r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
                "\n",
            ),
        )
        .unwrap();

        assert!(!owns(Source::Pi, &path));
        assert!(owns(Source::Omp, &path));
        assert_eq!(
            parse_transcript(&path).unwrap().map(|proj| proj.source),
            Some(Source::Omp),
            "a self-identifying transcript keeps its own source whichever format reads it first"
        );
    }

    #[test]
    fn a_claude_transcript_belongs_to_claude_alone() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claude.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"user","message":{"role":"user","content":"a question"}}"#,
        )
        .unwrap();

        assert_eq!(
            parse_transcript(&path).unwrap().map(|proj| proj.source),
            Some(Source::Claude)
        );
        assert!(owns(Source::Claude, &path));
        assert!(!owns(Source::Pi, &path));
        assert!(!owns(Source::Omp, &path));
    }

    /// A record that is not a whole Claude record is recognized by no
    /// format.
    #[test]
    fn a_file_no_registered_format_recognizes_is_unrecognized() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unknown.jsonl");
        std::fs::write(&path, "{\"type\":\"user\"}\n").unwrap();

        assert!(parse_transcript(&path).unwrap().is_none());
        assert!(!owns(Source::Claude, &path));
    }

    #[test]
    fn a_file_that_is_not_a_transcript_is_refused_as_a_missing_session() {
        let directory = tempfile::tempdir().unwrap();
        let notes = directory.path().join("notes.txt");
        let claude = directory.path().join("claude.jsonl");
        std::fs::write(&notes, "keep").unwrap();
        std::fs::write(&claude, "{\"type\":\"user\"}\n").unwrap();

        for path in [&notes, &claude] {
            assert!(
                matches!(
                    require_owned_transcript(Source::Pi, path),
                    Err(AppError::SessionNotFound(_))
                ),
                "{} is not a Pi transcript",
                path.display()
            );
        }
    }

    /// A locator names a session under a container file; no file-reading
    /// format can open one, and the scan must read that as "claimed by
    /// nobody", not as a failure.
    #[test]
    fn a_path_under_a_file_fails_no_scan_and_is_claimed_by_nobody() {
        let directory = tempfile::tempdir().unwrap();
        let container = directory.path().join("container.db");
        std::fs::write(&container, "opaque").unwrap();

        assert!(
            parse_transcript(&container.join("inside.jsonl"))
                .unwrap()
                .is_none()
        );
        assert!(
            parse_transcript(&directory.path().join("absent.jsonl"))
                .unwrap()
                .is_none(),
            "a path that plainly does not exist is claimed by nobody either"
        );
    }

    /// With a format registered whose sessions live in a container, the scan
    /// passes over the file-reading formats ahead of it and lets it claim
    /// its locator.
    #[test]
    fn the_scan_reaches_a_format_whose_paths_are_not_files() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("opencode.db");
        opencode::fixture::create_database(&database);
        let connection = rusqlite::Connection::open(&database).unwrap();
        opencode::fixture::standard_session(&connection, "ses_scanned");
        drop(connection);

        let projection = parse_transcript(&database.join("ses_scanned.jsonl"))
            .unwrap()
            .expect("the OpenCode format claims its locator");
        assert_eq!(projection.source, Source::OpenCode);
    }

    /// A directory cannot be read as a transcript, so the guard has to choose
    /// between the two failures it is allowed to report.
    #[test]
    fn an_unreadable_transcript_is_refused_as_a_read_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unreadable.jsonl");
        std::fs::create_dir(&path).unwrap();

        let error = require_owned_transcript(Source::Pi, &path).unwrap_err();

        assert!(
            matches!(error, AppError::Io(_)),
            "a transcript that cannot be read must not be reported as absent: {error}"
        );
        assert!(
            error.to_string().contains("unreadable.jsonl"),
            "a refusal must name the file it refused: {error}"
        );
    }

    #[test]
    fn trim_blank_lines_drops_the_blank_lines_at_each_end_and_keeps_indentation() {
        let cases = [
            ("\n    Directory: C:\\x\n", "    Directory: C:\\x"),
            ("   \n\nout\n  \n", "out"),
            ("  indented\n  more", "  indented\n  more"),
            ("\n\n", ""),
            ("first\n   \nlast", "first\n   \nlast"),
            ("a\r\nb\r\n", "a\r\nb"),
            ("a\r\nb\r", "a\r\nb"),
        ];
        for (input, expected) in cases {
            assert_eq!(trim_blank_lines(input), expected, "input {input:?}");
        }
    }
}
