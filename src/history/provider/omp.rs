//! OMP sessions, stored under `~/.omp/agent/sessions/`, beside which OMP
//! writes each session's artifacts.

use super::walk::SessionFiles;
use super::{
    Deleted, DiscoveredSessions, PathResumeLauncher, RefNamespaces, ResolvedSession, RootOrigin,
    SessionCache, SessionLauncher, SessionProvider, SessionRoot, SessionStorage, SessionStub,
    SourceLabels, walk,
};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::Result;
use crate::history::format::{self, SessionFormat, pi_log};
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
            conversation: Some("agent-omp-v1"),
            project: "agent-omp-project-v1",
        }
    }

    fn storage(&self) -> Option<&dyn SessionStorage> {
        Some(&OmpStorage)
    }

    fn format(&self) -> Option<&dyn SessionFormat> {
        Some(&pi_log::OMP_LOG)
    }

    fn launcher(&self) -> &dyn SessionLauncher {
        &LAUNCHER
    }

    fn rename_session(&self, path: &Path, title: &str) -> Result<()> {
        pi_log::append_omp_session_rename(path, title)
    }

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

    /// An OMP session states its id in its header, not its file name, as a Pi
    /// session does. OMP sessions resolve by id only once listed.
    fn resolve_session_id(&self, _session_id: &str) -> Result<Option<ResolvedSession>> {
        Ok(None)
    }

    /// Read from the header, as Pi's is, and just as capable of repeating
    /// across two logs in one project.
    fn find_sessions_by_id(&self, session_id: &str) -> Result<Vec<PathBuf>> {
        let root = omp_loader::session_root()?;
        pi_log::sessions_with_id(&root.root.path, root.depth, session_id)
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
/// A missing artifacts directory yields none. An unreadable one yields none
/// too and is reported at warn level, so the session still lists and deletes.
///
/// Layout per OMP's `docs/blob-artifact-architecture.md` at `d17c270`.
fn subagent_transcripts_in(artifacts: &Path, debug_level: Option<DebugLevel>) -> Vec<PathBuf> {
    walk::jsonl_files_at_depth(artifacts, 0).unwrap_or_else(|error| {
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
            schema_version: 6,
        }
    }

    fn roots(&self) -> Result<Vec<SessionRoot>> {
        Ok(vec![omp_loader::session_root()?.root])
    }

    /// The walk depth belongs to the resolution that produced the root, so it
    /// is re-resolved here rather than guessed from the path.
    fn discover(&self, root: &SessionRoot) -> Result<DiscoveredSessions> {
        discover_sessions(root, omp_loader::session_root()?.depth)
    }

    /// Everything under OMP's own tree is OMP's, title slot or not. A redirected
    /// session directory belongs to no one in particular, so its session files
    /// are left to the registry to attribute rather than claimed outright. The
    /// artifacts directories beside them are OMP's own, so what they hold is
    /// read as OMP's; the registry would read a file no format claims as a
    /// Claude transcript.
    fn parse_session(
        &self,
        stub: &SessionStub,
        root: &SessionRoot,
        debug_level: Option<DebugLevel>,
    ) -> Result<Option<Conversation>> {
        match root.origin() {
            RootOrigin::AgentTree => {
                parser::process_session_file(stub, &pi_log::OMP_LOG, debug_level)
            }
            RootOrigin::Redirected => {
                let session = format::parse_transcript(&stub.locator)?;
                parser::process_projected_session(stub, session, &pi_log::OMP_LOG, debug_level)
            }
        }
    }

    fn max_session_bytes(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_entry::LogEntry;

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
            .parse_session(&stub, &root, None)
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
    fn deleting_a_session_removes_its_artifacts_directory_and_reports_its_sub_agents() {
        let directory = tempfile::tempdir().unwrap();
        let session = write_session(directory.path(), "session");
        write_subagent(&session, "worker");
        let artifacts = artifacts_directory(&session);
        std::fs::create_dir_all(artifacts.join("tool-results")).unwrap();
        std::fs::write(artifacts.join("tool-results/output.txt"), "result").unwrap();

        let deleted = OmpProvider.delete_session(&session).unwrap();

        assert_eq!(
            deleted,
            Deleted {
                stored_copies: 1,
                subagent_sessions: 1,
            }
        );
        assert!(!session.exists());
        assert!(
            !artifacts.exists(),
            "tool results named after the transcript would be orphaned"
        );
    }

    /// One stub per session, its artifacts directory's `.jsonl` files named
    /// on it in name order; the `.md` reports, the tool results and a
    /// session with no directory contribute nothing.
    #[test]
    fn discovery_names_the_transcripts_in_each_sessions_artifacts_directory() {
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
        let session_without_artifacts = write_session(&project, "2026-01-03T00-00-00_plain");
        let root = SessionRoot::new(directory.path()).in_agent_tree();

        let discovered = discover_sessions(&root, 1).unwrap();

        assert_eq!(discovered.stubs.len(), 2);
        assert_eq!(discovered.stubs[0].locator, session);
        assert_eq!(discovered.stubs[0].subagents, vec![worker, nested]);
        assert_eq!(discovered.stubs[1].locator, session_without_artifacts);
        assert!(discovered.stubs[1].subagents.is_empty());
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
            .parse_session(&stub_without_subagents, &root, None)
            .unwrap()
            .unwrap();
        let stub = discover_sessions(&root, 1).unwrap().stubs.remove(0);

        let merged = OmpStorage
            .parse_session(&stub, &root, None)
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

    /// A redirected session directory holds its sessions beside itself, and
    /// their artifacts directories beside them. The registry attributes the
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
            .parse_session(&stub, &root, None)
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
    fn view_with_two_threads(directory: &Path) -> Vec<(usize, LogEntry)> {
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
    fn thread_labels_at(entries: &[(usize, LogEntry)]) -> Vec<(usize, String)> {
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
