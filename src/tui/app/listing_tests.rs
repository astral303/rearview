//! One sweep over every path that decides whether a session is listed. A
//! session is listed exactly when it was found by its session ID, is already
//! listed, or has a message inside the window.

use super::claude_home::{ClaudeHome, TemplateCache, admitted_cache, app_reading};
use super::commit_failure_tests::{launched_app, tick_until_no_refresh_is_pending};
use super::refresh_tests::{
    refresh_a_new_session_with_an_old_copy, serve_list_refresh, serve_session_refresh,
    window_from_half_past_noon,
};
use super::*;
use crate::agent::service::AgentService;
use crate::agent::test_support::agent_command;
use crate::clock::TestClock;
use crate::history::listing;
use crate::history::sqlite_cache::refresh::tests::{allow_owner_commits, fail_owner_commits};
use crate::history::{ProcessIdentity, RefreshOptions};
use chrono::TimeZone;
use std::path::Path;

/// The lower bound of [`window_from_half_past_noon`], as an agent command's
/// `--after`.
const AFTER_HALF_PAST_NOON: &str = "2026-10-01T12:30:00+00:00";

/// The session most cases decide on, in Claude Code's session ID shape so a
/// lookup finds it.
const SESSION_ID: &str = "0f000000-0000-4000-8000-000000000001";

/// The text of the session's message.
const SESSION_TEXT: &str = "the session under test";

/// The sessions a launch reads when its cache writes stop: more than one
/// parse group, so the launch leaves some of them unparsed.
const SESSIONS_PAST_THE_FIRST_PARSE_GROUP: usize = 8;

/// The way a session comes before the list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Origin {
    FoundById,
    AlreadyListed,
    New,
}

/// The session's messages against the window from 12:30.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Messages {
    Inside,
    OnlyOutside,
}

impl Messages {
    /// The minute past noon the session's messages are written at.
    fn minute(self) -> u32 {
        match self {
            Self::Inside => 40,
            Self::OnlyOutside => 10,
        }
    }

    /// The minute past noon a change to the session is written at, on the
    /// same side of 12:30 as its messages.
    fn change_minute(self) -> u32 {
        self.minute() + 5
    }
}

/// A path that decides whether a session is listed.
#[derive(Clone, Copy, Debug)]
enum ListingPath {
    Launch,
    /// A list `Ctrl+R`.
    ListRefresh,
    /// The session refresh of a new session a list `Ctrl+R` read from an
    /// out-of-date copy.
    RefreshAfterListRefresh,
    /// The session refresh of the sessions a launch left unlisted once its
    /// cache writes stopped.
    RefreshAfterStoppedLaunch,
    /// A session-ID lookup of a session the cache holds.
    IdLookup,
    /// The session refresh of a session found by ID that the cache lacks.
    RefreshAfterIdLookup,
    /// `--debug-search`, `--semantic-search` and the other printouts: the
    /// rows [`listing::retain_listed`] keeps.
    Printout,
    AgentSessions,
    AgentSearch,
    /// `agent sessions` over a session without a row, judged by its
    /// transcript's last write.
    AgentSessionWithoutRow,
}

impl ListingPath {
    const ALL: [Self; 10] = [
        Self::Launch,
        Self::ListRefresh,
        Self::RefreshAfterListRefresh,
        Self::RefreshAfterStoppedLaunch,
        Self::IdLookup,
        Self::RefreshAfterIdLookup,
        Self::Printout,
        Self::AgentSessions,
        Self::AgentSearch,
        Self::AgentSessionWithoutRow,
    ];

