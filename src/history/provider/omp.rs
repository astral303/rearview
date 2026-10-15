//! OMP sessions, stored under `~/.omp/agent/sessions/`, beside which OMP
//! writes each session's artifacts.

use super::walk::{FileRoot, SessionFiles};
use super::{
    Capability, Deleted, DiscoveredSessions, PathResumeLauncher, RefNamespaces, RootEnvironment,
    RootOrigin, SessionCache, SessionDeleter, SessionForker, SessionIdResolver, SessionProvider,
    SessionRenamer, SessionResumer, SessionRoot, SessionStorage, SessionStub, SessionsDirLookup,
    SourceLabels, SubagentFinder, walk,
};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::Result;
use crate::history::format::{self, SessionFormat, SessionProjection, pi_log};
use crate::history::omp_loader::UserDirectories;
use crate::history::{Conversation, Source, omp_loader, parser};
use std::path::{Path, PathBuf};

pub struct OmpProvider;

impl SessionProvider for OmpProvider {
    fn source(&self) -> Source {
        Source::Omp
    }

    fn labels(&self) -> SourceLabels {
        SourceLabels {
            name: "omp",
            list: "OMP",
            display: "OMP",
        }
    }

    fn ref_namespaces(&self) -> RefNamespaces {
        RefNamespaces {
            conversation: "agent-omp-v1",
            project: "agent-omp-project-v1",
        }
    }

    fn storage(&self) -> &dyn SessionStorage {
        &OmpStorage
    }

    fn storage_env_vars(&self) -> &'static [&'static str] {
        omp_loader::STORAGE_ENV_VARS
    }

    fn format(&self) -> &dyn SessionFormat {
        &pi_log::OMP_LOG
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
        Capability::Capable(self)
    }

    fn subagent_by_id(&self) -> Capability {
        Capability::NotCapable(None)
    }

    /// An OMP session states its id in its header, not its file name, as a Pi
    /// session does. OMP sessions resolve by id only once listed.
    fn session_id_lookup(&self) -> Capability<&dyn SessionIdResolver> {
        Capability::NotCapable(None)
    }

    /// Read from the header, as Pi's is, and just as capable of repeating
    /// across two logs in one project.
    fn find_sessions_by_id(&self, session_id: &str) -> Result<Vec<PathBuf>> {
        sessions_omp_owns_with_id(&omp_loader::session_root()?, session_id)
    }

    fn workspace_sessions_dir(&self) -> Capability<SessionsDirLookup> {
        Capability::Capable(sessions_dir_of)
    }

    fn current_session_variable(&self) -> Capability<&'static str> {
        Capability::NotCapable(None)
    }
}

impl SessionRenamer for OmpProvider {
    fn rename_session(&self, path: &Path, title: &str) -> Result<()> {
        pi_log::append_omp_session_rename(path, title)
    }
}

impl SessionDeleter for OmpProvider {
    /// OMP keeps a session's tool results and sub-agent transcripts in its
    /// artifacts directory, so deleting the transcript alone would orphan
    /// them.
    fn delete_session(&self, path: &Path) -> Result<Deleted> {
        format::require_owned_transcript(Source::Omp, path)?;
        let artifacts = artifacts_directory(path);
        let subagent_sessions = subagent_transcripts_in(&artifacts, None).len();
        std::fs::remove_file(path)?;
        if artifacts.is_dir() {
            std::fs::remove_dir_all(artifacts)?;
        }
        Ok(Deleted {
            subagent_sessions,
            ..Deleted::just_the_session()
        })
    }
}

impl SubagentFinder for OmpProvider {
    fn subagent_transcripts(&self, locator: &Path) -> Result<Vec<PathBuf>> {
        read_subagent_transcripts(&artifacts_directory(locator))
    }
}

/// The directory OMP stores the sessions recorded in `directory` in.
fn sessions_dir_of(environment: &RootEnvironment, directory: &Path) -> Result<PathBuf> {
    Ok(UserDirectories::of(environment)?
        .sessions_dir_under(&omp_loader::session_root_in(environment)?, directory))
}

/// Every log under `root` stating `session_id` that the list attributes to
/// OMP: those [`attributed_projection`] reads as OMP's.
fn sessions_omp_owns_with_id(root: &FileRoot, session_id: &str) -> Result<Vec<PathBuf>> {
    let mut owned = Vec::new();
    for path in pi_log::sessions_with_id(&root.root.path, root.depth, session_id)? {
        if attributed_projection(&root.root, &path)?
            .is_some_and(|session| session.source == Source::Omp)
        {
            owned.push(path);
        }
    }
    Ok(owned)
}

