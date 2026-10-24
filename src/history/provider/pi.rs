//! Pi coding agent sessions, stored under `~/.pi/agent/sessions/`.

use super::walk::{DirectoryLister, DiscoveryLister, FileRoot};
use super::{
    Capability, Deleted, DiscoveredSessions, PathResumeLauncher, RefNamespaces, RootEnvironment,
    SessionCache, SessionDeleter, SessionForker, SessionIdResolver, SessionProvider,
    SessionRenamer, SessionResumer, SessionRoot, SessionStorage, SessionStub, SessionsDirLookup,
    SourceLabels, SubagentFinder, walk,
};
use crate::cli::DebugLevel;
use crate::clock::Clock;
use crate::error::Result;
use crate::history::format::{self, SessionFormat, pi_log};
use crate::history::{Conversation, ParsePermit, Source, parser, pi_loader};
use chrono::{DateTime, Utc};
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

    fn resume(&self) -> Capability<&dyn SessionResumer> {
        Capability::Capable(&LAUNCHER)
    }

    fn fork(&self) -> Capability<&dyn SessionForker> {
        Capability::Capable(&LAUNCHER)
    }

    fn rename(&self) -> Capability<&dyn SessionRenamer> {
        Capability::Capable(self)
    }

    fn delete(&self) -> Capability<&dyn SessionDeleter> {
        Capability::Capable(self)
    }

    fn subagents(&self) -> Capability<&dyn SubagentFinder> {
        Capability::NotCapable(None)
    }

    fn subagent_by_id(&self) -> Capability {
        Capability::NotCapable(None)
    }

    /// A Pi session states its id in its header, not its file name, and two
    /// logs in one project may state the same id. Pi sessions resolve by id
    /// only once listed.
    fn session_id_lookup(&self) -> Capability<&dyn SessionIdResolver> {
        Capability::NotCapable(None)
    }

    /// The id is in the header, so finding a session means reading the header
    /// of every log under the root — affordable once, not per keystroke. Two
    /// logs in one project may state the same id, so more than one can match.
    fn find_sessions_by_id(&self, session_id: &str) -> Result<Vec<PathBuf>> {
        sessions_pi_owns_with_id(&pi_loader::session_root()?, session_id)
    }

    fn workspace_sessions_dir(&self) -> Capability<SessionsDirLookup> {
        Capability::Capable(sessions_dir_of)
    }

    fn current_session_variable(&self) -> Capability<&'static str> {
        Capability::NotCapable(None)
    }
}

impl SessionRenamer for PiProvider {
    fn rename_session(
        &self,
        permit: ParsePermit<'_>,
        path: &Path,
        title: &str,
        renamed_at: DateTime<Utc>,
    ) -> Result<()> {
        pi_log::append_session_rename(permit, path, title, renamed_at)
    }
}

impl SessionDeleter for PiProvider {
    fn delete_session(&self, permit: ParsePermit<'_>, path: &Path) -> Result<Deleted> {
        format::require_owned_transcript(permit, Source::Pi, path)?;
        std::fs::remove_file(path)?;
        Ok(Deleted::just_the_session())
    }
}

/// The directory Pi stores the sessions recorded in `directory` in.
fn sessions_dir_of(environment: &RootEnvironment, directory: &Path) -> Result<PathBuf> {
    Ok(pi_loader::sessions_dir_under(
        &pi_loader::session_root_in(environment)?,
        directory,
    ))
}

/// Every log under `root` stating `session_id` that the list attributes to Pi,
/// by its header alone. A log with an OMP title slot is OMP's whichever
/// directory holds it, and the two agents can share one, so it is left for
/// OMP to find.
fn sessions_pi_owns_with_id(root: &FileRoot, session_id: &str) -> Result<Vec<PathBuf>> {
    let mut owned = Vec::new();
    for path in pi_log::sessions_with_id(&root.root.path, root.depth, session_id)? {
        if pi_log::read_header(&path, Source::Pi)?.is_some_and(|(_, source)| source == Source::Pi) {
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
            schema_version: 17,
        }
    }

    fn roots_in(&self, environment: &RootEnvironment) -> Result<Vec<SessionRoot>> {
        Ok(vec![pi_loader::session_root_in(environment)?.root])
    }

    /// The walk depth belongs to the resolution that produced the root, so it
    /// is re-resolved here rather than guessed from the path.
    fn discover(
        &self,
        root: &SessionRoot,
        lister: &dyn DirectoryLister,
    ) -> Result<DiscoveredSessions> {
        discover_at_depth(lister, root, pi_loader::session_root()?.depth)
    }

    /// Pi sessions resolve by ID only once listed.
    fn rediscover_unlisted_session(
        &self,
        _root: &SessionRoot,
        _locator: &Path,
        _lister: &dyn DirectoryLister,
    ) -> Result<Option<SessionStub>> {
        Ok(None)
    }

    fn parse_session(
        &self,
        permit: ParsePermit<'_>,
        stub: &SessionStub,
        _root: &SessionRoot,
        debug_level: Option<DebugLevel>,
        on_transcript_read: &(dyn Fn() + Sync),
        clock: &dyn Clock,
    ) -> Result<Option<Conversation>> {
        parser::process_session_file(
            permit,
            stub,
            &pi_log::PI_LOG,
            debug_level,
            on_transcript_read,
            clock,
        )
    }
}

