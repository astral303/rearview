//! Transcript formats: turning a session file on disk into the normalized
//! [`LogEntry`] stream the rest of the application renders, searches and indexes.
//!
//! A format answers two questions about a file — *is this mine* and *what does it
//! say*. Where the file was found, how it is cached and how the session is resumed
//! belong to the [`SessionProvider`](super::provider::SessionProvider) instead.

pub mod pi_log;

use super::{Source, provider};
use crate::claude::LogEntry;
use crate::error::{AppError, Result};
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
}

/// One transcript, normalized. Each entry keeps the file line it came from so
/// parse errors and viewer positions can still name a place in the original file.
#[derive(Clone, Debug)]
pub struct SessionProjection {
    pub source: Source,
    /// The session this one is a sub-agent thread of, when the agent records that
    /// relationship between separate transcript files.
    pub parent_session_id: Option<String>,
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

/// The first registered format that recognizes `path`.
///
/// Used where the caller has only a file — the viewer, export, and the Claude
/// loader's fallback. Registration order settles files more than one format can
/// read; today that is the Pi-family log, which Pi and OMP share.
pub fn parse_transcript(path: &Path) -> Result<Option<SessionProjection>> {
    for provider in provider::providers() {
        let Some(format) = provider.format() else {
            continue;
        };
        if let Some(projection) = format.parse_transcript(path)? {
            return Ok(Some(projection));
        }
    }
    Ok(None)
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

/// Refuse `path` unless it is a transcript `source` owns, so one agent cannot
/// delete or rewrite another's session file.
///
/// A file that exists but cannot be read fails as the read error it is, named
/// with its path — never as a missing session.
pub fn require_owned_transcript(source: Source, path: &Path) -> Result<()> {
    let not_found = || AppError::SessionNotFound(path.display().to_string());
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
        return Err(not_found());
    }
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