/// The log at `path` parsed by the reader `root` assigns. Everything under
/// OMP's own tree is OMP's, title slot or not. A `PI_CODING_AGENT_SESSION_DIR`
/// directory belongs to no one in particular, so its logs are left to the
/// registry to attribute rather than claimed outright, and the projection there
/// may name Pi.
fn attributed_projection(root: &SessionRoot, path: &Path) -> Result<Option<SessionProjection>> {
    match root.origin() {
        RootOrigin::AgentTree => pi_log::OMP_LOG.parse_transcript(path),
        RootOrigin::Redirected => format::parse_transcript(path),
    }
}

static LAUNCHER: PathResumeLauncher = PathResumeLauncher {
    program: "omp",
    resume_flag: "--resume",
    fork_flag: "--fork",
};

/// The directory OMP writes a session's artifacts to, when it has any: the
/// transcript's path with `.jsonl` stripped.
fn artifacts_directory(transcript: &Path) -> PathBuf {
    transcript.with_extension("")
}

/// The sub-agent transcripts in a session's artifacts directory: the `.jsonl`
/// files directly inside it, sorted. OMP names each after the sub-agent's
/// output id, a nested one dot-qualified (`Parent.Child.jsonl`) in the same
/// directory, and writes a `.md` report beside it that is not a transcript.
/// A missing artifacts directory yields none; an unreadable one is an error.
///
/// Layout per OMP's `docs/blob-artifact-architecture.md` at `d17c270`.
fn read_subagent_transcripts(artifacts: &Path) -> Result<Vec<PathBuf>> {
    walk::jsonl_files_at_depth(artifacts, 0)
}

/// [`read_subagent_transcripts`] for discovery and delete: an unreadable
/// artifacts directory yields none and is reported at warn level, so the
/// session still lists and deletes.
fn subagent_transcripts_in(artifacts: &Path, debug_level: Option<DebugLevel>) -> Vec<PathBuf> {
    read_subagent_transcripts(artifacts).unwrap_or_else(|error| {
        debug::warn(
            debug_level,
            &format!(
                "Failed to list the sub-agent transcripts in {}: {error}",
                artifacts.display()
            ),
        );
        Vec::new()
    })
}

/// Every session `depth` levels under `root`, each with the sub-agent
/// transcripts its artifacts directory holds.
fn discover_sessions(root: &SessionRoot, depth: usize) -> Result<DiscoveredSessions> {
    let sessions = walk::jsonl_files_at_depth(&root.path, depth)?
        .into_iter()
        .map(|transcript| SessionFiles {
            subagents: subagent_transcripts_in(&artifacts_directory(&transcript), None),
            transcript,
        })
        .collect();
    Ok(DiscoveredSessions::complete(walk::session_stubs(
        root, sessions,
    )))
}

struct OmpStorage;

impl SessionStorage for OmpStorage {
    fn source(&self) -> Source {
        Source::Omp
    }

    fn cache(&self) -> SessionCache {
        SessionCache {
            directory: "omp",
            magic: *b"OMHIST01",
            schema_version: 16,
        }
    }

    fn roots_in(&self, environment: &RootEnvironment) -> Result<Vec<SessionRoot>> {
        Ok(vec![omp_loader::session_root_in(environment)?.root])
    }

    /// The walk depth belongs to the resolution that produced the root, so it
    /// is re-resolved here rather than guessed from the path.
    fn discover(&self, root: &SessionRoot) -> Result<DiscoveredSessions> {
        discover_sessions(root, omp_loader::session_root()?.depth)
    }

