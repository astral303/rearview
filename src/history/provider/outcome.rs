//! One session's read, from the loader to the list or command that asked for
//! it: which file, the transcript revision discovery found, what the read
//! found, and what the TUI does next with the session.

use super::load::ReadError;
use super::storage::SessionStub;
use crate::history::Conversation;
use crate::history::fingerprint::CachedFingerprint;
use crate::history::sqlite_cache::NextStep;
use std::path::{Path, PathBuf};

/// The result of reading one session.
pub enum SessionRead {
    /// The row the list shows for the session.
    Listed(Box<Conversation>),
    /// Read cleanly, and holds no conversation the provider lists.
    Empty,
    /// Neither discovery nor a lookup of the file finds a session there.
    Missing,
    /// Could not be read.
    Failed(ReadError),
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
}

impl SessionOutcome {
    /// `read`, the read of the session at `stub`, settled.
    pub(super) fn settled(stub: &SessionStub, read: SessionRead) -> Self {
        Self {
            locator: stub.locator.clone(),
            revision: stub.fingerprint.stamp(),
            read,
            next: NextStep::Settled,
        }
    }

    /// No session at `locator`.
    pub(super) fn missing(locator: &Path) -> Self {
        Self::unread(locator, SessionRead::Missing)
    }

    /// The session at `locator`, which a failed discovery or lookup kept from
    /// being read: it may still be there, unchanged.
    pub(super) fn failed_transiently(locator: &Path) -> Self {
        Self::unread(locator, SessionRead::Failed(ReadError::Transient))
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

    pub fn read(&self) -> &SessionRead {
        &self.read
    }

    pub fn next(&self) -> NextStep {
        self.next
    }

    /// This outcome with a listed row filed where `listed`, the row it reads
    /// again, is: at its index, under its project.
    pub fn in_place_of(mut self, listed: &Conversation) -> Self {
        if let SessionRead::Listed(row) = &mut self.read {
            row.index = listed.index;
            row.project_name.clone_from(&listed.project_name);
            row.project_path.clone_from(&listed.project_path);
        }
        self
    }

    /// The read, once the caller has acted on [`Self::next`].
    pub fn into_read(self) -> SessionRead {
        self.read
    }
}
