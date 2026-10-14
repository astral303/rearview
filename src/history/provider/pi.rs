//! Pi coding agent sessions, stored under `~/.pi/agent/sessions/`.

use super::walk::FileRoot;
use super::{
    Capability, Deleted, DiscoveredSessions, PathResumeLauncher, RefNamespaces, ResolvedSession,
    RootEnvironment, SessionCache, SessionLauncher, SessionProvider, SessionRoot, SessionStorage,
    SessionStub, SourceLabels, walk,
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
            conversation: "agent-pi-v1",
            project: "agent-pi-project-v1",
        }
    }

    fn storage(&self) -> &dyn SessionStorage {
        &PiStorage
    }

    fn storage_env_vars(&self) -> &'static [&'static str] {
        pi_loader::STORAGE_ENV_VARS
    }

    fn format(&self) -> &dyn SessionFormat {
        &pi_log::PI_LOG
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

    fn subagents(&self) -> Capability {
        Capability::NotCapable
    }

    fn subagent_by_id(&self) -> Capability {
        Capability::NotCapable
    }

    fn session_id_lookup(&self) -> Capability {
        Capability::NotCapable
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
            schema_version: 13,
        }
    }

    fn roots_in(&self, environment: &RootEnvironment) -> Result<Vec<SessionRoot>> {
        Ok(vec![pi_loader::session_root_in(environment)?.root])
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
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::history::provider::contract_tests::{
        AgentSession, FixtureIds, IdCase, Marker, ProviderFixture, fixture_with_lines,
    };

    /// Pi's sessions for the provider contracts in `contract_tests`.
    pub(crate) struct PiFixture;

    impl ProviderFixture for PiFixture {
        fn provider(&self) -> &'static dyn SessionProvider {
            Source::Pi.provider()
        }

        fn ids(&self) -> FixtureIds {
            FixtureIds {
                session: "session",
                other: "other",
                unknown: "absent",
                session_in_other_case: "SESSION",
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

        fn every_record_session(&self) -> AgentSession {
            let (session, storage) = fixture_with_lines(
                &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl"),
                &[
                    r#"{"type":"message","id":"list-u","parentId":"name2","timestamp":"2024-03-01T00:18:00.000Z","message":{"role":"user","content":"List the source directory"}}"#,
                    r#"{"type":"message","id":"list-a","parentId":"list-u","timestamp":"2024-03-01T00:18:01.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_ps","name":"powershell","arguments":{"command":"Get-ChildItem POWERSHELL_SENTINEL"}},{"type":"toolCall","id":"call_ls","name":"ls","arguments":{"path":"LS_SENTINEL_DIR"}}]}}"#,
                    r#"{"type":"message","id":"list-ps","parentId":"list-a","timestamp":"2024-03-01T00:18:02.000Z","message":{"role":"toolResult","toolCallId":"call_ps","toolName":"powershell","content":[{"type":"text","text":"main.rs"}]}}"#,
                    r#"{"type":"message","id":"list-ls","parentId":"list-ps","timestamp":"2024-03-01T00:18:03.000Z","message":{"role":"toolResult","toolCallId":"call_ls","toolName":"ls","content":[{"type":"text","text":"main.rs"}]}}"#,
                    r#"{"type":"message","id":"empty-u","parentId":"list-ls","timestamp":"2024-03-01T00:19:00.000Z","message":{"role":"user","content":"Is the build clean?"}}"#,
                    r#"{"type":"message","id":"empty-a","parentId":"empty-u","timestamp":"2024-03-01T00:19:01.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_empty","name":"read","arguments":{"path":"notes.txt"}}]}}"#,
                    r#"{"type":"message","id":"empty-t","parentId":"empty-a","timestamp":"2024-03-01T00:19:02.000Z","message":{"role":"toolResult","toolCallId":"call_empty","content":[]}}"#,
                    r#"{"type":"message","id":"empty-b","parentId":"empty-t","timestamp":"2024-03-01T00:19:03.000Z","message":{"role":"bashExecution","command":"true","output":""}}"#,
                    r#"{"type":"custom_message","id":"empty-m","parentId":"empty-b","timestamp":"2024-03-01T00:19:04.000Z","customType":"note","content":"","display":true}"#,
                    r#"{"type":"message","id":"empty-r","parentId":"empty-m","timestamp":"2024-03-01T00:19:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"NUMBERED_REPLY_SENTINEL: the build is clean and the notes file is empty."}]}}"#,
                ]
                .map(str::to_owned),
            );
            const LISTING_RUN: &str = "Listed 1 directory, ran 1 shell command";
            AgentSession::new(
                Source::Pi,
                session,
                Vec::new(),
                vec![
                    Marker::searched("user text", "active root question"),
                    Marker::searched("assistant text", "root answer"),
                    Marker::shown("thinking", "private reasoning"),
                    Marker::shown("tool call", "README.md").in_summary_row("Read 1 file"),
                    Marker::searched("tool result", "tool output searchable"),
                    Marker::searched("user's shell command output", "bash output searchable"),
                    Marker::searched("custom message", "visible custom searchable"),
                    Marker::shown("powershell call", "POWERSHELL_SENTINEL")
                        .in_summary_row(LISTING_RUN),
                    Marker::shown("directory listing", "LS_SENTINEL_DIR")
                        .in_summary_row(LISTING_RUN),
                ],
            )
            .with_hidden(&[
                ("abandoned branch's prompt", "ABANDONED_BRANCH_SENTINEL"),
                ("abandoned branch's reply", "ABANDONED_ANSWER_SENTINEL"),
                ("custom message with display off", "HIDDEN_CUSTOM_SENTINEL"),
                ("tool result's image data", "TOOL_BASE64_SECRET"),
                ("custom message's image data", "CUSTOM_BASE64_SECRET"),
            ])
            .with_reply_after_records_without_text("NUMBERED_REPLY_SENTINEL", 15)
            .kept_in(storage)
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
