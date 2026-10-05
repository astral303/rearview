//! Pi coding agent sessions, stored under `~/.pi/agent/sessions/`.

use super::walk::FileRoot;
use super::{
    Deleted, DiscoveredSessions, PathResumeLauncher, RefNamespaces, ResolvedSession, SessionCache,
    SessionLauncher, SessionProvider, SessionRoot, SessionStorage, SessionStub, SourceLabels, walk,
};
use crate::cli::DebugLevel;
use crate::error::Result;
use crate::history::format::{self, SessionFormat, pi_log};
use crate::history::{Conversation, Source, parser, pi_loader};
use std::path::{Path, PathBuf};

pub struct PiProvider;

impl SessionProvider for PiProvider {
    fn source(&self) -> Source {
        Source::Pi
    }

    fn labels(&self) -> SourceLabels {
        SourceLabels {
            name: "pi",
            list: "Pi",
            display: "Pi",
        }
    }

    fn ref_namespaces(&self) -> RefNamespaces {
        RefNamespaces {
            conversation: Some("agent-pi-v1"),
            project: "agent-pi-project-v1",
        }
    }

    fn storage(&self) -> Option<&dyn SessionStorage> {
        Some(&PiStorage)
    }

    fn format(&self) -> Option<&dyn SessionFormat> {
        Some(&pi_log::PI_LOG)
    }

    fn launcher(&self) -> &dyn SessionLauncher {
        &LAUNCHER
    }

    fn rename_session(&self, path: &Path, title: &str) -> Result<()> {
        pi_log::append_session_rename(path, title)
    }

    fn delete_session(&self, path: &Path) -> Result<Deleted> {
        format::require_owned_transcript(Source::Pi, path)?;
        std::fs::remove_file(path)?;
        Ok(Deleted::just_the_session())
    }

    /// A Pi session states its id in its header, not its file name, and two
    /// logs in one project may state the same id. Pi sessions resolve by id
    /// only once listed, so no query is answered as an id.
    fn is_session_id_shape(&self, _query: &str) -> bool {
        false
    }

    fn resolve_session_id(&self, _session_id: &str) -> Result<Option<ResolvedSession>> {
        Ok(None)
    }

    /// The id is in the header, so finding a session means reading the header
    /// of every log under the root — affordable once, not per keystroke. Two
    /// logs in one project may state the same id, so more than one can match.
    fn find_sessions_by_id(&self, session_id: &str) -> Result<Vec<PathBuf>> {
        sessions_pi_owns_with_id(&pi_loader::session_root()?, session_id)
    }
}

/// Every log under `root` stating `session_id` that the list attributes to Pi.
/// A log with an OMP title slot is OMP's whichever directory holds it, and the
/// two agents can share one, so it is left for OMP to find.
fn sessions_pi_owns_with_id(root: &FileRoot, session_id: &str) -> Result<Vec<PathBuf>> {
    let mut owned = Vec::new();
    for path in pi_log::sessions_with_id(&root.root.path, root.depth, session_id)? {
        if format::parse_owned_transcript(Source::Pi, &path)?.is_some() {
            owned.push(path);
        }
    }
    Ok(owned)
}

static LAUNCHER: PathResumeLauncher = PathResumeLauncher {
    program: "pi",
    resume_flag: "--session",
    fork_flag: "--fork",
};

struct PiStorage;

impl SessionStorage for PiStorage {
    fn source(&self) -> Source {
        Source::Pi
    }

    fn cache(&self) -> SessionCache {
        SessionCache {
            directory: "pi",
            magic: *b"PIHIST01",
            schema_version: 6,
        }
    }

    fn roots(&self) -> Result<Vec<SessionRoot>> {
        Ok(vec![pi_loader::session_root()?.root])
    }

    /// The walk depth belongs to the resolution that produced the root, so it
    /// is re-resolved here rather than guessed from the path.
    fn discover(&self, root: &SessionRoot) -> Result<DiscoveredSessions> {
        let depth = pi_loader::session_root()?.depth;
        Ok(DiscoveredSessions::complete(walk::file_stubs(
            root,
            walk::jsonl_files_at_depth(&root.path, depth)?,
        )))
    }

    fn parse_session(
        &self,
        stub: &SessionStub,
        _root: &SessionRoot,
        debug_level: Option<DebugLevel>,
        on_transcript_read: &(dyn Fn() + Sync),
    ) -> Result<Option<Conversation>> {
        parser::process_session_file(stub, &pi_log::PI_LOG, debug_level, on_transcript_read)
    }

