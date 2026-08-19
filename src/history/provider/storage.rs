//! Providers that keep whole session transcripts under one or more roots.

use super::SessionRoot;
use crate::cli::DebugLevel;
use crate::error::Result;
use crate::history::Conversation;
use std::path::PathBuf;
use std::time::SystemTime;

/// On-disk identity of a provider's whole-root session cache.
///
/// All three fields are a compatibility contract with caches users already have:
/// `directory` names the folder under `~/.cache/claude-history`, and `magic` plus
/// `schema_version` stamp the file so an incompatible one is discarded rather
/// than misread. Changing any of them silently invalidates existing caches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionCache {
    pub directory: &'static str,
    pub magic: [u8; 8],
    pub schema_version: u32,
}

/// How a provider finds, bounds and parses the sessions under its roots.
///
/// Claude is deliberately absent: its transcripts are partitioned by project
/// directory and loaded through machinery that predates and outgrows this shape,
/// so it reports no storage rather than pretending to fit.
pub trait SessionStorage: Sync {
    fn cache(&self) -> SessionCache;

    /// Every directory this provider stores sessions under, in the order they
    /// should be searched.
    fn roots(&self) -> Result<Vec<SessionRoot>>;

    /// Parse one transcript into a conversation, or `None` if the file holds
    /// nothing this provider recognizes.
    ///
    /// `root` is supplied because the user can redirect a provider's session
    /// directory outside the agent's own tree, where transcripts may be written
    /// in a sibling agent's format.
    fn parse_session(
        &self,
        path: PathBuf,
        root: &SessionRoot,
        modified: Option<SystemTime>,
        debug_level: Option<DebugLevel>,
    ) -> Result<Option<Conversation>>;

    /// Largest transcript worth parsing, or `None` to accept any size.
    ///
    /// A cap trades completeness for a bounded worst case: a single rollout can
    /// run to hundreds of megabytes, and its parsed text is held in memory and
    /// written to the cache for the lifetime of the process.
    fn max_session_bytes(&self) -> Option<u64>;
}