    /// The artifacts directories beside a session file are OMP's own, so what
    /// they hold is read as OMP's; the registry would read a file no format
    /// claims as a Claude transcript.
    fn parse_session(
        &self,
        stub: &SessionStub,
        root: &SessionRoot,
        debug_level: Option<DebugLevel>,
        on_transcript_read: &(dyn Fn() + Sync),
    ) -> Result<Option<Conversation>> {
        parser::process_projected_session(
            stub,
            attributed_projection(root, &stub.locator)?,
            &pi_log::OMP_LOG,
            debug_level,
            on_transcript_read,
        )
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::history::omp_loader;
    use crate::history::provider::contract_tests::{
        AgentSession, ExpectedCommand, FixtureIds, IdCase, Marker, ProviderFixture,
        fixture_with_lines,
    };
    use crate::log_entry::LogEntry;
    use std::ffi::OsString;

    /// The output id of the sub-agent the contract fixture writes.
    const SUBAGENT: &str = "worker";
    /// The output id of the sub-agent [`SUBAGENT`] ran.
    const NESTED_SUBAGENT: &str = "reviewer";
    /// The working directory `tests/fixtures/omp/v3.jsonl` is recorded in.
    const PROJECT: &str = "/tmp/omp-project";
    /// The directory OMP names for [`PROJECT`].
    const PROJECT_DIR: &str = "--tmp-omp-project--";

    /// The directory under `home` holding the fixture's sessions.
    fn project_dir_under(home: &Path) -> PathBuf {
        OmpFixture.root_under(home).path.join(PROJECT_DIR)
    }

    /// OMP's sessions for the provider contracts in `contract_tests`.
    pub(crate) struct OmpFixture;

    impl ProviderFixture for OmpFixture {
        fn provider(&self) -> &'static dyn SessionProvider {
            Source::Omp.provider()
        }

        fn ids(&self) -> FixtureIds {
            FixtureIds {
                session: "2026-01-02T03-04-05_omp",
                other: "2026-01-03T00-00-00_plain",
                unknown: "2026-01-04T00-00-00_absent",
                session_in_other_case: "2026-01-02T03-04-05_OMP",
            }
        }

        fn id_case(&self) -> IdCase {
            IdCase::Exact
        }

        fn root_under(&self, home: &Path) -> SessionRoot {
            SessionRoot::new(home.join("sessions")).in_agent_tree()
        }

        fn write_session(&self, home: &Path, session: &str) -> PathBuf {
            write_session(&project_dir_under(home), session)
        }

        fn session_working_dir(&self) -> PathBuf {
            PathBuf::from(PROJECT)
        }

        /// OMP stores its sessions in `<agent directory>/sessions`.
        fn root_variables(&self, home: &Path) -> Vec<(&'static str, OsString)> {
            vec![(crate::history::pi_loader::AGENT_DIR_VARIABLE, home.into())]
        }

        fn every_record_session(&self) -> AgentSession {
            let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp");
            let (session, storage) = fixture_with_lines(
                &fixtures.join("v3.jsonl"),
                &[
                    r#"{"type":"reset_boundary","id":"clear","parentId":"a2","timestamp":"2026-01-02T03:04:14.000Z"}"#,
                    r#"{"type":"message","id":"empty-u","parentId":"clear","timestamp":"2026-01-02T03:04:15.000Z","message":{"role":"user","content":[{"type":"text","text":"Is the build clean?"}]}}"#,
                    r#"{"type":"message","id":"empty-a","parentId":"empty-u","timestamp":"2026-01-02T03:04:16.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_empty","name":"read","arguments":{"path":"notes.txt"}}]}}"#,
                    r#"{"type":"message","id":"empty-t","parentId":"empty-a","timestamp":"2026-01-02T03:04:17.000Z","message":{"role":"toolResult","toolCallId":"call_empty","content":[]}}"#,
                    r#"{"type":"message","id":"empty-b","parentId":"empty-t","timestamp":"2026-01-02T03:04:18.000Z","message":{"role":"bashExecution","command":"true","output":""}}"#,
                    r#"{"type":"custom_message","id":"empty-m","parentId":"empty-b","timestamp":"2026-01-02T03:04:19.000Z","customType":"note","content":"","display":true}"#,
                    r#"{"type":"message","id":"empty-r","parentId":"empty-m","timestamp":"2026-01-02T03:04:20.000Z","message":{"role":"assistant","content":[{"type":"text","text":"NUMBERED_REPLY_SENTINEL: the build is clean and the notes file is empty."}]}}"#,
                    r#"{"type":"message","id":"bash-u","parentId":"empty-r","timestamp":"2026-01-02T03:04:21.000Z","message":{"role":"user","content":[{"type":"text","text":"Show the last commit"}]}}"#,
                    r#"{"type":"message","id":"bash-a","parentId":"bash-u","timestamp":"2026-01-02T03:04:22.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_bash","name":"bash","arguments":{"command":"git log -1 OMP_COMMAND_SENTINEL","i":"show the last commit"}}]}}"#,
                    r#"{"type":"message","id":"bash-t","parentId":"bash-a","timestamp":"2026-01-02T03:04:23.000Z","message":{"role":"toolResult","toolCallId":"call_bash","toolName":"bash","content":[{"type":"text","text":"commit a1b2c3d"}]}}"#,
                ]
                .map(str::to_owned),
            );
            AgentSession::new(
                Source::Omp,
                session,
                vec![fixtures.join("subagent.jsonl")],
                vec![
                    Marker::searched("user text", "OMP active question"),
                    Marker::searched("assistant text", "OMP active answer"),
                    Marker::shown("thinking", "OMP private reasoning"),
                    Marker::agent_searched("sub-agent message", "OMP sub-agent answer searchable"),
                    Marker::searched("/clear boundary", "Context cleared"),
                    Marker::agent_searched("tool call", "OMP_COMMAND_SENTINEL")
                        .in_summary_row("Ran 1 shell command"),
                ],
            )
            .with_hidden(&[("abandoned branch's prompt", "OMP_ABANDONED_SENTINEL")])
            .with_reply_after_records_without_text("NUMBERED_REPLY_SENTINEL", 13)
            // The sub-agent's reply: 7 input and 3 output.
            .with_subagent_tokens(10)
            .kept_in(storage)
        }

        fn write_subagent(&self, home: &Path, session: &str) -> PathBuf {
            write_subagent(
                &project_dir_under(home).join(format!("{session}.jsonl")),
                SUBAGENT,
            )
        }

        /// OMP names a nested sub-agent's transcript after the sub-agent
        /// that ran it (`worker.reviewer`), beside its parent's.
        fn write_nested_subagent(&self, home: &Path, session: &str) -> PathBuf {
            write_subagent(
                &project_dir_under(home).join(format!("{session}.jsonl")),
                &format!("{SUBAGENT}.{NESTED_SUBAGENT}"),
            )
        }

        /// At the depth `write_session` writes: OMP's own discovery takes its
        /// walk depth from the environment.
        fn discover_under(&self, home: &Path) -> Vec<SessionStub> {
            discover_sessions(&self.root_under(home), 1).unwrap().stubs
        }

        fn find_under(&self, home: &Path, id: &str) -> Result<Vec<PathBuf>> {
            let root = FileRoot {
                root: self.root_under(home),
                depth: 1,
            };
            sessions_omp_owns_with_id(&root, id)
        }

        fn roots_from(&self, override_dir: Option<&str>, home: &Path) -> Vec<PathBuf> {
            let root = omp_loader::session_root_from(
                None,
                None,
                override_dir.map(PathBuf::from),
                None,
                None,
                Some(home.to_path_buf()),
                None,
            );
            vec![root.unwrap().root.path]
        }

        fn expected_fork(&self, locator: &Path) -> ExpectedCommand {
            ExpectedCommand {
                program: "omp",
                arguments: vec![OsString::from("--fork"), locator.into()],
                current_dir: None,
            }
        }
    }