    /// The origins this path can give a session.
    fn origins(self) -> &'static [Origin] {
        match self {
            Self::ListRefresh => &[Origin::AlreadyListed, Origin::New],
            Self::IdLookup | Self::RefreshAfterIdLookup => &[Origin::FoundById],
            _ => &[Origin::New],
        }
    }

    /// For each session this path decides on, true when it is listed. A
    /// case's session cache starts as a copy of `admitted`.
    fn listed(self, origin: Origin, messages: Messages, admitted: &TemplateCache) -> Vec<bool> {
        match self {
            Self::Launch => launch(Case::with_session(admitted, messages)),
            Self::ListRefresh => list_refresh(Case::new(admitted), origin, messages),
            Self::RefreshAfterListRefresh => refresh_after_list_refresh(messages),
            Self::RefreshAfterStoppedLaunch => {
                refresh_after_stopped_launch(Case::new(admitted), messages)
            }
            Self::IdLookup => id_lookup(Case::with_session(admitted, messages)),
            Self::RefreshAfterIdLookup => {
                refresh_after_id_lookup(Case::with_session(admitted, messages))
            }
            Self::Printout => printout(Case::with_session(admitted, messages)),
            Self::AgentSessions => vec![
                Case::with_session(admitted, messages)
                    .agent_output(&["sessions", "--all"])
                    .contains("\nsessions matched=1 "),
            ],
            Self::AgentSearch => vec![
                Case::with_session(admitted, messages)
                    .agent_output(&["search", SESSION_TEXT, "--all"])
                    .contains("\nconversation rank="),
            ],
            Self::AgentSessionWithoutRow => {
                agent_session_without_row(Case::new(admitted), messages)
            }
        }
    }
}

/// A Claude Code home with a session cache of its own, on test time.
struct Case {
    home: ClaudeHome,
    cache: tempfile::TempDir,
    clock: Arc<TestClock>,
}

impl Case {
    /// A case without sessions, its cache a copy of `admitted`.
    fn new(admitted: &TemplateCache) -> Self {
        Self {
            home: ClaudeHome::new(),
            cache: admitted.copy(),
            clock: Arc::new(TestClock::default()),
        }
    }

    /// A case holding the session, its message written `messages`.
    fn with_session(admitted: &TemplateCache, messages: Messages) -> Self {
        let case = Self::new(admitted);
        case.write_session(messages);
        case
    }

    fn environment(&self) -> Environment {
        self.home.environment_with_cache(
            self.cache.path(),
            ProcessIdentity::default(),
            self.clock.clone(),
        )
    }

    /// Write the session, its message written `messages`. Returns its
    /// transcript.
    fn write_session(&self, messages: Messages) -> PathBuf {
        self.home
            .write(SESSION_ID, messages.minute(), &[SESSION_TEXT])
    }

    /// An app listing `rows` under the window from 12:30.
    fn app_listing(&self, rows: Vec<Conversation>) -> App {
        let mut app = app_reading(rows, self.environment());
        app.set_list_window(window_from_half_past_noon());
        app
    }

    /// The output of the agent command `args`, from 12:30.
    fn agent_output(&self, args: &[&str]) -> String {
        let args: Vec<&str> = args
            .iter()
            .copied()
            .chain(["--after", AFTER_HALF_PAST_NOON])
            .collect();
        AgentService::new(self.environment())
            .execute(agent_command(&args))
            .unwrap()
    }
}

fn window_options() -> RefreshOptions {
    RefreshOptions {
        time: window_from_half_past_noon(),
        ..RefreshOptions::default()
    }
}

/// True when `app` lists the session at `path`.
fn lists(app: &App, path: &Path) -> bool {
    app.conversations().iter().any(|row| row.path == path)
}

/// True when `app` lists the session [`SESSION_ID`].
fn lists_the_session(app: &App) -> bool {
    app.conversations()
        .iter()
        .any(|row| row.session_id == SESSION_ID)
}

fn launch(case: Case) -> Vec<bool> {
    vec![lists_the_session(&launched_app(
        &case.environment(),
        window_options(),
    ))]
}

fn list_refresh(case: Case, origin: Origin, messages: Messages) -> Vec<bool> {
    let mut app = if origin == Origin::AlreadyListed {
        let session = case.write_session(messages);
        let app = case.app_listing(case.home.rows(&case.environment()));
        case.home
            .append(&session, messages.change_minute(), "a change");
        app
    } else {
        let app = case.app_listing(Vec::new());
        case.write_session(messages);
        app
    };
    serve_list_refresh(&mut app, window_from_half_past_noon());
    vec![lists_the_session(&app)]
}

