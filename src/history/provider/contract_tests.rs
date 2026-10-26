//! Behaviors every provider promises, each tested once over every registered
//! provider. Every contract runs through [`run_contract`], the one code path
//! that reaches the fixtures: the fixture list is private to [`runner`].
//!
//! A provider's capability declarations decide which contracts run for it
//! ([`Contract::runs_for`]). A Capable declaration turns on every contract
//! that needs the capability. A Not Capable declaration turns them off, and
//! turns on its declaration contract where it has one. A declaration
//! contract checks the declaration against the provider's behaviour. A
//! provider without a fixture, and a fixture that cannot run a contract its
//! provider's declarations turn on, each fail the test.
//!
//! A new provider implements [`ProviderFixture`] in its own test module and
//! joins the fixture list in [`runner`]. A test a second provider could pass
//! unchanged belongs here. A provider's own test module keeps only what is
//! specific to that agent's storage.
//!
//! The display, search, message-numbering and token-count coverage tests read
//! each provider's [`ProviderFixture::every_record_session`] through
//! [`every_record_sessions`]. It fails for a provider without a fixture.

use super::walk::test_support::ScriptedLister;
use super::walk::{DirectoryLister, SystemLister};
use super::{
    Capability, Deleted, DenialDetail, DiscoveredSessions, Fingerprint, Operation, ResolvedSession,
    RootEnvironment, SessionCache, SessionIdResolver, SessionLaunch, SessionProvider, SessionRead,
    SessionRenamer, SessionRoot, SessionStorage, SessionStub, SessionTitle, Stored, SubagentFinder,
};
use crate::cli::DebugLevel;
use crate::clock::{Clock, TestClock};
use crate::error::{AppError, Result};
use crate::history::environment::test_support::waiting_operation;
use crate::history::load_meter::AgentMeter;
use crate::history::sqlite_cache::SqliteCache;
use crate::history::{Conversation, Environment, NextStep, ProcessIdentity, Source};
use runner::run_contract;
pub(crate) use runner::{every_record_sessions, registered_fixtures};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// One variant per contract test below, named after it.
#[derive(Clone, Copy, Debug)]
enum Contract {
    SubagentDiscovery,
    SubagentTranscripts,
    SubagentDelete,
    SubagentDeleteCount,
    SubagentsNotCapable,
    WarmCacheRename,
    SessionIdLookup,
    SessionIdLookupMatchesDiscovery,
    UnknownIdLookup,
    IdCaseRule,
    SessionIdLookupNotCapable,
    SubagentIdLookup,
    SubagentIdReadThroughCache,
    SubagentByIdNotCapable,
    QueryWithoutIdShape,
    ForkCommand,
    ForkNotCapable,
    WorkspaceSessionsDir,
    RootOverride,
    StorageEnvVars,
    UnreadableDirectory,
    ForeignFileDelete,
}

impl Contract {
    /// True when `provider`'s capability declarations turn the contract on.
    fn runs_for(self, provider: &dyn SessionProvider) -> bool {
        let subagents = provider.subagents().is_capable();
        let subagent_by_id = provider.subagent_by_id().is_capable();
        let session_id_lookup = provider.session_id_lookup().is_capable();
        let fork = provider.fork().is_capable();
        let rename = provider.rename().is_capable();
        let delete = provider.delete().is_capable();
        match self {
            Self::SubagentDiscovery | Self::SubagentTranscripts => subagents,
            Self::SubagentDelete | Self::SubagentDeleteCount => subagents && delete,
            Self::SubagentsNotCapable => !subagents,
            Self::SessionIdLookup
            | Self::SessionIdLookupMatchesDiscovery
            | Self::UnknownIdLookup
            | Self::IdCaseRule
            | Self::QueryWithoutIdShape => session_id_lookup,
            Self::SessionIdLookupNotCapable => !session_id_lookup,
            Self::SubagentIdLookup | Self::SubagentIdReadThroughCache => subagent_by_id,
            // A provider declaring `subagents` or `session_id_lookup` Not
            // Capable cannot resolve a sub-agent id; that capability's
            // declaration contract checks the provider.
            Self::SubagentByIdNotCapable => !subagent_by_id && subagents && session_id_lookup,
            Self::ForkCommand => fork,
            Self::ForkNotCapable => !fork,
            Self::WorkspaceSessionsDir => provider.workspace_sessions_dir().is_capable(),
            Self::WarmCacheRename => rename,
            Self::ForeignFileDelete => delete,
            Self::RootOverride | Self::StorageEnvVars | Self::UnreadableDirectory => true,
        }
    }
}

/// The argument `[resume].default_args` adds in the fork command contract.
/// Claude's fork alone passes it on.
pub(crate) const CONFIGURED_ARGUMENT: &str = "--claude-only";

/// A command a fixture expects its provider to build.
pub(crate) struct ExpectedCommand {
    pub(crate) program: &'static str,
    pub(crate) arguments: Vec<OsString>,
    /// `None` when the command runs in rearview's current directory.
    pub(crate) current_dir: Option<PathBuf>,
}

/// The case rule a provider compares session ids by.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdCase {
    /// Hex UUIDs: an id in another case names the same session.
    Insensitive,
    /// Ids drawn from a case-sensitive alphabet (OpenCode's base62): an id
    /// in another case is another id.
    Exact,
}

/// The ids a fixture writes sessions under, in the agent's id shape where it
/// has one.
pub(crate) struct FixtureIds {
    pub(crate) session: &'static str,
    pub(crate) other: &'static str,
    /// An id with the agent's shape that no fixture session uses.
    pub(crate) unknown: &'static str,
    /// `session` as a user might paste it in another case.
    pub(crate) session_in_other_case: &'static str,
}

/// One record kind in a fixture session and its marker text.
pub(crate) struct Marker {
    pub(crate) record: &'static str,
    pub(crate) text: &'static str,
    pub(crate) search: MarkerSearch,
    /// The summary-mode row of the run holding the marker's call; `None` for a
    /// marker outside a call, or in a `Tool::Other` call: such a call shows
    /// its input as JSON.
    pub(crate) summary_row: Option<&'static str>,
}

/// Search coverage of a [`Marker`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MarkerSearch {
    /// A searched marker: the session list's search and `agent search` both
    /// find it.
    ListAndAgent,
    /// An agent-searched marker: `agent search` finds it, and the session
    /// list's search does not.
    AgentOnly,
    /// The session list's search does not find the marker, and no test checks
    /// `agent search` for it.
    Unchecked,
}

