//! One name for a session in every place that shows it, for every agent and
//! every way the session gets its name. The name a load into an empty cache
//! shows is the name every place shows.

use super::claude_home::{admitted_cache, app_reading, serve_list_refresh};
use super::refresh_tests::{serve_list_change, serve_session_refresh};
use super::test_waits::finish_pending_view_reload;
use super::*;
use crate::agent::service::AgentService;
use crate::agent::test_support::agent_command;
use crate::history::Source;
use crate::history::environment::test_support::test_environment;
use crate::history::provider::contract_tests::{
    Naming, ProviderFixture, kept_storage_under, registered_fixtures,
};
use crate::history::provider::{SessionStorage, TitleAndSummary};
use crate::history::sqlite_cache::admitted_reads;
use crate::time_filter::TimeFilter;
use std::path::Path;

/// The names the agent gives the session while the lists show it, one per
/// step.
const AGENT_NAMES: [&str; 3] = ["Zebrafish plans", "Yakety release", "Walrus outline"];
/// The name the user gives the session in rearview.
const REARVIEW_NAME: &str = "Xylophone notes";

#[test]
fn every_place_shows_the_name_a_load_into_an_empty_cache_shows() {
    // Each agent's session runs on a thread of its own: a list marks its
    // thread as the drawing thread, and the drawing thread doesn't parse a
    // transcript.
    let running: Vec<_> = registered_fixtures()
        .into_iter()
        .map(|fixture| std::thread::spawn(move || misses_of(fixture)))
        .collect();
    let wrong: Vec<String> = running
        .into_iter()
        .flat_map(|session| session.join().unwrap())
        .collect();
    assert!(
        wrong.is_empty(),
        "every place shows the session's one name:\n{}",
        wrong.join("\n")
    );
}

/// The places that show another name than a load into an empty cache, for
/// `fixture`'s session without a name, after each name the agent gives it
/// while a list launched each way shows it, after its transcript gains a
/// turn and after a rename in rearview.
fn misses_of(fixture: &'static dyn ProviderFixture) -> Vec<String> {
    let home = tempfile::tempdir().unwrap();
    let locator = fixture.write_session(home.path(), fixture.ids().session);
    let storage = kept_storage_under(fixture, home.path());
    let mut lists =
        [Launch::OverCache, Launch::OnEmptyCache].map(|launch| List::launch(launch, storage));
    let body_word = lists[0].app.conversations[0]
        .preview_first
        .split(|ch: char| !ch.is_alphabetic())
        .find(|word| word.len() > 3)
        .expect("the session's text has a word")
        .to_lowercase();
    let mut places = Places {
        source: fixture.provider().source(),
        storage,
        body_word,
        given: Vec::new(),
        wrong: Vec::new(),
    };
    places.check(&mut lists, "after the load", Commands::Run);

    for (naming, name) in agent_namings(fixture).into_iter().zip(AGENT_NAMES) {
        let step = format!("after the agent names it {name:?} ({naming:?}) and a list refresh");
        let fingerprint_before = fingerprint_of(fixture, home.path(), &locator);
        off_the_drawing_thread(|| naming.name(fixture, &locator, name));
        let reads_before = admitted_reads::count(&locator);
        for list in &mut lists {
            serve_list_refresh(&mut list.app, TimeFilter::default());
        }
        if fingerprint_of(fixture, home.path(), &locator) == fingerprint_before
            && admitted_reads::count(&locator) != reads_before
        {
            places.miss(&step, "the refreshes read the unchanged transcript");
        }
        places.given.push(name);
        places.check(&mut lists, &step, Commands::Run);
    }

    // A resumed read must not bring back an earlier name.
    off_the_drawing_thread(|| fixture.append_turn(&locator, "a later reply"));
    for list in &mut lists {
        serve_list_refresh(&mut list.app, TimeFilter::default());
    }
    places.check(
        &mut lists,
        "after the transcript gains a turn",
        Commands::Skip,
    );

    if fixture.provider().rename().is_capable() {
        for list in &mut lists {
            open_the_session(&mut list.app);
            rename_in_rearview(&mut list.app, REARVIEW_NAME);
        }
        places.given.push(REARVIEW_NAME);
        places.check(
            &mut lists,
            "after a rename in the viewer, before its session refresh",
            Commands::Skip,
        );
        for list in &mut lists {
            open_the_session(&mut list.app);
            serve_session_refresh(&mut list.app);
            finish_pending_view_reload(&mut list.app, VIEWPORT_HEIGHT);
        }
        places.check(
            &mut lists,
            "after a rename in the viewer and its session refresh",
            Commands::Run,
        );
    }
    places.wrong
}