    fn parsed_source(root: SessionRoot) -> Source {
        std::fs::create_dir_all(&root.path).unwrap();
        let path = root.path.join("session.jsonl");
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/pi/v3-branched.jsonl"),
            &path,
        )
        .unwrap();
        let stub = walk::file_stubs(&root, vec![path]).remove(0);
        OmpStorage
            .parse_session(&stub, &root, None, &|| {})
            .unwrap()
            .unwrap()
            .source
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/omp")
            .join(name)
    }

    /// The session fixture copied to `<directory>/<name>.jsonl`.
    fn write_session(directory: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(directory).unwrap();
        let session = directory.join(format!("{name}.jsonl"));
        std::fs::copy(fixture("v3.jsonl"), &session).unwrap();
        session
    }

    /// A sub-agent transcript in the session's artifacts directory, with the
    /// `.md` report OMP writes beside it, returning the transcript's path.
    fn write_subagent(session: &Path, output_id: &str) -> PathBuf {
        let artifacts = artifacts_directory(session);
        std::fs::create_dir_all(&artifacts).unwrap();
        let transcript = artifacts.join(format!("{output_id}.jsonl"));
        std::fs::copy(fixture("subagent.jsonl"), &transcript).unwrap();
        std::fs::write(artifacts.join(format!("{output_id}.md")), "# report\n").unwrap();
        transcript
    }

    #[test]
    fn deleting_a_session_removes_its_artifacts_directory() {
        let directory = tempfile::tempdir().unwrap();
        let session = write_session(directory.path(), "session");
        let artifacts = artifacts_directory(&session);
        std::fs::create_dir_all(artifacts.join("tool-results")).unwrap();
        std::fs::write(artifacts.join("tool-results/output.txt"), "result").unwrap();

        OmpProvider.delete_session(&session).unwrap();

        assert!(!session.exists());
        assert!(
            !artifacts.exists(),
            "tool results named after the transcript would be orphaned"
        );
    }

    /// A session's artifacts directory names its `.jsonl` files in name
    /// order; the `.md` reports and the tool results contribute nothing.
    #[test]
    fn only_the_artifacts_directorys_transcripts_are_named_in_name_order() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        let session = write_session(&project, "2026-01-02T03-04-05_omp");
        let nested = write_subagent(&session, "worker.reviewer");
        let worker = write_subagent(&session, "worker");
        std::fs::create_dir_all(artifacts_directory(&session).join("tool-results")).unwrap();
        std::fs::write(
            artifacts_directory(&session).join("tool-results/output.txt"),
            "result",
        )
        .unwrap();
        let root = SessionRoot::new(directory.path()).in_agent_tree();

        let discovered = discover_sessions(&root, 1).unwrap();

        assert_eq!(discovered.stubs[0].subagents, vec![worker, nested]);
    }

    /// Each sub-agent transcript's text reaches the row's `agent_search_text`
    /// and not `full_text`, and its messages and tokens are counted, as for
    /// the other providers.
    #[test]
    fn a_sessions_row_merges_its_sub_agent_transcripts() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        let session = write_session(&project, "session");
        write_subagent(&session, "worker");
        write_subagent(&session, "worker.reviewer");
        let root = SessionRoot::new(directory.path()).in_agent_tree();
        let stub_without_subagents = walk::file_stubs(&root, vec![session.clone()]).remove(0);
        let without_subagents = OmpStorage
            .parse_session(&stub_without_subagents, &root, None, &|| {})
            .unwrap()
            .unwrap();
        let stub = discover_sessions(&root, 1).unwrap().stubs.remove(0);

        let merged = OmpStorage
            .parse_session(&stub, &root, None, &|| {})
            .unwrap()
            .unwrap();

        assert_eq!(merged.source, Source::Omp);
        assert_eq!(
            merged
                .agent_search_text
                .matches("OMP sub-agent answer searchable")
                .count(),
            2
        );
        assert!(!merged.full_text.contains("OMP sub-agent answer searchable"));
        assert_eq!(merged.message_count, without_subagents.message_count + 4);
        assert_eq!(merged.total_tokens, without_subagents.total_tokens + 20);
        assert_eq!(merged.subagents, stub.subagents);
    }

    /// A session directory override holds its sessions directly inside it,
    /// and their artifacts directories beside them. The registry attributes the
    /// session file. The artifacts directory is OMP's own, so a non-OMP file
    /// there is skipped; the registry would have read it as a Claude
    /// transcript.
    #[test]
    fn a_redirected_root_merges_the_artifacts_directorys_transcripts_too() {
        let directory = tempfile::tempdir().unwrap();
        let session = write_session(directory.path(), "session");
        let worker = write_subagent(&session, "worker");
        let claude_shaped = artifacts_directory(&session).join("notes.jsonl");
        std::fs::write(
            &claude_shaped,
            concat!(
                r#"{"type":"user","timestamp":"2026-01-02T03:04:12.500Z","message":{"role":"user","content":"CLAUDE_SHAPED_SENTINEL"}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-01-02T03:04:12.600Z","message":{"role":"assistant","content":[{"type":"text","text":"CLAUDE_SHAPED_SENTINEL"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let root = SessionRoot::new(directory.path());
        let stub = discover_sessions(&root, 0).unwrap().stubs.remove(0);
        assert_eq!(stub.subagents, vec![claude_shaped, worker]);

        let merged = OmpStorage
            .parse_session(&stub, &root, None, &|| {})
            .unwrap()
            .unwrap();

        assert_eq!(merged.source, Source::Omp, "the title slot names OMP");
        assert!(
            merged
                .agent_search_text
                .contains("OMP sub-agent answer searchable")
        );
        assert!(!merged.agent_search_text.contains("CLAUDE_SHAPED_SENTINEL"));
    }

    /// The artifacts path can be a file, or a directory the user cannot
    /// read. Failing the root would delist every OMP session for one
    /// directory, and failing the delete would leave the session in place.
    #[test]
    fn a_session_whose_artifacts_directory_cannot_be_read_still_lists_and_deletes() {
        let directory = tempfile::tempdir().unwrap();
        let session = write_session(directory.path(), "session");
        let artifacts = artifacts_directory(&session);
        std::fs::write(&artifacts, "not a directory").unwrap();
        let root = SessionRoot::new(directory.path()).in_agent_tree();

        let discovered = discover_sessions(&root, 0).unwrap();
        assert_eq!(discovered.stubs.len(), 1);
        assert!(discovered.stubs[0].subagents.is_empty());

        let deleted = OmpProvider.delete_session(&session).unwrap();

        assert_eq!(deleted, Deleted::just_the_session());
        assert!(!session.exists());
        assert!(artifacts.exists(), "a file that is not OMP's is left alone");
    }

    /// The view of the session fixture with the `worker` and
    /// `worker.reviewer` threads spliced in.
    fn view_with_two_threads(directory: &Path) -> Vec<(format::RecordLine, LogEntry)> {
        let session = write_session(directory, "session");
        let subagents = vec![
            write_subagent(&session, "worker"),
            write_subagent(&session, "worker.reviewer"),
        ];
        format::view_projection(&pi_log::OMP_LOG, &session, &subagents)
            .unwrap()
            .unwrap()
            .entries
    }

    /// The label of each spliced entry, with its position in the view.
    fn thread_labels_at(entries: &[(format::RecordLine, LogEntry)]) -> Vec<(usize, String)> {
        entries
            .iter()
            .enumerate()
            .filter_map(|(index, (_, entry))| match entry {
                LogEntry::Progress { data, .. } => {
                    Some((index, data["agentId"].as_str().unwrap().to_owned()))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_view_splices_each_thread_in_under_its_output_id() {
        let directory = tempfile::tempdir().unwrap();

        let labels_at = thread_labels_at(&view_with_two_threads(directory.path()));

        for label in ["worker", "worker.reviewer"] {
            assert_eq!(
                labels_at.iter().filter(|(_, found)| found == label).count(),
                2,
                "each thread's question and answer splice in under its output id"
            );
        }
    }

    /// The threads' timestamps fall between the session's last question and
    /// its answer, so that is where the view places them.
    #[test]
    fn spliced_threads_sit_between_the_dispatching_question_and_its_answer() {
        use crate::log_entry::{extract_text_from_assistant, extract_text_from_user};

        let directory = tempfile::tempdir().unwrap();
        let entries = view_with_two_threads(directory.path());
        let position_of = |text: &str| {
            entries
                .iter()
                .position(|(_, entry)| match entry {
                    LogEntry::User { message, .. } => extract_text_from_user(message) == text,
                    LogEntry::Assistant { message, .. } => {
                        extract_text_from_assistant(message) == text
                    }
                    _ => false,
                })
                .unwrap()
        };
        let dispatched = position_of("OMP active question");
        let answered = position_of("OMP active answer");

        let positions = thread_labels_at(&entries)
            .into_iter()
            .map(|(index, _)| index)
            .collect::<Vec<_>>();

        assert!(
            positions
                .iter()
                .all(|index| dispatched < *index && *index < answered),
            "thread entries at {positions:?} sit outside {dispatched}..{answered}"
        );
    }

    #[test]
    fn find_by_id_leaves_the_untitled_log_in_a_shared_session_directory_to_pi() {
        let directory = tempfile::tempdir().unwrap();
        let (titled, _) = pi_log::test_support::write_titled_and_untitled(directory.path());
        let root = FileRoot {
            root: SessionRoot::new(directory.path()),
            depth: 0,
        };

        let found = sessions_omp_owns_with_id(&root, "omp_session_custom_id").unwrap();

        assert_eq!(found, vec![titled]);
    }

    #[test]
    fn find_by_id_keeps_every_log_under_omps_own_tree() {
        let directory = tempfile::tempdir().unwrap();
        let (titled, untitled) = pi_log::test_support::write_titled_and_untitled(directory.path());
        let root = FileRoot {
            root: SessionRoot::new(directory.path()).in_agent_tree(),
            depth: 0,
        };

        let found = sessions_omp_owns_with_id(&root, "omp_session_custom_id").unwrap();

        assert_eq!(found, vec![titled, untitled]);
    }

    /// A transcript with no OMP title slot cannot say which agent wrote it, so the
    /// tree holding it decides. Attributing it wrongly would also mislabel every
    /// assistant turn in the viewer.
    #[test]
    fn an_untitled_transcript_belongs_to_the_tree_that_holds_it() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            parsed_source(SessionRoot::new(directory.path().join("own-tree")).in_agent_tree()),
            Source::Omp
        );
        assert_eq!(
            parsed_source(SessionRoot::new(directory.path().join("redirected"))),
            Source::Pi,
            "a directory the user redirected OMP to can hold Pi's transcripts"
        );
    }
}