impl Marker {
    /// A searched marker. Every output shows it.
    pub(crate) fn searched(record: &'static str, text: &'static str) -> Self {
        Self {
            record,
            text,
            search: MarkerSearch::ListAndAgent,
            summary_row: None,
        }
    }

    /// An agent-searched marker, as for a sub-agent's text or a call's
    /// header. Every output shows it.
    pub(crate) fn agent_searched(record: &'static str, text: &'static str) -> Self {
        Self {
            search: MarkerSearch::AgentOnly,
            ..Self::searched(record, text)
        }
    }

    /// A marker outside the session list's search, as for thinking or a call's
    /// body. Every output shows it.
    pub(crate) fn shown(record: &'static str, text: &'static str) -> Self {
        Self {
            search: MarkerSearch::Unchecked,
            ..Self::searched(record, text)
        }
    }

    /// Place the marker in a call whose run shows `row` in summary mode.
    pub(crate) fn in_summary_row(self, row: &'static str) -> Self {
        Self {
            summary_row: Some(row),
            ..self
        }
    }
}

/// Record kinds held in a fixture session and shown in no output, with each
/// one's marker text. Neither the session list's search nor exact
/// `agent search` finds them.
pub(crate) type HiddenMarkers = &'static [(&'static str, &'static str)];

/// A reply's marker and its message number in `agent read`.
pub(crate) struct NumberedReply {
    pub(crate) marker: &'static str,
    pub(crate) number: usize,
}

/// One agent's session with each of its record kinds, as a session list row,
/// and each record kind's marker text.
pub(crate) struct AgentSession {
    pub(crate) row: Conversation,
    pub(crate) markers: Vec<Marker>,
    pub(crate) hidden: HiddenMarkers,
    /// The reply after records without text, such as an empty tool result.
    /// The message-numbering coverage test reads it.
    pub(crate) reply_after_records_without_text: Option<NumberedReply>,
    /// The hand count of tokens in the session's sub-agent transcripts. The
    /// token-count coverage test reads it.
    pub(crate) subagent_tokens: u64,
    /// Keeps a fixture built on disk for the test alive.
    _storage: Option<tempfile::TempDir>,
}

impl AgentSession {
    pub(crate) fn new(
        source: Source,
        path: PathBuf,
        subagents: Vec<PathBuf>,
        markers: Vec<Marker>,
    ) -> Self {
        Self {
            row: list_row(source, &path, &subagents),
            markers,
            hidden: &[],
            reply_after_records_without_text: None,
            subagent_tokens: 0,
            _storage: None,
        }
    }

    /// Name the session's record kinds shown in no output.
    pub(crate) fn with_hidden(self, hidden: HiddenMarkers) -> Self {
        Self { hidden, ..self }
    }

    /// Name the reply after the session's records without text, and its
    /// message number in `agent read`.
    pub(crate) fn with_reply_after_records_without_text(
        self,
        marker: &'static str,
        number: usize,
    ) -> Self {
        Self {
            reply_after_records_without_text: Some(NumberedReply { marker, number }),
            ..self
        }
    }

    /// Set the hand count of tokens in the session's sub-agent transcripts.
    pub(crate) fn with_subagent_tokens(self, subagent_tokens: u64) -> Self {
        Self {
            subagent_tokens,
            ..self
        }
    }

    /// Keep `storage`, the directory the session was built in, alive with
    /// the session.
    pub(crate) fn kept_in(self, storage: tempfile::TempDir) -> Self {
        Self {
            _storage: Some(storage),
            ..self
        }
    }
}

/// Copies `fixture` under its own name into a new temporary directory and
/// appends `lines`. Returns the copy's path and the directory; keep the
/// directory alive while the copy is read.
pub(crate) fn fixture_with_lines(fixture: &Path, lines: &[String]) -> (PathBuf, tempfile::TempDir) {
    let storage = tempfile::tempdir().unwrap();
    let path = storage.path().join(fixture.file_name().unwrap());
    let mut text = std::fs::read_to_string(fixture).unwrap();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
    (path, storage)
}

/// Copies the directory `from` and everything in it to `to`.
pub(crate) fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Copies the file `from` to `to`, creating `to`'s directory.
pub(crate) fn copy_file(from: &Path, to: &Path) {
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::copy(from, to).unwrap();
}

/// The session list's row for the session at `locator`, read with its
/// sub-agent transcripts `subagents`.
pub(crate) fn list_row(source: Source, locator: &Path, subagents: &[PathBuf]) -> Conversation {
    let stub = SessionStub {
        locator: locator.to_owned(),
        subagents: subagents.to_vec(),
        cache_key: String::new(),
        fingerprint: Fingerprint {
            size: 0,
            modified: None,
        },
    };
    let root = SessionRoot::new(locator.parent().unwrap());
    source
        .provider()
        .storage()
        .parse_session(
            crate::history::FOR_FIXTURES,
            &stub,
            &root,
            None,
            &|| {},
            &TestClock::default(),
        )
        .unwrap()
        .expect("the session is listed")
}

/// One provider's sessions, written under a temporary home and read,
/// resolved and deleted there. Every method takes the home as a parameter:
/// the providers read their roots from the environment, which a parallel
/// test run cannot set.
///
/// A default that calls [`cannot_run`](Self::cannot_run) fails the contract
/// that calls it: the fixture overrides it, or its provider declares the
/// capability that contract needs Not Capable.
pub(crate) trait ProviderFixture: Sync {
    fn provider(&self) -> &'static dyn SessionProvider;

    fn ids(&self) -> FixtureIds;

    /// The agent's case rule for ids.
    fn id_case(&self) -> IdCase;

    /// One listable session; returns its locator.
    fn write_session(&self, home: &Path, session: &str) -> PathBuf;

    /// One session with every record kind this agent writes, read by the
    /// display, search, message-numbering and token-count coverage tests. Add
    /// a record kind the agent starts writing to the session, and to its
    /// markers or, when no output shows it, to its hidden markers.
    fn every_record_session(&self) -> AgentSession;

    /// Copies `session`, [`every_record_session`](Self::every_record_session)'s
    /// session, under `home` where discovery under
    /// [`root_under`](Self::root_under)`(home)` finds it with its sub-agent
    /// transcripts. The output contract reads it there.
    fn write_every_record_session(&self, _home: &Path, _session: &AgentSession) {
        self.cannot_run("write its every-record session where discovery finds it")
    }