fn refresh_after_list_refresh(messages: Messages) -> Vec<bool> {
    let refreshed = refresh_a_new_session_with_an_old_copy(messages.change_minute());
    vec![lists(&refreshed.app, &refreshed.new)]
}

fn refresh_after_stopped_launch(case: Case, messages: Messages) -> Vec<bool> {
    let sessions: Vec<PathBuf> = (0..SESSIONS_PAST_THE_FIRST_PARSE_GROUP)
        .map(|index| {
            case.home.write(
                &format!("session-{index}"),
                messages.minute(),
                &[SESSION_TEXT],
            )
        })
        .collect();
    fail_owner_commits(case.cache.path());
    let mut app = launched_app(&case.environment(), window_options());
    assert!(
        sessions
            .iter()
            .any(|session| !lists(&app, session) && app.store.is_refresh_pending(session)),
        "the launch whose writes stopped leaves an unlisted session pending a refresh"
    );

    allow_owner_commits(case.cache.path());
    tick_until_no_refresh_is_pending(&mut app, &case.clock);
    sessions
        .iter()
        .map(|session| lists(&app, session))
        .collect()
}

/// Paste [`SESSION_ID`] into `app`'s search box.
fn look_up_the_session(app: &mut App) {
    app.set_query_for_test(SESSION_ID);
    app.update_filter();
}

fn id_lookup(case: Case) -> Vec<bool> {
    case.home.rows(&case.environment());
    let mut app = case.app_listing(Vec::new());
    look_up_the_session(&mut app);
    vec![lists_the_session(&app)]
}

fn refresh_after_id_lookup(case: Case) -> Vec<bool> {
    let mut app = case.app_listing(Vec::new());
    look_up_the_session(&mut app);
    assert!(
        !lists_the_session(&app),
        "the session isn't listed before its refresh"
    );
    serve_session_refresh(&mut app);
    vec![lists_the_session(&app)]
}

fn printout(case: Case) -> Vec<bool> {
    let mut rows = case.home.rows(&case.environment());
    listing::retain_listed(&mut rows, &window_from_half_past_noon());
    vec![rows.iter().any(|row| row.session_id == SESSION_ID)]
}

/// `agent sessions` over a transcript without a message, last written
/// `messages`: it is listed when the skipped-transcript warning names it.
fn agent_session_without_row(case: Case, messages: Messages) -> Vec<bool> {
    let transcript = case.home.transcript(SESSION_ID);
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(
        &transcript,
        r#"{"type":"summary","summary":"a summary","leafUuid":"leaf"}"#.to_owned() + "\n",
    )
    .unwrap();
    let last_write = chrono::Utc
        .with_ymd_and_hms(2026, 10, 1, 12, messages.minute(), 0)
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&transcript)
        .unwrap()
        .set_modified(last_write.into())
        .unwrap();
    vec![
        case.agent_output(&["sessions", "--all"])
            .contains("protocol agent-warning kind=skipped "),
    ]
}

#[test]
fn session_is_listed_exactly_when_found_by_id_already_listed_or_with_message_inside_window() {
    let admitted = TemplateCache::new(admitted_cache());
    let mut wrong = Vec::new();
    for path in ListingPath::ALL {
        for &origin in path.origins() {
            for messages in [Messages::Inside, Messages::OnlyOutside] {
                let expected = origin != Origin::New || messages == Messages::Inside;
                let listed = path.listed(origin, messages, &admitted);
                if listed.iter().any(|&is_listed| is_listed != expected) {
                    wrong.push(format!(
                        "{path:?}, {origin:?}, messages {messages:?}: listed {listed:?}, expected {expected}"
                    ));
                }
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "each cell lists the session as the rule expects:\n{}",
        wrong.join("\n")
    );
}