/// Every session `depth` directory levels under `root`, listed through
/// `lister`.
fn discover_at_depth(
    lister: &dyn DirectoryLister,
    root: &SessionRoot,
    depth: usize,
) -> Result<DiscoveredSessions> {
    let discovery = DiscoveryLister::beneath(lister, &root.path);
    let transcripts = walk::jsonl_files_at_depth(&discovery, &root.path, depth)?;
    Ok(discovery.discovered(walk::file_stubs(root, transcripts)))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::history::provider::contract_tests::{
        AgentSession, ExpectedCommand, FixtureIds, IdCase, Marker, ProviderFixture, copy_file,
        fixture_with_lines,
    };
    use std::ffi::OsString;

    /// The working directory `tests/fixtures/pi/v1.jsonl` is recorded in.
    const PROJECT: &str = "/tmp/pi-v1";
    /// The directory Pi names for [`PROJECT`].
    const PROJECT_DIR: &str = "--tmp-pi-v1--";

    /// Pi's sessions for the provider contracts in `contract_tests`.
    pub(crate) struct PiFixture;

    impl PiFixture {
        /// One listable session in the directory `project_dir` under
        /// `home`; returns its transcript.
        fn write_session_in(&self, home: &Path, project_dir: &str, session: &str) -> PathBuf {
            let path = self
                .root_under(home)
                .path
                .join(project_dir)
                .join(format!("{session}.jsonl"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v1.jsonl"),
                &path,
            )
            .unwrap();
            path
        }
    }

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
            SessionRoot::new(home.join("sessions")).in_agent_tree()
        }

        fn write_session(&self, home: &Path, session: &str) -> PathBuf {
            self.write_session_in(home, PROJECT_DIR, session)
        }

        fn write_session_in_another_directory(&self, home: &Path, session: &str) -> PathBuf {
            self.write_session_in(home, "--tmp-pi-other--", session)
        }

        fn session_working_dir(&self) -> PathBuf {
            PathBuf::from(PROJECT)
        }

        /// Pi stores its sessions in `<agent directory>/sessions`.
        fn root_variables(&self, home: &Path) -> Vec<(&'static str, OsString)> {
            vec![(pi_loader::AGENT_DIR_VARIABLE, home.into())]
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
                    Marker::agent_searched("tool call", "README.md").in_summary_row("Read 1 file"),
                    Marker::searched("tool result", "tool output searchable"),
                    Marker::searched("user's shell command output", "bash output searchable"),
                    Marker::searched("custom message", "visible custom searchable"),
                    Marker::agent_searched("powershell call", "POWERSHELL_SENTINEL")
                        .in_summary_row(LISTING_RUN),
                    Marker::agent_searched("directory listing", "LS_SENTINEL_DIR")
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

        /// In the directory `write_session` writes to.
        fn write_every_record_session(&self, home: &Path, session: &AgentSession) {
            let path = &session.row.path;
            copy_file(
                path,
                &self
                    .root_under(home)
                    .path
                    .join(PROJECT_DIR)
                    .join(path.file_name().unwrap()),
            );
        }

        /// At the depth `write_session` writes: Pi's own discovery takes its
        /// walk depth from the environment.
        fn discover_listed_by(
            &self,
            home: &Path,
            lister: &dyn DirectoryLister,
        ) -> Result<DiscoveredSessions> {
            discover_at_depth(lister, &self.root_under(home), 1)
        }

        fn find_under(&self, home: &Path, id: &str) -> Result<Vec<PathBuf>> {
            let root = FileRoot {
                root: self.root_under(home),
                depth: 1,
            };
            sessions_pi_owns_with_id(&root, id)
        }

        fn expected_fork(&self, locator: &Path) -> ExpectedCommand {
            ExpectedCommand {
                program: "pi",
                arguments: vec![OsString::from("--fork"), locator.into()],
                current_dir: None,
            }
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
    fn delete_removes_only_transcript_pi_owns() {
        let directory = tempfile::tempdir().unwrap();
        let session = directory.path().join("session.jsonl");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v1.jsonl"),
            &session,
        )
        .unwrap();
        let sibling = directory.path().join("sibling.txt");
        std::fs::write(&sibling, "keep").unwrap();

        PiProvider
            .delete_session(crate::history::FOR_FIXTURES, &session)
            .unwrap();

        assert!(!session.exists());
        assert!(sibling.exists());
        assert!(
            PiProvider
                .delete_session(crate::history::FOR_FIXTURES, &sibling)
                .is_err(),
            "a file Pi does not own must survive a delete aimed at it"
        );
    }

    #[test]
    fn find_by_id_leaves_titled_log_in_shared_session_directory_to_omp() {
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
    fn log_under_pis_root_is_stored_and_its_copy_elsewhere_is_not() {
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