    /// A sub-agent transcript of `session`; returns its locator.
    fn write_subagent(&self, _home: &Path, _session: &str) -> PathBuf {
        self.cannot_run("record a sub-agent")
    }

    /// A sub-agent transcript of the sub-agent
    /// [`write_subagent`](Self::write_subagent) writes for `session`; returns
    /// its locator.
    fn write_nested_subagent(&self, _home: &Path, _session: &str) -> PathBuf {
        self.cannot_run("record a sub-agent's sub-agent")
    }

    /// The root under `home` that the provider reads its sessions from.
    fn root_under(&self, home: &Path) -> SessionRoot;

    /// Discovery under `home`, through the provider's storage.
    fn discover_under(&self, home: &Path) -> Vec<SessionStub> {
        self.discover_listed_by(home, &SystemLister).unwrap().stubs
    }

    /// Discovery under `home`, through the provider's storage, listing every
    /// directory through `lister`.
    fn discover_listed_by(
        &self,
        home: &Path,
        lister: &dyn DirectoryLister,
    ) -> Result<DiscoveredSessions> {
        self.storage().discover(&self.root_under(home), lister)
    }

    /// One listable session in another directory than
    /// [`write_session`](Self::write_session)'s, one discovery lists apart
    /// from it; returns its locator.
    fn write_session_in_another_directory(&self, _home: &Path, _session: &str) -> PathBuf {
        self.cannot_run("write a session in another directory")
    }

    /// The directory discovery lists to find the session at `locator`;
    /// `None` for an agent whose discovery lists no directory.
    fn listed_directory_of(&self, locator: &Path) -> Option<PathBuf> {
        locator.parent().map(Path::to_path_buf)
    }

    /// The session `id` names under `home`, as the provider's
    /// [`SessionIdResolver::resolve_session_id_in`] answers with its root
    /// pinned there.
    fn resolve_under(&self, _home: &Path, _id: &str) -> Result<Option<ResolvedSession>> {
        self.cannot_run("resolve a session id")
    }

    /// The provider's session-id resolver.
    fn session_id_resolver(&self) -> &'static dyn SessionIdResolver {
        match self.provider().session_id_lookup() {
            Capability::Capable(resolver) => resolver,
            Capability::NotCapable(_) => self.cannot_run("resolve a session id"),
        }
    }

    fn renamer(&self) -> &'static dyn SessionRenamer {
        match self.provider().rename() {
            Capability::Capable(renamer) => renamer,
            Capability::NotCapable(_) => self.cannot_run("rename a session"),
        }
    }

    /// The provider's fork command for `launch`, a session under `home`.
    fn fork_under(&self, _home: &Path, launch: &SessionLaunch) -> Result<Command> {
        match self.provider().fork() {
            Capability::Capable(forker) => forker.fork_command(launch),
            Capability::NotCapable(_) => self.cannot_run("fork a session"),
        }
    }

    /// The command that forks the session written for `ids().session` at
    /// `locator`, launched with [`CONFIGURED_ARGUMENT`].
    fn expected_fork(&self, _locator: &Path) -> ExpectedCommand {
        self.cannot_run("state its fork command")
    }

    /// A working directory. [`write_session`](Self::write_session) writes
    /// into the sessions directory the agent names for it.
    fn session_working_dir(&self) -> PathBuf {
        self.cannot_run("name a working directory for its sessions")
    }

    /// The environment variables that point the provider's root at
    /// [`root_under`](Self::root_under)`(home)`.
    fn root_variables(&self, _home: &Path) -> Vec<(&'static str, OsString)> {
        self.cannot_run("point its root at a home through the environment")
    }

    /// Every session `id` names under `home`, as the provider's
    /// [`SessionProvider::find_sessions_by_id`] answers with its root pinned
    /// there.
    fn find_under(&self, _home: &Path, _id: &str) -> Result<Vec<PathBuf>> {
        self.cannot_run("search its sessions by id")
    }

    /// The provider's sub-agent finder.
    fn subagent_finder(&self) -> &'static dyn SubagentFinder {
        match self.provider().subagents() {
            Capability::Capable(finder) => finder,
            Capability::NotCapable(_) => self.cannot_run("name a session's sub-agents"),
        }
    }

    /// The provider's delete of the session at `locator` under `home`.
    fn delete_under(&self, _home: &Path, locator: &Path) -> Result<Deleted> {
        match self.provider().delete() {
            Capability::Capable(deleter) => {
                deleter.delete_session(crate::history::FOR_FIXTURES, locator)
            }
            Capability::NotCapable(_) => self.cannot_run("delete a session"),
        }
    }

    /// True while the agent still stores anything of the session at
    /// `locator`.
    fn is_stored(&self, _home: &Path, locator: &Path) -> bool {
        locator.exists()
    }

    /// The id of the sub-agent [`write_subagent`](Self::write_subagent)
    /// writes for `session`.
    fn subagent_id(&self, _session: &str) -> String {
        self.cannot_run("name a sub-agent's id")
    }

    /// The roots for an override value and a home.
    fn roots_from(&self, _override_dir: Option<&str>, _home: &Path) -> Vec<PathBuf> {
        self.cannot_run("resolve its roots from an override and a home")
    }

    /// A transcript another agent wrote, copied into `directory` under the
    /// name this agent gives its own.
    fn foreign_transcript(&self, directory: &Path) -> PathBuf {
        let (fixture, name) = match self.provider().source() {
            Source::Pi | Source::Omp => ("codex/rollout.jsonl", "session.jsonl"),
            Source::Kimi => ("pi/v3-branched.jsonl", "wire.jsonl"),
            _ => ("pi/v3-branched.jsonl", "session.jsonl"),
        };
        let path = directory.join(name);
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(fixture),
            &path,
        )
        .unwrap();
        path
    }

    fn storage(&self) -> &'static dyn SessionStorage {
        self.provider().storage()
    }

    /// Fails the running contract: the fixture cannot do `what`, and the
    /// provider's declarations turn the contract on.
    fn cannot_run(&self, what: &str) -> ! {
        panic!(
            "the {} fixture cannot {what}: implement it, or change the declaration \
             that turns this contract on",
            self.provider().labels().name
        )
    }
}

