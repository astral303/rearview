//! The TUI's next step with a session it read: nothing, a second read once a
//! refresh completes, a session refresh on the background worker, or a
//! session refresh once the transcript changes after a failed write.

use super::observer::{self, AwaitedRefresh, Observation};
use super::{CacheBase, is_busy_error};
use crate::error::{AppError, Result};
use crate::history::fingerprint::CachedFingerprint;
use crate::history::listing::SessionTimes;
use crate::history::{Conversation, MessageTimes};
use chrono::{DateTime, Local};
use std::path::PathBuf;

/// The TUI's next step with a session it read.
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use = "a session left without its next step is never refreshed"]
pub enum NextStep {
    /// The database served the read or holds what it read, or a session
    /// refresh wouldn't find anything to cache.
    Settled,
    /// A refresh owns the cache, in this process or another: read the
    /// session again once it completes.
    AwaitRefresh(AwaitedRefresh),
    /// No refresh owns the cache, or its refresh state could not be read: a
    /// session refresh on the TUI's background worker reads the session under
    /// refresh ownership and caches it.
    CacheInBackground,
    /// The read couldn't write what it found to the cache. Refresh the
    /// session once its transcripts no longer stamp as the failure's
    /// `revision` at the TUI's once-a-second check, when the user acts on
    /// it, or when another rearview's refresh it waits for ends.
    RefreshOnChange(Box<WriteFailure>),
}

/// A session read whose write to the cache failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteFailure {
    /// The stamp discovery gave the session's transcripts before the read;
    /// `None` when unknown.
    pub revision: Option<CachedFingerprint>,
    /// The sub-agent transcripts `revision` spans.
    pub subagents: Vec<PathBuf>,
    /// The messages the list judges the session by.
    pub found: FoundMessages,
    /// The error the write failed with.
    pub error: String,
}

/// The messages found by the read behind a [`WriteFailure`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FoundMessages {
    /// They are in the read's row: a parse whose write failed.
    InRow,
    /// The read parsed messages at these times; its row is an older copy
    /// without them.
    Parsed(MessageTimes),
    /// The read didn't parse the session, and its row is an older copy.
    Unparsed,
}

impl WriteFailure {
    /// The times the list judges `row`, the read's row, by: the messages the
    /// read found, or, when it didn't parse them, the transcripts' last write
    /// at this failure's revision.
    pub fn times<'a>(&'a self, row: &'a Conversation) -> SessionTimes<'a> {
        match &self.found {
            FoundMessages::InRow => SessionTimes::Messages(&row.message_times),
            FoundMessages::Parsed(times) => SessionTimes::Messages(times),
            FoundMessages::Unparsed => SessionTimes::LastWrite(
                self.revision
                    .map(|revision| DateTime::<Local>::from(revision.modified())),
            ),
        }
    }
}

impl NextStep {
    /// The failed write this step follows; `None` for any other step.
    pub fn write_failure(&self) -> Option<&WriteFailure> {
        match self {
            Self::RefreshOnChange(failure) => Some(failure.as_ref()),
            Self::Settled | Self::AwaitRefresh(_) | Self::CacheInBackground => None,
        }
    }

    /// [`Self::write_failure`], taken out of this step.
    pub fn into_write_failure(self) -> Option<WriteFailure> {
        match self {
            Self::RefreshOnChange(failure) => Some(*failure),
            Self::Settled | Self::AwaitRefresh(_) | Self::CacheInBackground => None,
        }
    }

    /// The error of the failed write this step follows; `None` for any other
    /// step.
    pub fn write_error(&self) -> Option<&str> {
        self.write_failure().map(|failure| failure.error.as_str())
    }

    /// The next step of a session whose copy in the database under `base` is
    /// out of date or missing, read by the drawing thread or by a read that
    /// could not write: wait for the refresh the cache's observation names,
    /// or request a session refresh while none owns it. The
    /// observation waits up to 100 ms for the database's exclusive lock.
    pub(crate) fn after_read_without_writing(base: &CacheBase) -> Self {
        Self::after_observation(observer::observe(base))
    }

    /// [`Self::after_read_without_writing`] for a drawing-thread read of the
    /// database under `base` that failed with `error`. After a busy error,
    /// observe without waiting: the read already waited 150 ms for the lock,
    /// longer than the observation's 100 ms.
    pub(crate) fn after_failed_interactive_read(base: &CacheBase, error: &AppError) -> Self {
        if is_busy_error(error) {
            Self::after_observation(observer::observe_after_locked_read(base))
        } else {
            Self::after_read_without_writing(base)
        }
    }

    fn after_observation(observation: Result<Observation>) -> Self {
        observation
            .ok()
            .and_then(Observation::awaited)
            .map_or(Self::CacheInBackground, Self::AwaitRefresh)
    }
}
