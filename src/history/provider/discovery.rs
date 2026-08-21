//! Where a provider keeps its sessions.

use std::path::PathBuf;

/// Where a root came from, which decides whether a transcript in it that names
/// no agent belongs to the provider that listed the root.
///
/// Only the code that resolves a root knows this. It cannot be recovered from
/// the path afterwards: a home directory can be named after an agent, and a
/// redirected directory can sit anywhere.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootOrigin {
    /// The agent's own installation tree. A transcript here is that agent's even
    /// when the file itself says nothing about which agent wrote it.
    AgentTree,
    /// A directory the user pointed the agent at, through configuration or the
    /// environment. Pi-family agents share one wire format and can be aimed at
    /// the same directory, so an unmarked transcript here names no owner.
    Redirected,
}

/// One place a provider stores sessions — a directory tree for file-backed
/// providers, but any container a provider can enumerate: each root carries its
/// own session cache, keyed by `path`. How sessions are found inside it is the
/// provider's own knowledge, stated in its
/// [`discover`](super::SessionStorage::discover).
///
/// Roots are [`RootOrigin::Redirected`] unless [`SessionRoot::in_agent_tree`]
/// says otherwise. That default is the cautious one: a provider that does not
/// declare a root as its own cannot claim transcripts that name no agent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRoot {
    pub path: PathBuf,
    origin: RootOrigin,
}

impl SessionRoot {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            origin: RootOrigin::Redirected,
        }
    }

    /// Mark this root as part of the agent's own tree rather than a directory
    /// the user redirected it to.
    #[must_use]
    pub fn in_agent_tree(self) -> Self {
        Self {
            origin: RootOrigin::AgentTree,
            ..self
        }
    }

    pub fn origin(&self) -> RootOrigin {
        self.origin
    }
}