/// The fixture list and the one runner over it. The list is private to this
/// module: a contract cannot loop over the fixtures itself and skip the
/// providers' declarations.
mod runner {
    use super::super::{SessionProvider, providers};
    use super::{AgentSession, Contract, ProviderFixture};
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

    /// Every provider's fixture, in registration order.
    static FIXTURES: &[&dyn ProviderFixture] = &[
        &super::super::claude::tests::ClaudeFixture,
        &super::super::codex::tests::CodexFixture,
        &super::super::opencode::tests::OpenCodeFixture,
        &super::super::kimi::tests::KimiFixture,
        &super::super::grok::tests::GrokFixture,
        &super::super::pi::tests::PiFixture,
        &super::super::omp::tests::OmpFixture,
    ];

    /// Fails for a provider without a fixture.
    fn fixture_of(provider: &dyn SessionProvider) -> &'static dyn ProviderFixture {
        FIXTURES
            .iter()
            .copied()
            .find(|fixture| fixture.provider().source() == provider.source())
            .unwrap_or_else(|| {
                panic!(
                    "{} has no fixture: give it one in FIXTURES",
                    provider.labels().name
                )
            })
    }

    /// Runs `contract` over every registered provider whose declarations turn
    /// it on, naming the provider when the contract fails for it.
    pub(super) fn run_contract(contract: Contract, body: impl Fn(&'static dyn ProviderFixture)) {
        let mut ran = false;
        for provider in providers() {
            if !contract.runs_for(*provider) {
                continue;
            }
            ran = true;
            let fixture = fixture_of(*provider);
            if let Err(failure) = catch_unwind(AssertUnwindSafe(|| body(fixture))) {
                eprintln!(
                    "{contract:?} failed for provider {}",
                    provider.labels().name
                );
                resume_unwind(failure);
            }
        }
        assert!(
            ran,
            "no provider's declarations turn {contract:?} on, so it checks nothing"
        );
    }

    /// Every registered provider's every-record session, in registration
    /// order.
    pub(crate) fn every_record_sessions() -> Vec<AgentSession> {
        registered_fixtures()
            .into_iter()
            .map(|fixture| fixture.every_record_session())
            .collect()
    }

    /// Every registered provider's fixture, in registration order.
    pub(crate) fn registered_fixtures() -> Vec<&'static dyn ProviderFixture> {
        providers()
            .iter()
            .map(|provider| fixture_of(*provider))
            .collect()
    }

    #[test]
    fn every_registered_provider_has_one_fixture() {
        let without_one: Vec<_> = providers()
            .iter()
            .filter(|provider| {
                FIXTURES
                    .iter()
                    .filter(|fixture| fixture.provider().source() == provider.source())
                    .count()
                    != 1
            })
            .map(|provider| provider.labels().name)
            .collect();

        assert!(
            without_one.is_empty(),
            "providers without exactly one fixture in FIXTURES: {without_one:?}"
        );
    }
}

/// Every conversation under `home`, through the shared load loop with its
/// session cache database under `cache_base`.
pub(crate) fn load_under(
    fixture: &'static dyn ProviderFixture,
    home: &Path,
    cache_base: &Path,
) -> Vec<Conversation> {
    rows_loaded_through(&fixture_environment(fixture, home, cache_base))
}

/// Every conversation `environment`'s one storage holds, through the shared
/// load loop.
fn rows_loaded_through(environment: &Environment) -> Vec<Conversation> {
    let operation = waiting_operation(environment);
    let rows = super::load_sessions(
        environment,
        environment.storages()[0],
        false,
        None,
        AgentMeter::UNMETERED,
        super::LeftOutRows::NONE,
        &operation,
        &mut |_, _| {},
    )
    .unwrap()
    .conversations;
    operation.finish().unwrap();
    rows
}

/// `fixture`'s storage pinned to `home`, alone, with its session cache under
/// `cache_base`, on test time.
pub(crate) fn fixture_environment(
    fixture: &'static dyn ProviderFixture,
    home: &Path,
    cache_base: &Path,
) -> Environment {
    fixture_environment_listed_by(fixture, home, cache_base, Arc::new(SystemLister))
}

/// [`fixture_environment`], listing every directory through `lister`.
fn fixture_environment_listed_by(
    fixture: &'static dyn ProviderFixture,
    home: &Path,
    cache_base: &Path,
    lister: Arc<dyn DirectoryLister>,
) -> Environment {
    Environment::new(
        vec![kept_storage_under(fixture, home)],
        cache_base.to_path_buf(),
        ProcessIdentity::default(),
        Arc::new(TestClock::default()),
        lister,
    )
}

/// The provider's storage pinned to `home`, with discovery answered by the
/// fixture's `discover_listed_by`: Pi's and OMP's own discovery reads its
/// walk depth from the environment.
struct FixtureStorage<'a> {
    fixture: &'a dyn ProviderFixture,
    home: &'a Path,
}

impl SessionStorage for FixtureStorage<'_> {
    fn source(&self) -> Source {
        self.fixture.storage().source()
    }

    fn cache(&self) -> SessionCache {
        self.fixture.storage().cache()
    }

    fn roots_in(&self, _environment: &RootEnvironment) -> Result<Vec<SessionRoot>> {
        Ok(vec![self.fixture.root_under(self.home)])
    }

    fn discover(
        &self,
        _root: &SessionRoot,
        lister: &dyn DirectoryLister,
    ) -> Result<DiscoveredSessions> {
        self.fixture.discover_listed_by(self.home, lister)
    }

    fn rediscover_unlisted_session(
        &self,
        root: &SessionRoot,
        locator: &Path,
        lister: &dyn DirectoryLister,
    ) -> Result<Option<SessionStub>> {
        self.fixture
            .storage()
            .rediscover_unlisted_session(root, locator, lister)
    }

    fn parse_session(
        &self,
        permit: crate::history::ParsePermit<'_>,
        stub: &SessionStub,
        root: &SessionRoot,
        debug_level: Option<DebugLevel>,
        on_transcript_read: &(dyn Fn() + Sync),
        clock: &dyn Clock,
    ) -> Result<crate::history::parser::ParsedSession> {
        self.fixture.storage().parse_session(
            permit,
            stub,
            root,
            debug_level,
            on_transcript_read,
            clock,
        )
    }

    fn external_titles(&self, root: &SessionRoot) -> HashMap<String, SessionTitle> {
        self.fixture.storage().external_titles(root)
    }
}

