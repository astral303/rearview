//! Behaviors every provider promises, each tested once over every registered
//! provider. Every contract runs through [`run_contract`], the one code path
//! that reaches the fixtures: the fixture list is private to [`runner`].
//!
//! A provider skips a contract only by naming it in its fixture's
//! [`ProviderFixture::opt_outs`] with a reason, and the contract runs for it
//! too and must fail. A provider without a fixture, a fixture that cannot
//! run a contract its provider did not opt out of, and a provider that passes
//! a contract it opted out of each fail the test.
//!
//! A new provider implements [`ProviderFixture`] in its own test module and
//! joins the fixture list in [`runner`]. A test a second provider could pass
//! unchanged belongs here. A provider's own test module keeps only what is
//! specific to that agent's storage.

use super::{
    Deleted, DiscoveredSessions, ResolvedSession, SessionCache, SessionProvider, SessionRoot,
    SessionStorage, SessionStub, SessionTitle,
};
use crate::cli::DebugLevel;
use crate::error::Result;
use crate::history::cache::SessionCacheStore;
use crate::history::{Conversation, Source};
use runner::run_contract;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One variant per contract test below, named after it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Contract {
    SubAgentDiscovery,
    SubAgentDelete,
    SubAgentDeleteCount,
    WarmCacheRename,
    SessionIdLookup,
    SessionIdLookupMatchesDiscovery,
    UnknownIdLookup,
    IdCaseRule,
    SubAgentIdLookup,
    QueryWithoutIdShape,
    RootOverride,
    ForeignFileDelete,
}

/// Contracts a provider skips, and the reason it cannot run them.
pub(crate) struct OptOut {
    pub(crate) contracts: &'static [Contract],
    pub(crate) reason: &'static str,
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
    /// A sub-agent of `session`; `None` for an agent without sub-agents.
    pub(crate) child: Option<&'static str>,
    pub(crate) nested: Nesting,
}