/// The names `fixture`'s agent gives the session in turn: from no name to
/// each kind of name and from each kind to the other. An agent that
/// generates names ends on a generated one, which the rename in rearview
/// then replaces.
fn agent_namings(fixture: &dyn ProviderFixture) -> Vec<Naming> {
    let namings = Naming::every_one_of(fixture);
    if namings.contains(&Naming::Generated) {
        vec![Naming::Generated, Naming::Chosen, Naming::Generated]
    } else {
        namings.repeat(2)
    }
}

const VIEWPORT_HEIGHT: usize = 10;

/// Open the list's only session in the viewer.
fn open_the_session(app: &mut App) {
    app.selected = Some(0);
    app.enter_view_mode(80);
}

/// The fingerprint discovery lists for `fixture`'s session at `locator`
/// under `home`.
fn fingerprint_of(
    fixture: &dyn ProviderFixture,
    home: &Path,
    locator: &Path,
) -> crate::history::provider::Fingerprint {
    off_the_drawing_thread(|| {
        fixture
            .discover_under(home)
            .into_iter()
            .find(|stub| stub.locator == locator)
            .expect("discovery lists the session")
            .fingerprint
    })
}

/// The rows a list launches with.
#[derive(Clone, Copy, Debug)]
enum Launch {
    /// Read from the copy an earlier load wrote.
    OverCache,
    /// Parsed by the list's own first load, whose write makes the copy the
    /// row reads.
    OnEmptyCache,
}

/// A list of the session, over a cache of its own.
struct List {
    launch: Launch,
    app: App,
    cache: tempfile::TempDir,
}

impl List {
    /// The list `launch` launches with `storage`'s sessions.
    fn launch(launch: Launch, storage: &'static dyn SessionStorage) -> Self {
        let cache = off_the_drawing_thread(admitted_cache);
        let environment = test_environment(vec![storage], Some(cache.path()));
        let rows = off_the_drawing_thread(|| {
            let rows = rows_of(&environment, storage);
            match launch {
                Launch::OverCache => rows_of(&environment, storage),
                Launch::OnEmptyCache => rows,
            }
        });
        Self {
            launch,
            app: app_reading(rows, environment),
            cache,
        }
    }
}

/// `run`'s result, run on a thread of its own: the drawing thread doesn't
/// parse a transcript or own the cache, and an agent's renamer reads the
/// transcript first.
fn off_the_drawing_thread<T: Send>(run: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| scope.spawn(run).join().unwrap())
}

/// Rename the list's only session to `name` in the name box, as the
/// background worker makes the rename.
fn rename_in_rearview(app: &mut App, name: &str) {
    app.selected = Some(0);
    app.start_rename();
    app.dialog_mode = DialogMode::Rename {
        input: name.to_owned(),
        cursor: name.chars().count(),
    };
    app.submit_rename();
    serve_list_change(app);
}

/// Whether a check runs the agent commands. They read the agent's files and
/// the cache, so a step that changes only the lists' rows skips them.
#[derive(Clone, Copy)]
enum Commands {
    Run,
    Skip,
}

/// Every place that shows the session, and the places found showing another
/// name than a load into an empty cache.
struct Places {
    source: Source,
    storage: &'static dyn SessionStorage,
    /// A word of the session's text, which every search finds it by.
    body_word: String,
    /// Each name given to the session so far.
    given: Vec<&'static str>,
    wrong: Vec<String>,
}

impl Places {
    fn miss(&mut self, step: &str, miss: impl std::fmt::Display) {
        self.wrong
            .push(format!("{:?}, {step}: {miss}", self.source));
    }