/// `fixture`'s storage pinned to `home`, kept for the rest of the process as
/// the first build keeps the storages it samples.
pub(crate) fn kept_storage_under(
    fixture: &'static dyn ProviderFixture,
    home: &Path,
) -> &'static dyn SessionStorage {
    let home: &'static Path = Box::leak(home.to_path_buf().into_boxed_path());
    Box::leak(Box::new(FixtureStorage { fixture, home }))
}

fn sorted(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths
}

/// The fixture's session, a sub-agent of it, a sub-agent of that one, and an
/// unrelated session, under `home`.
struct SessionFamily {
    session: PathBuf,
    /// The sub-agent, then the sub-agent it ran.
    subagents: Vec<PathBuf>,
    other: PathBuf,
}

impl SessionFamily {
    fn write(fixture: &dyn ProviderFixture, home: &Path) -> Self {
        let ids = fixture.ids();
        let session = fixture.write_session(home, ids.session);
        let subagents = vec![
            fixture.write_subagent(home, ids.session),
            fixture.write_nested_subagent(home, ids.session),
        ];
        let other = fixture.write_session(home, ids.other);
        Self {
            session,
            subagents,
            other,
        }
    }
}

/// One stub per session, its sub-agent transcripts named on it, nested ones
/// flattened; a sub-agent never lists as a session of its own.
#[test]
fn discovery_names_each_sessions_sub_agent_transcripts() {
    run_contract(Contract::SubagentDiscovery, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let family = SessionFamily::write(fixture, home.path());

        let mut stubs = fixture.discover_under(home.path());
        stubs.sort_by(|left, right| left.locator.cmp(&right.locator));

        assert_eq!(
            stubs
                .iter()
                .map(|stub| (stub.locator.clone(), sorted(stub.subagents.clone())))
                .collect::<Vec<_>>(),
            sorted(vec![family.session.clone(), family.other.clone()])
                .into_iter()
                .map(|locator| {
                    let subagents = if locator == family.session {
                        sorted(family.subagents.clone())
                    } else {
                        Vec::new()
                    };
                    (locator, subagents)
                })
                .collect::<Vec<_>>()
        );
    });
}

/// `--render` and `rearview <file>` read sub-agents through
/// `subagent_transcripts`, not through discovery.
#[test]
fn subagent_transcripts_returns_each_sub_agent_of_session() {
    run_contract(Contract::SubagentTranscripts, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let family = SessionFamily::write(fixture, home.path());

        let named = fixture
            .subagent_finder()
            .subagent_transcripts(&family.session)
            .unwrap();

        assert_eq!(sorted(named), sorted(family.subagents));
    });
}

/// A sub-agent session left behind would list as a session the user never
/// started, under an id they never saw.
#[test]
fn delete_removes_every_sub_agent_session_with_session() {
    run_contract(Contract::SubagentDelete, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let family = SessionFamily::write(fixture, home.path());

        let deleted = fixture.delete_under(home.path(), &family.session).unwrap();

        assert!(
            matches!(deleted.stored, Stored::SessionFiles(1) | Stored::Copies(1)),
            "{deleted:?}"
        );
        assert!(!fixture.is_stored(home.path(), &family.session));
        for subagent in &family.subagents {
            assert!(
                !fixture.is_stored(home.path(), subagent),
                "{} survived its session's delete",
                subagent.display()
            );
        }
        assert!(fixture.is_stored(home.path(), &family.other));
    });
}

/// The delete report names the sub-agent sessions a delete removed beside
/// the one the user named.
#[test]
fn delete_counts_sub_agent_sessions_it_removes() {
    run_contract(Contract::SubagentDeleteCount, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let family = SessionFamily::write(fixture, home.path());

        let deleted = fixture.delete_under(home.path(), &family.session).unwrap();

        assert_eq!(deleted.subagent_sessions, family.subagents.len());
    });
}

/// Discovery runs through [`ProviderFixture::discover_under`]. Where the
/// provider's own discovery reads its walk depth from the environment, as
/// Pi's does, the fixture copies the walk, and this contract does not run the
/// provider's discovery.
#[test]
fn provider_declaring_subagents_not_capable_doesnt_report_sub_agent() {
    run_contract(Contract::SubagentsNotCapable, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let ids = fixture.ids();
        let session = fixture.write_session(home.path(), ids.session);
        let other = fixture.write_session(home.path(), ids.other);

        let mut stubs = fixture.discover_under(home.path());
        stubs.sort_by(|left, right| left.locator.cmp(&right.locator));
        assert_eq!(
            stubs
                .iter()
                .map(|stub| (stub.locator.clone(), stub.subagents.clone()))
                .collect::<Vec<_>>(),
            sorted(vec![session.clone(), other])
                .into_iter()
                .map(|locator| (locator, Vec::new()))
                .collect::<Vec<_>>()
        );
        if fixture.provider().delete().is_capable() {
            let deleted = fixture.delete_under(home.path(), &session).unwrap();
            assert_eq!(deleted.subagent_sessions, 0);
        }
    });
}

/// A rename the cache's change detector cannot see, such as a rewritten
/// sidecar or index, still reaches the next load: a warm load would
/// otherwise restore the old name from the cache indefinitely.
#[test]
fn rename_reaches_next_load_through_warm_cache() {
    run_contract(Contract::WarmCacheRename, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let clock = TestClock::default();
        let session = fixture.write_session(home.path(), fixture.ids().session);
        fixture
            .renamer()
            .rename_session(
                crate::history::FOR_FIXTURES,
                &session,
                "old name",
                clock.wall(),
            )
            .unwrap();

        let first = load_under(fixture, home.path(), cache.path());
        assert_eq!(first[0].custom_title.as_deref(), Some("old name"));

        clock.advance(std::time::Duration::from_secs(1));
        fixture
            .renamer()
            .rename_session(
                crate::history::FOR_FIXTURES,
                &session,
                "fresh name",
                clock.wall(),
            )
            .unwrap();

        let second = load_under(fixture, home.path(), cache.path());
        assert_eq!(second[0].custom_title.as_deref(), Some("fresh name"));
    });
}

#[test]
fn session_id_resolves_to_its_session_with_its_sub_agents() {
    run_contract(Contract::SessionIdLookup, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let family = SessionFamily::write(fixture, home.path());

        let resolved = fixture
            .resolve_under(home.path(), fixture.ids().session)
            .unwrap()
            .unwrap();

        assert_eq!(resolved.stub.locator, family.session);
        assert_eq!(
            sorted(resolved.stub.subagents.clone()),
            sorted(family.subagents)
        );
    });
}