/// A sub-agent of `child`, as the fixture records it. Every fixture states
/// one or the other; there is no default.
pub(crate) enum Nesting {
    /// The id of a sub-agent that `child` ran.
    Recorded(&'static str),
    /// The agent records no sub-agent of a sub-agent, for this reason.
    NotRecorded(&'static str),
}

/// One provider's sessions, written under a temporary home and read,
/// resolved and deleted there. Every method takes the home as a parameter:
/// the providers read their roots from the environment, which a parallel
/// test run cannot set.
///
/// A default that calls [`cannot_run`](Self::cannot_run) fails the contract
/// that calls it: the fixture overrides it, or opts out of that contract.
pub(crate) trait ProviderFixture: Sync {
    fn provider(&self) -> &'static dyn SessionProvider;

    /// The contracts this provider skips; it runs every other one.
    fn opt_outs(&self) -> &'static [OptOut];

    fn ids(&self) -> FixtureIds;

    /// The agent's case rule for ids.
    fn id_case(&self) -> IdCase;

    /// One listable session; returns its locator.
    fn write_session(&self, home: &Path, session: &str) -> PathBuf;

    /// A sub-agent transcript of `session`; returns its locator.
    fn write_subagent(&self, _home: &Path, _session: &str, _child: &str) -> PathBuf {
        self.cannot_run("record a sub-agent")
    }

    /// A transcript of the sub-agent `child`, run by the sub-agent `parent`
    /// of `session`; returns its locator.
    fn write_nested_subagent(
        &self,
        _home: &Path,
        _session: &str,
        _parent: &str,
        _child: &str,
    ) -> PathBuf {
        self.cannot_run("record a sub-agent's sub-agent")
    }

    /// The root under `home` that the provider reads its sessions from.
    fn root_under(&self, home: &Path) -> SessionRoot;

    /// Discovery under `home`, through the provider's storage.
    fn discover_under(&self, home: &Path) -> Vec<SessionStub> {
        let storage = self.storage();
        storage.discover(&self.root_under(home)).unwrap().stubs
    }

    /// The session `id` names under `home`, as the provider's
    /// `resolve_session_id` answers with its root pinned there.
    fn resolve_under(&self, home: &Path, id: &str) -> Result<Option<ResolvedSession>>;

    /// The provider's delete of the session at `locator` under `home`.
    fn delete_under(&self, home: &Path, locator: &Path) -> Result<Deleted>;

    /// True while the agent still stores anything of the session at
    /// `locator`.
    fn is_stored(&self, _home: &Path, locator: &Path) -> bool {
        locator.exists()
    }

    /// The id the sub-agent `child` of `parent` resolves by.
    fn subagent_id(&self, _parent: &str, _child: &str) -> String {
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
        self.provider()
            .storage()
            .unwrap_or_else(|| self.cannot_run("read sessions without a `SessionStorage`"))
    }

    /// Fails the running contract: the fixture cannot do `what`, and the
    /// provider did not opt out of the contract.
    fn cannot_run(&self, what: &str) -> ! {
        panic!(
            "the {} fixture cannot {what}: implement it, or opt the provider out of \
             this contract with a reason",
            self.provider().labels().name
        )
    }
}

/// The fixture list and the one runner over it. The list is private to this
/// module: a contract cannot loop over the fixtures itself and skip the
/// opt-out rule.
mod runner {
    use super::super::{SessionProvider, providers};
    use super::{Contract, Nesting, ProviderFixture};
    use std::collections::BTreeSet;
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

    /// Every provider's fixture, in registration order.
    static FIXTURES: &[&dyn ProviderFixture] = &[
        &super::super::claude::tests::ClaudeFixture,
        &super::super::codex::tests::CodexFixture,
        &super::super::opencode::tests::OpenCodeFixture,
        &super::super::kimi::tests::KimiFixture,
        &super::super::pi::tests::PiFixture,
        &super::super::omp::tests::OmpFixture,
    ];

    fn fixture_of(provider: &dyn SessionProvider) -> Option<&'static dyn ProviderFixture> {
        FIXTURES
            .iter()
            .copied()
            .find(|fixture| fixture.provider().source() == provider.source())
    }

    fn opts_out_of(fixture: &dyn ProviderFixture, contract: Contract) -> bool {
        fixture
            .opt_outs()
            .iter()
            .any(|opt_out| opt_out.contracts.contains(&contract))
    }

    /// Runs `contract` over every registered provider, naming the provider
    /// when the contract fails for it. A provider that opted out must fail
    /// it too, so an opt-out the provider no longer needs fails the test.
    /// Fails unless the contract passed for exactly the registered providers
    /// minus those that opted out; a provider without a fixture fails it.
    pub(super) fn run_contract(contract: Contract, body: impl Fn(&dyn ProviderFixture)) {
        let mut expected = BTreeSet::new();
        let mut passed = BTreeSet::new();
        for provider in providers() {
            let name = provider.labels().name;
            let fixture = fixture_of(*provider);
            let opted_out = fixture.is_some_and(|fixture| opts_out_of(fixture, contract));
            if !opted_out {
                expected.insert(name);
            }
            let Some(fixture) = fixture else {
                continue;
            };
            if opted_out {
                eprintln!("{name} opts out of {contract:?}; the failure below is expected");
            }
            match (opted_out, catch_unwind(AssertUnwindSafe(|| body(fixture)))) {
                (false, Ok(())) => {
                    passed.insert(name);
                }
                (false, Err(failure)) => {
                    eprintln!("the contract failed for provider {name}");
                    resume_unwind(failure);
                }
                (true, Ok(())) => {
                    panic!("{name} passes {contract:?}, which it opts out of: drop the opt-out")
                }
                (true, Err(_)) => {}
            }
        }
        let missing: Vec<_> = expected.difference(&passed).collect();
        assert!(
            missing.is_empty(),
            "{contract:?} did not run over {missing:?}: give each a fixture in FIXTURES"
        );
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

    #[test]
    fn every_opt_out_gives_a_reason_and_names_each_contract_once() {
        for fixture in FIXTURES {
            let name = fixture.provider().labels().name;
            let mut named = Vec::new();
            for opt_out in fixture.opt_outs() {
                assert!(
                    !opt_out.reason.trim().is_empty(),
                    "{name} opts out of {:?} without a reason",
                    opt_out.contracts
                );
                assert!(
                    !opt_out.contracts.is_empty(),
                    "{name} gives the reason {:?} for no contract",
                    opt_out.reason
                );
                for contract in opt_out.contracts {
                    assert!(
                        !named.contains(contract),
                        "{name} opts out of {contract:?} twice"
                    );
                    named.push(*contract);
                }
            }
        }
    }

    #[test]
    fn every_unrecorded_nesting_gives_a_reason() {
        for fixture in FIXTURES {
            if let Nesting::NotRecorded(reason) = fixture.ids().nested {
                assert!(
                    !reason.trim().is_empty(),
                    "{} records no nested sub-agent, without a reason",
                    fixture.provider().labels().name
                );
            }
        }
    }
}

/// Every conversation under `home`, through the shared load loop with its
/// session cache under `cache_base`.
pub(crate) fn load_under(
    fixture: &dyn ProviderFixture,
    home: &Path,
    cache_base: &Path,
) -> Vec<Conversation> {
    let storage = FixtureStorage { fixture, home };
    let cache = SessionCacheStore::under(cache_base, storage.cache());
    super::load_sessions_with_cache(&storage, &cache, false, None).unwrap()
}

/// The provider's storage pinned to `home`, with discovery answered by the
/// fixture's `discover_under`: Pi's and OMP's own discovery reads its walk
/// depth from the environment.
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

    fn roots(&self) -> Result<Vec<SessionRoot>> {
        Ok(vec![self.fixture.root_under(self.home)])
    }

    fn discover(&self, _root: &SessionRoot) -> Result<DiscoveredSessions> {
        Ok(DiscoveredSessions::complete(
            self.fixture.discover_under(self.home),
        ))
    }

    fn parse_session(
        &self,
        stub: &SessionStub,
        root: &SessionRoot,
        debug_level: Option<DebugLevel>,
        on_transcript_read: &(dyn Fn() + Sync),
    ) -> Result<Option<Conversation>> {
        self.fixture
            .storage()
            .parse_session(stub, root, debug_level, on_transcript_read)
    }

    fn max_session_bytes(&self) -> Option<u64> {
        self.fixture.storage().max_session_bytes()
    }

    fn external_titles(&self, root: &SessionRoot) -> HashMap<String, SessionTitle> {
        self.fixture.storage().external_titles(root)
    }
}

fn sorted(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths
}

/// The session `session`, a sub-agent of it, a sub-agent of that one unless
/// the fixture states [`Nesting::NotRecorded`], and an unrelated session,
/// under `home`.
struct SessionFamily {
    session: PathBuf,
    subagents: Vec<PathBuf>,
    other: PathBuf,
}

impl SessionFamily {
    fn write(fixture: &dyn ProviderFixture, home: &Path) -> Self {
        let ids = fixture.ids();
        let child = ids
            .child
            .unwrap_or_else(|| fixture.cannot_run("record a sub-agent"));
        let session = fixture.write_session(home, ids.session);
        let mut subagents = vec![fixture.write_subagent(home, ids.session, child)];
        match ids.nested {
            Nesting::Recorded(nested) => {
                subagents.push(fixture.write_nested_subagent(home, ids.session, child, nested));
            }
            Nesting::NotRecorded(_) => {}
        }
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
    run_contract(Contract::SubAgentDiscovery, |fixture| {
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

/// A sub-agent session left behind would list as a session the user never
/// started, under an id they never saw.
#[test]
fn delete_removes_every_sub_agent_session_with_the_session() {
    run_contract(Contract::SubAgentDelete, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let family = SessionFamily::write(fixture, home.path());

        let deleted = fixture.delete_under(home.path(), &family.session).unwrap();

        assert_eq!(deleted.stored_copies, 1);
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
fn delete_counts_the_sub_agent_sessions_it_removes() {
    run_contract(Contract::SubAgentDeleteCount, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let family = SessionFamily::write(fixture, home.path());

        let deleted = fixture.delete_under(home.path(), &family.session).unwrap();

        assert_eq!(deleted.subagent_sessions, family.subagents.len());
    });
}

/// A rename the cache's change detector cannot see, such as a rewritten
/// sidecar or index, still reaches the next load: a warm load would
/// otherwise restore the old name from the cache indefinitely.
#[test]
fn a_rename_reaches_the_next_load_through_a_warm_cache() {
    run_contract(Contract::WarmCacheRename, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let session = fixture.write_session(home.path(), fixture.ids().session);
        fixture
            .provider()
            .rename_session(&session, "old name")
            .unwrap();

        let first = load_under(fixture, home.path(), cache.path());
        assert_eq!(first[0].custom_title.as_deref(), Some("old name"));

        fixture
            .provider()
            .rename_session(&session, "fresh name")
            .unwrap();

        let second = load_under(fixture, home.path(), cache.path());
        assert_eq!(second[0].custom_title.as_deref(), Some("fresh name"));
    });
}

#[test]
fn a_session_id_resolves_to_its_session_with_its_sub_agents() {
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
fn a_session_id_resolves_to_the_stub_discovery_lists() {
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
fn an_id_the_provider_never_recorded_resolves_to_nothing() {
    run_contract(Contract::UnknownIdLookup, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let ids = fixture.ids();
        fixture.write_session(home.path(), ids.session);

        assert!(fixture.provider().is_session_id_shape(ids.unknown));
        assert_eq!(
            fixture.resolve_under(home.path(), ids.unknown).unwrap(),
            None
        );
    });
}

/// A paste in another case names the same session when the agent writes
/// hex UUIDs, and another session when its ids are case-sensitive.
#[test]
fn an_id_in_another_case_resolves_by_the_agents_case_rule() {
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

/// The one exception to every filter the list applies: a sub-agent's id
/// opens it as a session of its own, with the sub-agents beneath it.
#[test]
fn a_sub_agent_id_resolves_to_a_stub_of_its_own() {
    run_contract(Contract::SubAgentIdLookup, |fixture| {
        let home = tempfile::tempdir().unwrap();
        let ids = fixture.ids();
        let family = SessionFamily::write(fixture, home.path());
        let child = ids.child.unwrap();

        let resolved = fixture
            .resolve_under(home.path(), &fixture.subagent_id(ids.session, child))
            .unwrap()
            .unwrap();

        assert_eq!(resolved.stub.locator, family.subagents[0]);
        assert_eq!(resolved.stub.subagents, family.subagents[1..].to_vec());
    });
}

/// Text typed into the search box resolves to no session in any provider.
#[test]
fn a_query_without_the_id_shape_resolves_to_nothing() {
    run_contract(Contract::QueryWithoutIdShape, |fixture| {
        let provider = fixture.provider();
        assert!(!provider.is_session_id_shape("deployment"));
        assert_eq!(provider.resolve_session_id("deployment").unwrap(), None);
    });
}

/// The roots sit under the agent's home, an override replaces them, and an
/// empty override means unset.
#[test]
fn the_sessions_root_is_the_agents_home_or_its_override() {
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

/// A delete aimed at another agent's transcript, named as this agent names
/// its own, removes nothing.
#[test]
fn a_file_the_provider_does_not_own_survives_its_delete() {
    run_contract(Contract::ForeignFileDelete, |fixture| {
        let directory = tempfile::tempdir().unwrap();
        let foreign = fixture.foreign_transcript(directory.path());

        assert!(fixture.delete_under(directory.path(), &foreign).is_err());
        assert!(foreign.exists());
    });
}