    /// Check every place in `lists`, and the agent commands over the first
    /// list's cache when `commands` runs them, against a load into an empty
    /// cache. A viewer open through the step is checked, then closed: the
    /// list then applies what it deferred while the viewer was open.
    fn check(&mut self, lists: &mut [List], step: &str, commands: Commands) {
        let expected = self.load_into_empty_cache();
        let shown = title_and_summary(&expected);
        let mut misses = Vec::new();
        for list in lists.iter_mut() {
            let launch = list.launch;
            let app = &mut list.app;
            if let AppMode::View(_) = app.app_mode {
                misses.extend(
                    header_miss(app, &shown, "open through the step")
                        .map(|miss| format!("{launch:?}: {miss}")),
                );
                app.exit_view_mode();
                app.apply_finished_refresh();
            }
            let row = title_and_summary(&app.conversations[0]);
            if row != shown {
                misses.push(format!(
                    "{launch:?}: the list row shows {row:?}, not {shown:?}"
                ));
            }
            misses.extend(
                list_reader_misses(app, &shown, &self.given, &self.body_word)
                    .into_iter()
                    .map(|miss| format!("{launch:?}: {miss}")),
            );
            open_the_session(app);
            misses.extend(
                header_miss(app, &shown, "at open").map(|miss| format!("{launch:?}: {miss}")),
            );
            app.exit_view_mode();
        }
        if let Commands::Run = commands {
            let name = crate::agent::protocol::title_for_conversation(&expected);
            let is_named = shown.custom_title.is_some() || shown.summary.is_some();
            misses.extend(self.agent_command_misses(lists[0].cache.path(), &name, is_named));
        }
        for miss in misses {
            self.miss(step, miss);
        }
    }

    /// The session's row as a load into an empty cache lists it.
    fn load_into_empty_cache(&self) -> Conversation {
        let storage = self.storage;
        off_the_drawing_thread(|| {
            let cache = tempfile::tempdir().unwrap();
            rows_of(
                &test_environment(vec![storage], Some(cache.path())),
                storage,
            )
            .remove(0)
        })
    }

    /// The misses of `agent sessions`, `agent search` and `agent within`
    /// over `cache`: each prints `name`, the session's one-line name.
    /// `agent within` runs keeping the cache, and reading it as it is while
    /// another rearview keeps it, for a session `is_named`: it names an
    /// untitled session from other messages than the list preview.
    fn agent_command_misses(&self, cache: &Path, name: &str, is_named: bool) -> Vec<String> {
        let (storage, body_word) = (self.storage, self.body_word.as_str());
        let run = |args: &[&str]| {
            off_the_drawing_thread(|| {
                AgentService::new(test_environment(vec![storage], Some(cache)))
                    .execute(agent_command(args))
                    .unwrap_or_else(crate::error_report)
            })
        };
        let sessions = run(&["sessions", "--all"]);
        let reference = sessions
            .split_whitespace()
            .find_map(|atom| atom.strip_prefix("ref="))
            .expect("agent sessions lists the session")
            .to_owned();
        let search = run(&["search", body_word, "--all"]);
        let mut outputs = vec![("agent sessions", sessions), ("agent search", search)];
        if is_named {
            let within = ["within", reference.as_str(), body_word];
            outputs.push(("agent within keeping the cache", run(&within)));
            let other_refresh = off_the_drawing_thread(|| another_rearviews_refresh(cache));
            outputs.push(("agent within reading the cache as it is", run(&within)));
            drop(other_refresh);
        }
        outputs
            .into_iter()
            .filter(|(_, output)| !prints_only_title(output, name))
            .map(|(command, output)| format!("{command} titles it {name:?}:\n{output}"))
            .collect()
    }
}

/// Another rearview's refresh ownership of the cache at `cache`, held until
/// dropped.
fn another_rearviews_refresh(
    cache: &Path,
) -> crate::history::sqlite_cache::refresh::RefreshOwnership {
    crate::history::sqlite_cache::refresh::RefreshOwnership::acquire_for_test(
        &crate::history::CacheBase::new(
            cache,
            crate::history::ProcessIdentity::default(),
            std::sync::Arc::new(crate::clock::TestClock::default()),
            crate::history::provider::walk::SystemCanonicaliser::shared(),
        ),
    )
    .unwrap()
}