/// The stub matches discovery's, fingerprint included.
#[test]
fn session_id_resolves_to_stub_discovery_lists() {
    run_contract(Contract::SessionIdLookupMatchesDiscovery, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let family = SessionFamily::write(fixture, home.path());

        let resolved = fixture
            .resolve_under(home.path(), fixture.ids().session)
            .unwrap()
            .unwrap();
        let listed = fixture
            .discover_under(home.path())
            .into_iter()
            .find(|stub| stub.locator == family.session)
            .unwrap();

        assert_eq!(resolved.stub.fingerprint, listed.fingerprint);
    });
}

/// Asserts the shape first: the miss must come from the lookup, not from
/// rejecting the query as text.
#[test]
fn id_provider_never_recorded_resolves_to_nothing() {
    run_contract(Contract::UnknownIdLookup, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let ids = fixture.ids();
        fixture.write_session(home.path(), ids.session);

        assert!(
            fixture
                .session_id_resolver()
                .is_session_id_shape(ids.unknown)
        );
        assert_eq!(
            fixture.resolve_under(home.path(), ids.unknown).unwrap(),
            None
        );
    });
}

/// A paste in another case names the same session when the agent writes
/// hex UUIDs, and another session when its ids are case-sensitive.
#[test]
fn id_in_another_case_resolves_by_agents_case_rule() {
    run_contract(Contract::IdCaseRule, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let ids = fixture.ids();
        let session = fixture.write_session(home.path(), ids.session);
        let resolve = |id| {
            fixture
                .resolve_under(home.path(), id)
                .unwrap()
                .map(|resolved| resolved.stub.locator)
        };
        assert_eq!(resolve(ids.session), Some(session.clone()));

        let resolved = resolve(ids.session_in_other_case);

        match fixture.id_case() {
            IdCase::Insensitive => assert_eq!(resolved, Some(session)),
            IdCase::Exact => assert_eq!(resolved, None),
        }
    });
}

/// Searches by the id the list shows, not the name the fixture writes the
/// session under: a transcript can state an id of its own.
#[test]
fn provider_declaring_session_id_lookup_not_capable_finds_sessions_by_id_only_through_its_search() {
    run_contract(Contract::SessionIdLookupNotCapable, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let session = fixture.write_session(home.path(), fixture.ids().session);
        let listed = load_under(fixture, home.path(), cache.path());
        let asked: Vec<Source> = super::session_id_resolvers()
            .map(|(source, _)| source)
            .collect();

        assert!(
            !asked.contains(&fixture.provider().source()),
            "the registry asks {asked:?}"
        );
        assert_eq!(
            fixture
                .find_under(home.path(), &listed[0].session_id)
                .unwrap(),
            vec![session]
        );
    });
}

/// The one exception to every filter the list applies: a sub-agent's id
/// opens it as a session of its own, with the sub-agents beneath it.
#[test]
fn sub_agent_id_resolves_to_stub_of_its_own() {
    run_contract(Contract::SubagentIdLookup, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let ids = fixture.ids();
        let family = SessionFamily::write(fixture, home.path());

        let resolved = fixture
            .resolve_under(home.path(), &fixture.subagent_id(ids.session))
            .unwrap()
            .unwrap();

        assert_eq!(resolved.stub.locator, family.subagents[0]);
        assert_eq!(resolved.stub.subagents, family.subagents[1..].to_vec());
    });
}

/// True when `fixture`'s storage parses the sub-agent `resolved` names as a
/// listed session; false for an empty one.
fn subagent_lists(fixture: &dyn ProviderFixture, resolved: &ResolvedSession) -> bool {
    fixture
        .storage()
        .parse_session(
            crate::history::FOR_FIXTURES,
            &resolved.stub,
            &resolved.root,
            None,
            &|| {},
            &crate::clock::TestClock::default(),
        )
        .unwrap()
        .is_ok()
}

/// True when `read` is the read of a sub-agent whose parse lists it when
/// `lists` is true, and finds it empty otherwise.
fn reads_as_parsed(read: &SessionRead, lists: bool) -> bool {
    matches!(
        (lists, read),
        (true, SessionRead::Listed(_)) | (false, SessionRead::Empty)
    )
}

/// A pasted sub-agent ID the cache lacks reads as not cached and asks for a
/// session refresh, which reads it as the sub-agent's parsed session.
#[test]
fn sub_agent_id_cache_lacks_reads_as_its_parsed_session_through_session_refresh() {
    run_contract(Contract::SubagentIdReadThroughCache, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        let ids = fixture.ids();
        fixture.write_session(home.path(), ids.session);
        fixture.write_subagent(home.path(), ids.session);
        let resolved = fixture
            .resolve_under(home.path(), &fixture.subagent_id(ids.session))
            .unwrap()
            .unwrap();
        let lists = subagent_lists(fixture, &resolved);
        let environment = fixture_environment(fixture, home.path(), cache_base.path());
        let source = fixture.provider().source();
        let locator = resolved.stub.locator.clone();

        let (read, next) =
            super::read_resolved_session(&environment, source, resolved, false).into_parts();

        assert!(matches!(read, SessionRead::NotCached));
        assert_eq!(next, NextStep::CacheInBackground);
        let operation = waiting_operation(&environment);
        let (refreshed, _) = super::cache_sessions(
            &environment,
            source,
            &[&locator],
            false,
            &operation,
            AgentMeter::UNMETERED,
        )
        .unwrap()
        .pop()
        .unwrap()
        .into_parts();
        operation.finish().unwrap();
        assert!(
            reads_as_parsed(&refreshed, lists),
            "the sub-agent reads as its parsed session"
        );
    });
}