    fn max_session_bytes(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::history::provider::contract_tests::{
        Contract, FixtureIds, IdCase, Nesting, OptOut, ProviderFixture,
    };

    /// Pi's sessions for the provider contracts in `contract_tests`.
    pub(crate) struct PiFixture;

    impl ProviderFixture for PiFixture {
        fn provider(&self) -> &'static dyn SessionProvider {
            Source::Pi.provider()
        }

        fn opt_outs(&self) -> &'static [OptOut] {
            const OPT_OUTS: &[OptOut] = &[
                OptOut {
                    contracts: &[
                        Contract::SubAgentDiscovery,
                        Contract::SubAgentDelete,
                        Contract::SubAgentDeleteCount,
                    ],
                    reason: "Pi records no sub-agents",
                },
                OptOut {
                    contracts: &[
                        Contract::SessionIdLookup,
                        Contract::SessionIdLookupMatchesDiscovery,
                        Contract::UnknownIdLookup,
                        Contract::IdCaseRule,
                        Contract::SubAgentIdLookup,
                    ],
                    reason: "Pi resolves no id: a session states its id in its header, and \
                             two logs may state the same one",
                },
                OptOut {
                    contracts: &[Contract::RootOverride],
                    reason: "an empty PI_CODING_AGENT_SESSION_DIR resolves to the current \
                             directory, not to the default root",
                },
            ];
            OPT_OUTS
        }

        fn ids(&self) -> FixtureIds {
            FixtureIds {
                session: "session",
                other: "other",
                unknown: "absent",
                session_in_other_case: "SESSION",
                child: None,
                nested: Nesting::NotRecorded("Pi records no sub-agents"),
            }
        }

        fn id_case(&self) -> IdCase {
            IdCase::Insensitive
        }

        fn root_under(&self, home: &Path) -> SessionRoot {
            SessionRoot::new(home).in_agent_tree()
        }

        fn write_session(&self, home: &Path, session: &str) -> PathBuf {
            let path = home.join("project").join(format!("{session}.jsonl"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v1.jsonl"),
                &path,
            )
            .unwrap();
            path
        }

        /// At the depth `write_session` writes: Pi's own discovery takes its
        /// walk depth from the environment.
        fn discover_under(&self, home: &Path) -> Vec<SessionStub> {
            let root = self.root_under(home);
            walk::file_stubs(&root, walk::jsonl_files_at_depth(&root.path, 1).unwrap())
        }

        fn resolve_under(&self, _home: &Path, id: &str) -> Result<Option<ResolvedSession>> {
            PiProvider.resolve_session_id(id)
        }

        fn delete_under(&self, _home: &Path, locator: &Path) -> Result<Deleted> {
            PiProvider.delete_session(locator)
        }

        fn roots_from(&self, override_dir: Option<&str>, home: &Path) -> Vec<PathBuf> {
            let root = pi_loader::session_root_from(
                None,
                override_dir.map(PathBuf::from),
                Some(home.to_path_buf()),
                None,
            );
            vec![root.unwrap().root.path]
        }
    }

    #[test]
    fn delete_removes_only_the_transcript_pi_owns() {
        let directory = tempfile::tempdir().unwrap();
        let session = directory.path().join("session.jsonl");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v1.jsonl"),
            &session,
        )
        .unwrap();
        let sibling = directory.path().join("sibling.txt");
        std::fs::write(&sibling, "keep").unwrap();

        PiProvider.delete_session(&session).unwrap();

        assert!(!session.exists());
        assert!(sibling.exists());
        assert!(
            PiProvider.delete_session(&sibling).is_err(),
            "a file Pi does not own must survive a delete aimed at it"
        );
    }

    #[test]
    fn find_by_id_leaves_the_titled_log_in_a_shared_session_directory_to_omp() {
        let directory = tempfile::tempdir().unwrap();
        let (_, untitled) = pi_log::test_support::write_titled_and_untitled(directory.path());
        let root = FileRoot {
            root: SessionRoot::new(directory.path()),
            depth: 0,
        };

        let found = sessions_pi_owns_with_id(&root, "omp_session_custom_id").unwrap();

        assert_eq!(found, vec![untitled]);
    }

    #[test]
    fn a_log_under_pis_root_is_stored_and_its_copy_elsewhere_is_not() {
        let directory = tempfile::tempdir().unwrap();
        let (_, untitled) = pi_log::test_support::write_titled_and_untitled(directory.path());
        let root = FileRoot {
            root: SessionRoot::new(directory.path()),
            depth: 0,
        };
        let elsewhere = tempfile::tempdir().unwrap();
        let copy = elsewhere.path().join("copy.jsonl");
        std::fs::copy(&untitled, &copy).unwrap();

        let stored = sessions_pi_owns_with_id(&root, "omp_session_custom_id").unwrap();

        let respelled = directory
            .path()
            .join(".")
            .join(untitled.file_name().unwrap());
        assert!(super::super::contains_file(&stored, &respelled));
        assert!(!super::super::contains_file(&stored, &copy));
    }
}