/// True when `output` prints a title, and each title it prints is `name`,
/// truncated where the command's limit truncates it. A `session`,
/// `conversation` or `title` record prints the title after ` : `.
fn prints_only_title(output: &str, name: &str) -> bool {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut titles = output
        .lines()
        .filter(|line| {
            ["session ", "conversation ", "title "]
                .iter()
                .any(|record| line.starts_with(record))
        })
        .filter_map(|line| line.split_once(" : ").map(|(_, title)| title))
        .peekable();
    titles.peek().is_some()
        && titles.all(|title| match title.strip_suffix("...") {
            Some(cut) => name.starts_with(cut),
            None => title == name,
        })
}

fn title_and_summary(row: &Conversation) -> TitleAndSummary {
    TitleAndSummary {
        custom_title: row.custom_title.clone(),
        summary: row.summary.clone(),
    }
}

/// True when `shown` shows `name`, as the title or the summary.
fn shows(shown: &TitleAndSummary, name: &str) -> bool {
    [&shown.custom_title, &shown.summary]
        .into_iter()
        .any(|shown| shown.as_deref() == Some(name))
}

fn first_word(name: &str) -> &str {
    name.split_whitespace().next().unwrap()
}

/// The miss of the open viewer's header when it doesn't show `shown`.
fn header_miss(app: &App, shown: &TitleAndSummary, when: &str) -> Option<String> {
    let AppMode::View(state) = &app.app_mode else {
        return Some(format!("the viewer is open {when}"));
    };
    let header = state.header.as_ref().map(|header| TitleAndSummary {
        custom_title: header.custom_title.clone(),
        summary: header.summary.clone(),
    });
    (header.as_ref() != Some(shown))
        .then(|| format!("the viewer's header {when} shows {header:?}, not {shown:?}"))
}

/// The misses of every reader of `app`'s list but the row itself, read off
/// the drawing thread as the list's search worker reads the cache: each
/// finds the session by every name of `given` that `shown` shows, and by no
/// other. The session's text contains `body_word`.
fn list_reader_misses(
    app: &App,
    shown: &TitleAndSummary,
    given: &[&str],
    body_word: &str,
) -> Vec<String> {
    let (conversations, searchable) = (&app.conversations, &app.searchable);
    off_the_drawing_thread(|| {
        let mut misses = list_search_misses(conversations, searchable, shown, given, body_word);
        misses.extend(other_reader_misses(conversations, shown, given));
        misses
    })
}

/// The misses of the list's search of `conversations`, whose session shows
/// `shown` and has `body_word` in its text.
fn list_search_misses(
    conversations: &[Conversation],
    searchable: &[crate::search::SearchableConversation],
    shown: &TitleAndSummary,
    given: &[&str],
    body_word: &str,
) -> Vec<String> {
    let mut wrong = Vec::new();
    let in_memory = crate::search::SearchTextsInMemory::read(conversations);
    let without_texts = crate::search::SearchTextsInMemory::default();
    let name_queries: Vec<(String, usize)> = given
        .iter()
        .map(|name| {
            (
                first_word(name).to_lowercase(),
                usize::from(shows(shown, name)),
            )
        })
        .collect();
    let body_queries: Vec<(String, usize)> = given
        .iter()
        .filter(|name| shows(shown, name))
        .map(|name| {
            (
                format!("{} {body_word}", first_word(name).to_lowercase()),
                1,
            )
        })
        .collect();
    // A row without its texts in memory is searched on its title, summary and
    // project name: whether its body word matches depends on its summary.
    for (texts, read, queries) in [
        (
            &in_memory,
            "with the texts in memory",
            [name_queries.as_slice(), body_queries.as_slice()].concat(),
        ),
        (&without_texts, "no texts in memory", name_queries.clone()),
    ] {
        for (query, expected) in queries {
            let found = crate::search::try_debug_search(
                conversations,
                searchable,
                texts,
                &query,
                chrono::Local::now(),
                |_| true,
            )
            .unwrap()
            .results
            .len();
            if found != expected {
                wrong.push(format!(
                    "{query:?} finds {expected} session(s) read {read}, not {found}"
                ));
            }
        }
    }
    wrong
}