/// A sub-agent opened by its ID during a refresh waits for it, then a
/// session refresh reads it through the provider's lookup of a session
/// discovery leaves out, and the cache serves it after. Otherwise the
/// completed refresh removes the sub-agent's row.
#[test]
fn sub_agent_opened_by_id_during_refresh_is_read_again_once_it_completes() {
    run_contract(Contract::SubagentIdReadThroughCache, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        let ids = fixture.ids();
        fixture.write_session(home.path(), ids.session);
        fixture.write_subagent(home.path(), ids.session);
        let resolved = fixture
            .resolve_under(home.path(), &fixture.subagent_id(ids.session))
            .unwrap()
            .unwrap();
        let lists = subagent_lists(fixture, &resolved);
        let environment = fixture_environment(fixture, home.path(), cache_base.path());
        let source = fixture.provider().source();
        let refresh = crate::history::sqlite_cache::refresh::RefreshOwnership::acquire_for_test(
            environment.cache_base(),
        )
        .unwrap();

        let (_, opened) =
            super::read_resolved_session(&environment, source, resolved.clone(), false)
                .into_parts();
        assert!(
            matches!(opened, NextStep::AwaitRefresh(_)),
            "the sub-agent's read records the refresh to await"
        );
        drop(refresh);
        let locator = resolved.stub.locator.as_path();
        let operation = waiting_operation(&environment);
        let cached = super::cache_sessions(
            &environment,
            source,
            &[locator],
            false,
            &operation,
            AgentMeter::UNMETERED,
        )
        .unwrap()
        .pop()
        .unwrap();
        operation.finish().unwrap();
        let reread = super::reread_sessions(&environment, source, &[locator], false)
            .pop()
            .unwrap();

        for (read_again, next) in [cached.into_parts(), reread.into_parts()] {
            assert!(
                reads_as_parsed(&read_again, lists),
                "the sub-agent is read again after the refresh"
            );
            assert_eq!(next, NextStep::Settled);
        }
    });
}

/// Asserts first that the session's id resolves to a stub naming the
/// sub-agent: the miss must be a refusal, not a sub-agent the fixture failed
/// to write.
#[test]
fn provider_declaring_subagent_by_id_not_capable_doesnt_resolve_sub_agent_id() {
    run_contract(Contract::SubagentByIdNotCapable, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let ids = fixture.ids();
        let family = SessionFamily::write(fixture, home.path());
        let session = fixture
            .resolve_under(home.path(), ids.session)
            .unwrap()
            .unwrap();
        assert!(
            session.stub.subagents.contains(&family.subagents[0]),
            "the session's stub does not name the sub-agent {}: {:?}",
            family.subagents[0].display(),
            session.stub.subagents
        );

        let resolved = fixture
            .resolve_under(home.path(), &fixture.subagent_id(ids.session))
            .unwrap();

        assert_eq!(resolved, None);
    });
}

/// Text typed into the search box resolves to no session.
#[test]
fn query_without_id_shape_resolves_to_nothing() {
    run_contract(Contract::QueryWithoutIdShape, |fixture| {
        let resolver = fixture.session_id_resolver();
        assert!(!resolver.is_session_id_shape("deployment"));
        assert_eq!(
            super::resolve_in_storage(resolver, fixture.storage(), "deployment").unwrap(),
            None
        );
    });
}

/// True when the actions menu offers Fork on a row of `fixture`'s agent, once
/// the agent's `fork` declaration has been read.
fn actions_menu_offers_fork(fixture: &dyn ProviderFixture) -> bool {
    fixture.provider().fork();
    let menu = crate::tui::claude_home::app_reading(
        vec![fixture.every_record_session().row],
        crate::history::environment::test_support::test_environment(Vec::new(), None),
    );
    menu.is_action_available(crate::tui::SessionAction::Fork)
}

/// The actions menu offers Fork, and the fork starts the agent on the session
/// the user picked.
#[test]
fn fork_command_starts_agent_on_session() {
    run_contract(Contract::ForkCommand, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let recorded_project = tempfile::tempdir().unwrap();
        let session = fixture.write_session(home.path(), fixture.ids().session);
        let launch = SessionLaunch {
            path: &session,
            project_path: Some(recorded_project.path()),
            configured_args: &[CONFIGURED_ARGUMENT.to_owned()],
        };

        let fork = fixture.fork_under(home.path(), &launch).unwrap();

        let expected = fixture.expected_fork(&session);
        assert_eq!(fork.get_program(), expected.program);
        assert_eq!(
            fork.get_args().collect::<Vec<_>>(),
            expected
                .arguments
                .iter()
                .map(OsString::as_os_str)
                .collect::<Vec<_>>()
        );
        assert_eq!(fork.get_current_dir(), expected.current_dir.as_deref());
        assert!(actions_menu_offers_fork(fixture));
    });
}

/// The actions menu does not offer Fork, and `--fork-session` refuses with
/// the agent's name and builds no command.
#[test]
fn provider_declaring_fork_not_capable_refuses_fork_session() {
    run_contract(Contract::ForkNotCapable, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let session = fixture.write_session(home.path(), fixture.ids().session);
        let provider = fixture.provider();
        let launch = SessionLaunch {
            path: &session,
            project_path: None,
            configured_args: &[],
        };

        let refusal = super::launch_command(provider.source(), &launch, true).unwrap_err();

        assert!(
            matches!(
                refusal,
                AppError::NotCapable {
                    agent,
                    operation: Operation::Fork,
                    detail: Some(DenialDetail::ResumeThenForkInside),
                } if agent == provider.labels().display
            ),
            "{refusal:?}"
        );
        assert!(!actions_menu_offers_fork(fixture));
    });
}

/// The lookup `--show-dir` runs names the directory the agent writes a
/// working directory's sessions to: the one directory under the root that
/// holds them, named as the fixture names it by hand.
#[test]
fn working_directory_maps_to_directory_holding_its_sessions() {
    run_contract(Contract::WorkspaceSessionsDir, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let session = fixture.write_session(home.path(), fixture.ids().session);
        let variables = fixture.root_variables(home.path());
        let variable = |name: &str| {
            variables
                .iter()
                .find(|(listed, _)| *listed == name)
                .map(|(_, value)| value.clone())
        };
        let environment = RootEnvironment::new(&variable, Some(home.path().to_path_buf()), None);
        let Capability::Capable(sessions_dir_of) = fixture.provider().workspace_sessions_dir()
        else {
            panic!("the contract runs where `workspace_sessions_dir` is Capable");
        };

        let sessions_dir = sessions_dir_of(&environment, &fixture.session_working_dir()).unwrap();

        assert_eq!(
            sessions_dir.parent(),
            Some(fixture.root_under(home.path()).path.as_path())
        );
        assert!(
            session.starts_with(&sessions_dir),
            "{} is not in {}",
            session.display(),
            sessions_dir.display()
        );
    });
}

