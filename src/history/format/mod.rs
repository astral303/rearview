//! Transcript formats: turning a session file on disk into the normalized
//! [`LogEntry`] stream the rest of the application renders, searches and indexes.
//!
//! A format answers two questions about a file — *is this mine* and *what does it
//! say*. Where the file was found, how it is cached and how the session is resumed
//! belong to the [`SessionProvider`](super::provider::SessionProvider) instead.

pub mod codex;
pub mod kimi;
pub mod opencode;
pub mod pi_log;
mod splice;

use super::provider::SessionProvider;
use super::{Source, provider};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::{AppError, Result};
use crate::log_entry::LogEntry;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// The session-level facts a transcript states about itself before its first
/// message.
#[derive(Clone, Debug)]
pub struct SessionHeader {
    #[allow(dead_code)]
    pub version: u64,
    pub id: String,
    pub timestamp: String,
    pub cwd: PathBuf,
    /// The label a sub-agent thread splices in under, when it is not the
    /// id. The viewer shows a label's first characters, so a format whose
    /// thread ids share a prefix names the part that differs.
    pub thread_label: Option<String>,
}

impl SessionHeader {
    /// The label a sub-agent thread splices in under: `thread_label`, or
    /// the id when the format set none.
    pub fn thread_label(&self) -> &str {
        self.thread_label.as_deref().unwrap_or(&self.id)
    }
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
    pub entries: Vec<(usize, LogEntry)>,
    pub leaf_id: Option<String>,
    pub malformed_lines: Vec<usize>,
}

pub trait SessionFormat: Sync {
    /// Parse `path`, or `None` when the file is not a transcript in this format.
    ///
    /// Not recognizing a file is an ordinary outcome rather than an error: session
    /// roots overlap, and a redirected session directory can hold another agent's
    /// transcripts.
    fn parse_transcript(&self, path: &Path) -> Result<Option<SessionProjection>>;
}

/// The view of a session: `path` parsed as `format`, with the sub-agent
/// transcripts at `subagents` parsed the same way and spliced into the entry
/// stream as `Progress` entries — the shape Claude records natively inside
/// the parent file.
///
/// `subagents` come from the session's row.
pub fn view_projection(
    format: &dyn SessionFormat,
    path: &Path,
    subagents: &[PathBuf],
) -> Result<Option<SessionProjection>> {
    let Some(projection) = format.parse_transcript(path)? else {
        return Ok(None);
    };
    Ok(Some(splice_subagents(format, projection, subagents)))
}

/// [`view_projection`] for a session already parsed. Each thread splices in
/// under its header's thread label. The view has no debug channel, so a
/// read failure is not reported here; the load reports it when the row is
/// built.
pub fn splice_subagents(
    format: &dyn SessionFormat,
    mut projection: SessionProjection,
    subagents: &[PathBuf],
) -> SessionProjection {
    let mut threads = Vec::new();
    for subagent in subagents {
        if let Some(thread) = subagent_projection(
            format,
            subagent,
            projection.source,
            &projection.header.id,
            None,
        ) {
            threads.push((thread.header.thread_label().to_owned(), thread));
        }
    }
    projection.entries =
        splice::splice_by_timestamp(projection.entries, splice::progress_entries(threads));
    projection
}

/// `subagent` parsed as `format`, or `None` when the format does not
/// recognize it or cannot read it.
///
/// A read failure leaves the thread out of the session `session_id` names
/// rather than failing the session: the session's own transcript still reads,
/// and failing it with the thread would delist it until the thread changed on
/// disk. The failure is reported at warn level, as an unreadable session is.
pub(crate) fn subagent_projection(
    format: &dyn SessionFormat,
    subagent: &Path,
    source: Source,
    session_id: &str,
    debug_level: Option<DebugLevel>,
) -> Option<SessionProjection> {
    match format.parse_transcript(subagent) {
        Ok(projection) => projection,
        Err(error) => {
            debug::warn(
                debug_level,
                &format!(
                    "Failed to parse {} sub-agent transcript {} of session {session_id}: {error}",
                    source.list_label(),
                    subagent.display(),
                ),
            );
            None
        }
    }
}

/// [`view_projection`] for a bare file nothing has attributed (`--render`, a
/// direct path), with the sub-agent transcripts [`bare_file_subagents`]
/// names.
pub fn sniffed_view_projection(path: &Path) -> Result<Option<SessionProjection>> {
    let Some(Sniffed {
        provider,
        format,
        projection,
    }) = sniff(path)?
    else {
        return Ok(None);
    };
    let subagents = bare_file_subagents(provider.source(), &projection.header.id, path);
    Ok(Some(splice_subagents(format, projection, &subagents)))
}

/// The sub-agent transcripts of a bare file: the ones `source`'s session-id
/// lookup names for `session_id`, when the file is the session it lists
/// under that id. A copy outside the agent's tree has none.
pub fn bare_file_subagents(source: Source, session_id: &str, path: &Path) -> Vec<PathBuf> {
    source
        .provider()
        .resolve_session_id(session_id)
        .ok()
        .flatten()
        .filter(|resolved| same_file(&resolved.stub.locator, path))
        .map(|resolved| resolved.stub.subagents)
        .unwrap_or_default()
}

/// True when two paths name one file, however each was spelled. A locator
/// that is not a file compares as written.
fn same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// `path` parsed by the first registered format that recognizes it, or
/// `None` when no registered format does.
pub fn parse_transcript(path: &Path) -> Result<Option<SessionProjection>> {
    Ok(sniff(path)?.map(|sniffed| sniffed.projection))
}

/// A bare file parsed by the first registered format that recognized it,
/// with the provider that format belongs to.
struct Sniffed {
    provider: &'static dyn SessionProvider,
    format: &'static dyn SessionFormat,
    projection: SessionProjection,
}

/// `None` when no registered format recognizes `path`. Registration order
/// settles files more than one format can read; today that is the Pi-family
/// log, which Pi and OMP share.
fn sniff(path: &Path) -> Result<Option<Sniffed>> {
    for provider in provider::providers() {
        let Some(format) = provider.format() else {
            continue;
        };
        match format.parse_transcript(path) {
            Ok(Some(projection)) => {
                return Ok(Some(Sniffed {
                    provider: *provider,
                    format,
                    projection,
                }));
            }
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
    let Some(format) = source.provider().format() else {
        return Ok(None);
    };
    Ok(format
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
    fn a_claude_transcript_belongs_to_no_registered_format() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claude.jsonl");
        std::fs::write(&path, "{\"type\":\"user\"}\n").unwrap();

        assert!(parse_transcript(&path).unwrap().is_none());
        assert!(!owns(Source::Pi, &path));
        assert!(!owns(Source::Omp, &path));
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
}