/// The misses of the readers other than the list's search, for the session
/// of `conversations`, which shows `shown`: the list's evidence and context
/// lines, `agent search`'s ranking and exact filter, the agent's semantic
/// route and the semantic search's exact preview.
fn other_reader_misses(
    conversations: &[Conversation],
    shown: &TitleAndSummary,
    given: &[&str],
) -> Vec<String> {
    use crate::search::lexical::{RetrievalTerms, precompute_agent_search_text, try_agent_search};
    use crate::search::preview::{
        HighlightQuery, build_context_segments_from_ranges, build_literal_context_segments,
    };
    use crate::search::query::ParsedQuery;
    let mut wrong = Vec::new();
    let row = &conversations[0];
    let in_memory = crate::search::SearchTextsInMemory::read(conversations);
    let unwindowed = crate::time_filter::TimeFilter::default();
    let agent_texts = precompute_agent_search_text(conversations, &unwindowed);
    let content = conversations[0].load_held_content().unwrap().row;
    let route = content.semantic_route_text().to_lowercase();
    let every_row = (0..conversations.len()).collect::<Vec<_>>();
    for name in given {
        let word = first_word(name);
        let query = word.to_lowercase();
        let is_shown = shows(shown, name);
        let expected = usize::from(is_shown);
        let found = try_agent_search(
            conversations,
            &agent_texts,
            &query,
            RetrievalTerms::WordsAndLiterals,
            chrono::Local::now(),
            |_| true,
            crate::search::ScanScope::AllWorkspaces,
        )
        .unwrap()
        .ranked
        .len();
        if found != expected {
            wrong.push(format!(
                "agent search {query:?} finds {expected} session(s), not {found}"
            ));
        }
        let phrase = name.to_lowercase();
        let found = crate::agent::service::lexically_rank_scoped(
            conversations,
            &phrase,
            crate::agent::service::ScopedSessions {
                indices: &every_row,
                scope: crate::agent::search::AgentSearchScope::Global,
            },
            crate::search::mode::SearchMode::Exact,
            &unwindowed,
            &crate::clock::TestClock::default(),
        )
        .unwrap()
        .ranked
        .len();
        if found != expected {
            wrong.push(format!(
                "exact agent search {phrase:?} finds {expected} session(s), not {found}"
            ));
        }
        if route.contains(&query) != is_shown {
            wrong.push(format!(
                "the semantic route text contains {query:?} only while the session shows it: \
                 {route:?}"
            ));
        }
        if !is_shown {
            continue;
        }
        for (texts, read) in [
            (in_memory.get(row), "with the texts in memory"),
            (None, "no texts in memory"),
        ] {
            let evidence = crate::search::evidence::build_lexical_evidence(
                row,
                texts,
                &ParsedQuery::parse(&query),
            );
            let line = evidence.as_ref().and_then(|evidence| {
                build_context_segments_from_ranges(
                    &evidence.texts(row),
                    &evidence.context_ranges,
                    80,
                )
            });
            if !line.as_deref().is_some_and(|line| line.contains(word)) {
                wrong.push(format!("the evidence read {read} shows {word:?}: {line:?}"));
            }
        }
        let quoted = format!("\"{query}\"");
        let line =
            build_literal_context_segments(row, &row.preview, &HighlightQuery::parse(&quoted), 80);
        if !line.as_deref().is_some_and(|line| line.contains(word)) {
            wrong.push(format!("the literal context line shows {word:?}: {line:?}"));
        }
        let quoted = ParsedQuery::parse(&quoted);
        let evidence =
            crate::search::build_session_list_evidence(conversations, &in_memory, &[0], &quoted);
        let preview =
            super::exact_literal::exact_literal_results(conversations, &evidence, &[0], &quoted)
                .remove(&0)
                .map(|metadata| metadata.explanation.evidence_preview);
        if !preview
            .as_deref()
            .is_some_and(|preview| preview.contains(word))
        {
            wrong.push(format!(
                "the semantic search's exact preview shows {word:?}: {preview:?}"
            ));
        }
    }
    wrong
}

/// Every session `storage` stores, as a load through `environment` lists
/// them.
fn rows_of(
    environment: &crate::history::Environment,
    storage: &'static dyn SessionStorage,
) -> Vec<Conversation> {
    let operation = crate::history::environment::test_support::waiting_operation(environment);
    let rows = crate::history::provider::load_sessions(
        environment,
        storage,
        crate::history::provider::LoadOptions::default(),
        &operation,
        &mut |_, _| {},
    )
    .unwrap()
    .conversations;
    operation.finish().fail_if_low_space().unwrap();
    rows
}
