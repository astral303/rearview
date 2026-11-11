//! One session's read, from the loader to the list or command that asked for
//! it: which file, the transcript revision discovery found, what the read
//! found, and what the TUI does next with the session.

use super::load::ReadError;
use super::storage::SessionStub;
use crate::agent::diagnostic::AgentWarning;
use crate::history::fingerprint::CachedFingerprint;
use crate::history::sqlite_cache::NextStep;
use crate::history::{Conversation, TranscriptEntries};
use std::path::{Path, PathBuf};

/// The result of reading one session.
pub enum SessionRead {
    /// The row the list shows for the session.
    Listed(Box<Conversation>),
    /// Read cleanly, though the file doesn't hold any conversation the
    /// provider lists. Carries the warning agent commands print for it,
    /// built from the parse or as the cache recorded it (`None` for a row
    /// recorded without one).
    Empty(Option<AgentWarning>),
    /// Neither discovery nor a lookup of the file finds a session there.
    Missing,
    /// Could not be read, with the warning agent commands print for it, as
    /// for an empty session; `None` too when a failed discovery or lookup
    /// kept the session from being read.
    Failed(ReadError, Option<AgentWarning>),
    /// Missing from the session cache, and nothing parsed the transcript: a
    /// read of the cache alone, as the TUI's drawing thread reads it.
    NotCached,
}

/// One session's read, by the provider the caller asked, and what the TUI
/// does next with the session.
#[must_use = "a session outcome carries the TUI's next step for the session"]
pub struct SessionOutcome {
    /// The file the read was asked for.
    locator: PathBuf,
    /// The transcripts' size and modification time discovery found; `None`
    /// for a session discovery did not find, or found without them.
    revision: Option<CachedFingerprint>,
    read: SessionRead,
    next: NextStep,
    unwritten: UnwrittenParse,
}

/// The findings of a session's whole parse missing from its copy in the
/// cache.
pub enum UnwrittenParse {
    /// The copy holds what the read found, the read parsed nothing, or the
    /// read resumed from the stored copy: a resumed read whose write fails
    /// reads as that copy, older than the transcript.
    Nothing,
    /// The parse's entries: the write did not land, no record could be
    /// built, a file changed after its stamp, or the session doesn't hold
    /// any conversation the provider lists.
    Entries(TranscriptEntries),
    /// The parse found the file is not a transcript in the agent's format.
    /// The read's warning reports it.
    Rejected,
}

impl SessionOutcome {
    /// `read`, the read of the session at `stub`, settled.
    pub(super) fn settled(stub: &SessionStub, read: SessionRead) -> Self {
        Self {
            locator: stub.locator.clone(),
            revision: stub.fingerprint.stamp(),
            read,
            next: NextStep::Settled,
            unwritten: UnwrittenParse::Nothing,
        }
    }

    /// `read`, the read of the session at `stub`, settled. The copy in the
    /// cache lacks `unwritten`, what the parse found.
    pub(super) fn settled_with_unwritten(
        stub: &SessionStub,
        read: SessionRead,
        unwritten: UnwrittenParse,
    ) -> Self {
        Self {
            unwritten,
            ..Self::settled(stub, read)
        }
    }

    /// No session at `locator`.
    pub(super) fn missing(locator: &Path) -> Self {
        Self::unread(locator, SessionRead::Missing)
    }

    /// The session at `locator`, unread because discovery or a lookup failed.
    /// It may still be there, unchanged.
    pub(super) fn failed_transiently(locator: &Path) -> Self {
        Self::unread(locator, SessionRead::Failed(ReadError::Transient, None))
    }

    /// The session at `stub`, missing from the session cache.
    pub(super) fn not_cached(stub: &SessionStub) -> Self {
        Self {
            revision: stub.fingerprint.stamp(),
            ..Self::unread(&stub.locator, SessionRead::NotCached)
        }
    }

    fn unread(locator: &Path, read: SessionRead) -> Self {
        Self {
            locator: locator.to_path_buf(),
            revision: None,
            read,
            next: NextStep::Settled,
            unwritten: UnwrittenParse::Nothing,
        }
    }

    /// This outcome, with `next` as its next step.
    pub(super) fn with_next_step(mut self, next: NextStep) -> Self {
        self.next = next;
        self
    }

    /// This outcome as the answer for `locator`, the caller's spelling of
    /// the file it was read from.
    pub(super) fn answering(mut self, locator: &Path) -> Self {
        self.locator = locator.to_path_buf();
        self
    }

    /// [`Self::settled`], for a test that stands in for the loader.
    #[cfg(test)]
    pub(crate) fn settled_for_test(stub: &SessionStub, read: SessionRead) -> Self {
        Self::settled(stub, read)
    }

    pub fn locator(&self) -> &Path {
        &self.locator
    }

    pub fn revision(&self) -> Option<CachedFingerprint> {
        self.revision
    }

    /// True when the read found a row the list shows.
    pub fn is_listed(&self) -> bool {
        matches!(self.read, SessionRead::Listed(_))
    }

    /// The row the list shows for the session; `None` when the read didn't
    /// find one.
    pub fn listed_row(&self) -> Option<&Conversation> {
        match &self.read {
            SessionRead::Listed(row) => Some(row),
            _ => None,
        }
    }

    /// True when no session is at the file the read was asked for.
    pub fn is_missing(&self) -> bool {
        matches!(self.read, SessionRead::Missing)
    }

    /// This outcome with its listed row given the index and project of
    /// `listed`, the row this read refreshes.
    pub fn in_place_of(mut self, listed: &Conversation) -> Self {
        if let SessionRead::Listed(row) = &mut self.read {
            row.index = listed.index;
            row.project_name.clone_from(&listed.project_name);
            row.project_path.clone_from(&listed.project_path);
        }
        self
    }

    /// The read and the next step for the caller to act on.
    pub fn into_parts(self) -> (SessionRead, NextStep) {
        (self.read, self.next)
    }

    /// The read, and the parse findings missing from the session's copy in
    /// the cache, for a caller that reads the session's content once and
    /// doesn't refresh it.
    pub fn into_read_and_unwritten(self) -> (SessionRead, UnwrittenParse) {
        (self.read, self.unwritten)
    }
}