/// The roots sit under the agent's home, an override replaces them, and an
/// empty override means unset.
#[test]
fn sessions_root_is_agents_home_or_its_override() {
    run_contract(Contract::RootOverride, |fixture| {
        // A temporary directory is absolute on Windows too, where `/opt/agent`
        // is not.
        let directory = tempfile::tempdir().unwrap();
        let home = &directory.path().join("home");
        let overridden_home = &directory.path().join("agent");

        let defaults = fixture.roots_from(None, home);
        assert!(!defaults.is_empty());
        assert!(
            defaults.iter().all(|root| root.starts_with(home)),
            "{defaults:?}"
        );

        let overridden = fixture.roots_from(Some(&overridden_home.to_string_lossy()), home);
        assert!(!overridden.is_empty());
        assert!(
            overridden
                .iter()
                .all(|root| root.starts_with(overridden_home)),
            "the override replaces every default: {overridden:?}"
        );

        assert_eq!(
            fixture.roots_from(Some(""), home),
            defaults,
            "an empty override means unset"
        );
    });
}

/// Root resolution reads exactly the variables the provider lists, and with
/// each of them pointing into one directory, every root lies inside it.
#[test]
fn listed_environment_variables_decide_every_root() {
    run_contract(Contract::StorageEnvVars, |fixture| {
        let provider = fixture.provider();
        let listed: BTreeSet<&str> = provider.storage_env_vars().iter().copied().collect();
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let current_dir = directory.path().join("current");
        let redirected = directory.path().join("redirected");
        let read = RefCell::new(BTreeSet::new());
        let resolve = |variable: &dyn Fn(&str) -> Option<OsString>| {
            let recording = |name: &str| {
                read.borrow_mut().insert(name.to_owned());
                variable(name)
            };
            let environment =
                RootEnvironment::new(&recording, Some(home.clone()), Some(current_dir.clone()));
            provider.storage().roots_in(&environment).unwrap()
        };

        let roots = resolve(&|name| {
            listed
                .contains(name)
                .then(|| redirected.join(name).into_os_string())
        });
        assert!(!roots.is_empty());
        assert!(
            roots.iter().all(|root| root.path.starts_with(&redirected)),
            "a root does not follow the listed variables: {roots:?}"
        );
        resolve(&|_| None);

        let read: BTreeSet<String> = read.into_inner();
        assert_eq!(
            read.iter().map(String::as_str).collect::<BTreeSet<_>>(),
            listed,
            "the variables root resolution reads"
        );
    });
}

/// A directory discovery cannot list hides no session: the other
/// directories still list, discovery reports the directory, and the cached
/// sessions under it stay listed from the session cache, by the list and by
/// agent `sessions`, which names the directory in one `io` warning.
#[test]
fn unreadable_directory_keeps_its_cached_sessions_listed() {
    run_contract(Contract::UnreadableDirectory, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let hidden = fixture.write_session(home.path(), fixture.ids().session);
        let other = fixture.write_session_in_another_directory(home.path(), fixture.ids().other);
        let lister = Arc::new(ScriptedLister::default());
        let environment =
            fixture_environment_listed_by(fixture, home.path(), cache.path(), lister.clone());
        let root = fixture.root_under(home.path());
        let listed_before = listed_paths(&environment);
        assert_eq!(listed_before, sorted(vec![hidden.clone(), other.clone()]));
        let cached_before = cached_keys(&environment, &root);
        assert_eq!(
            cached_before.len(),
            2,
            "the first load cached both sessions"
        );
        let refused = fixture.listed_directory_of(&hidden);
        if let Some(refused) = &refused {
            lister.refuse(refused);
        }

        let listed = listed_paths(&environment);
        let found = environment.storages()[0]
            .discover(&root, lister.as_ref())
            .unwrap();

        assert_eq!(
            listed, listed_before,
            "the sessions under the unreadable directory stay listed"
        );
        assert_eq!(
            found
                .unreadable_directories
                .into_iter()
                .map(|directory| directory.path)
                .collect::<Vec<_>>(),
            Vec::from_iter(refused.clone()),
            "discovery reports the directory it could not list"
        );
        assert_eq!(
            cached_keys(&environment, &root),
            cached_before,
            "the cache keeps the rows of the sessions under the directory"
        );

        let output = crate::agent::service::execute(
            crate::agent::test_support::agent_command(&["sessions", "--all"]),
            fixture_environment_listed_by(fixture, home.path(), cache.path(), lister),
        )
        .unwrap_or_else(crate::error_report);
        let directory_warnings = output
            .lines()
            .filter(|line| {
                line.starts_with("protocol agent-warning kind=io ")
                    && refused.as_ref().is_some_and(|refused| {
                        line.contains(&crate::agent::protocol::escape_atom(
                            &refused.file_name().unwrap().to_string_lossy(),
                        ))
                    })
            })
            .count();
        assert_eq!(
            directory_warnings,
            usize::from(refused.is_some()),
            "agent sessions names each unreadable directory in one io warning:\n{output}"
        );
        assert!(
            output
                .lines()
                .any(|line| line.starts_with("sessions matched=2 ")),
            "agent sessions lists the cached sessions under the unreadable directory:\n{output}"
        );
    });
}

/// The path of every session a load through `environment`'s one storage
/// lists, sorted.
fn listed_paths(environment: &Environment) -> Vec<PathBuf> {
    sorted(
        rows_loaded_through(environment)
            .into_iter()
            .map(|row| row.path)
            .collect(),
    )
}

/// The cache keys `environment`'s session cache stores for its one
/// storage's `root`, sorted.
fn cached_keys(environment: &Environment, root: &SessionRoot) -> Vec<String> {
    let storage = environment.storages()[0];
    let mut keys = SqliteCache::open_waiting(environment.cache_base())
        .unwrap()
        .unwrap()
        .stored_keys(storage.cache(), &root.path)
        .unwrap();
    keys.sort();
    keys
}

/// A delete aimed at another agent's transcript, named as this agent names
/// its own, removes nothing.
#[test]
fn file_provider_does_not_own_survives_its_delete() {
    run_contract(Contract::ForeignFileDelete, |fixture| {
        let directory = tempfile::tempdir().unwrap();
        let foreign = fixture.foreign_transcript(directory.path());

        assert!(fixture.delete_under(directory.path(), &foreign).is_err());
        assert!(foreign.exists());
    });
}
