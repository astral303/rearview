//! Transcript formats: turning a session file on disk into the normalized
//! [`LogEntry`] stream the rest of the application renders, searches and indexes.
//!
//! A format answers two questions about a file — *is this mine* and *what does it
//! say*. Where the file was found, how it is cached and how the session is resumed
//! belong to the [`SessionProvider`](super::provider::SessionProvider) instead.

pub mod pi_log;

use super::{Source, provider};
use crate::claude::LogEntry;
use crate::error::Result;
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
pub fn parse_owned_transcript(source: Source, path: &Path) -> Option<SessionProjection> {
    source
        .provider()
        .format()?
        .parse_transcript(path)
        .ok()
        .flatten()
        .filter(|projection| projection.source == source)
}

/// Whether `path` holds a transcript `source` owns. Guards destructive operations
/// so one agent cannot delete or rewrite another's session file.
pub fn owns_transcript(source: Source, path: &Path) -> bool {
    parse_owned_transcript(source, path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pi")
            .join(name)
    }

    #[test]
    fn a_transcript_without_a_title_slot_belongs_to_whichever_source_asks() {
        let path = fixture("v3-branched.jsonl");
        assert_eq!(
            parse_owned_transcript(Source::Pi, &path).map(|projection| projection.source),
            Some(Source::Pi)
        );
        assert_eq!(
            parse_owned_transcript(Source::Omp, &path).map(|projection| projection.source),
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

        assert!(!owns_transcript(Source::Pi, &path));
        assert!(owns_transcript(Source::Omp, &path));
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
        assert!(!owns_transcript(Source::Pi, &path));
        assert!(!owns_transcript(Source::Omp, &path));
        assert!(!owns_transcript(Source::Claude, &path));
    }
}
