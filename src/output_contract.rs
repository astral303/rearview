//! The output contract: each agent command's output, and the TUI's list,
//! search results and viewer, over every provider's every-record session,
//! compared byte for byte with the expected files in `tests/output-contract/`.
//! A change to how sessions are stored or read must leave every file
//! unchanged.
//!
//! One test per surface compares one file and names the first line that
//! differs. The outputs come from one child process: the test binary runs
//! [`child_process_writes_the_outputs`] alone, with the home holding the
//! sessions, the cache, the time zone and the clock fixed, so the outputs
//! are the same on every machine and every day:
//! - The home holds each provider's every-record session at the provider's
//!   first default root, with fixed modification times, and no
//!   configuration file. Every provider's storage variables and session
//!   variables are unset.
//! - The cache starts empty, and no command searches semantically.
//! - Every command and the TUI run on a test clock standing at
//!   [`FIXED_NOW`].
//! - Times print in America/New_York. On Unix the child process's `TZ` sets
//!   it. Windows reads the system time zone whatever `TZ` holds, so there the
//!   tests require US Eastern Time.
//!
//! When an output changes on purpose, write the files again and review
//! their diff before committing it:
//!
//! ```text
//! REARVIEW_WRITE_OUTPUT_CONTRACT=1 cargo test --lib output_contract
//! ```

use crate::agent::test_support::atom;
use crate::clock::TestClock;
use crate::history::provider::contract_tests::{AgentSession, registered_fixtures};
use crate::history::provider::{RootEnvironment, SessionStorage};
use chrono::{DateTime, TimeZone, Utc};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

/// The test the child process runs, by its path in the test binary.
const CHILD_TEST: &str = "output_contract::child_process_writes_the_outputs";
/// Names the directory the child process writes the outputs to. Unset, the
/// child test does nothing.
const OUTPUT_DIR_VARIABLE: &str = "REARVIEW_OUTPUT_CONTRACT_OUTPUT";
/// Set to write the expected files from the outputs instead of comparing.
const WRITE_VARIABLE: &str = "REARVIEW_WRITE_OUTPUT_CONTRACT";
const TIME_ZONE: &str = "America/New_York";
/// After every fixture's last message.
const FIXED_NOW: &str = "2026-10-01T16:00:00Z";
/// The modification time of the first provider's transcripts; each later
/// provider's are an hour later, so the list has an order to show.
const FIRST_MODIFIED: &str = "2026-09-20T12:00:00Z";

/// One file of the contract.
#[derive(Clone, Copy, Debug)]
enum Surface {
    AgentSessions,
    AgentSearch,
    AgentWithin,
    AgentRead,
    AgentOutline,
    TuiList,
    TuiSearch,
    TuiViewer,
}

impl Surface {
    const ALL: [Self; 8] = [
        Self::AgentSessions,
        Self::AgentSearch,
        Self::AgentWithin,
        Self::AgentRead,
        Self::AgentOutline,
        Self::TuiList,
        Self::TuiSearch,
        Self::TuiViewer,
    ];

    fn file_name(self) -> &'static str {
        match self {
            Self::AgentSessions => "agent-sessions.txt",
            Self::AgentSearch => "agent-search.txt",
            Self::AgentWithin => "agent-within.txt",
            Self::AgentRead => "agent-read.txt",
            Self::AgentOutline => "agent-outline.txt",
            Self::TuiList => "tui-list.txt",
            Self::TuiSearch => "tui-search.txt",
            Self::TuiViewer => "tui-viewer.txt",
        }
    }

    fn expected_path(self) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/output-contract")
            .join(self.file_name())
    }
}

fn fixed_time(rfc_3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc_3339)
        .unwrap()
        .with_timezone(&Utc)
}

/// The clock the outputs read: [`FIXED_NOW`]. Nothing in the child process
/// sleeps on it, so it stays there.
fn fixed_clock() -> Arc<TestClock> {
    let clock = TestClock::default();
    clock.set_wall(fixed_time(FIXED_NOW));
    Arc::new(clock)
}

// ---------------------------------------------------------------------------
// The surface tests and the parent process
// ---------------------------------------------------------------------------

#[test]
fn agent_sessions_output_matches_the_contract() {
    assert_matches_contract(Surface::AgentSessions);
}

#[test]
fn agent_search_output_matches_the_contract() {
    assert_matches_contract(Surface::AgentSearch);
}

#[test]
fn agent_within_output_matches_the_contract() {
    assert_matches_contract(Surface::AgentWithin);
}

#[test]
fn agent_read_output_matches_the_contract() {
    assert_matches_contract(Surface::AgentRead);
}

#[test]
fn agent_outline_output_matches_the_contract() {
    assert_matches_contract(Surface::AgentOutline);
}

#[test]
fn tui_list_matches_the_contract() {
    assert_matches_contract(Surface::TuiList);
}

#[test]
fn tui_search_results_match_the_contract() {
    assert_matches_contract(Surface::TuiSearch);
}

#[test]
fn tui_viewer_matches_the_contract() {
    assert_matches_contract(Surface::TuiViewer);
}

/// Fails on the first line where `surface`'s output differs from its file,
/// naming both lines. With [`WRITE_VARIABLE`] set, writes the file instead.
#[track_caller]
fn assert_matches_contract(surface: Surface) {
    let outputs = match contract_outputs() {
        Ok(outputs) => outputs,
        Err(failure) => panic!("{failure}"),
    };
    let actual = &outputs[surface as usize];
    let path = surface.expected_path();
    if std::env::var_os(WRITE_VARIABLE).is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read {}: {error}; write it with {WRITE_VARIABLE}=1",
            path.display()
        )
    });
    if let Some(difference) = first_difference(&expected, actual) {
        panic!(
            "{} differs from its output at {difference}\nRun with {WRITE_VARIABLE}=1 \
             to write the outputs, then review their diff.",
            surface.file_name()
        );
    }
}

/// `line N`, then the expected and the actual line, for the first line that
/// differs; `None` when the texts are equal.
fn first_difference(expected: &str, actual: &str) -> Option<String> {
    if expected == actual {
        return None;
    }
    let mut expected_lines = expected.split_inclusive('\n');
    let mut actual_lines = actual.split_inclusive('\n');
    for number in 1.. {
        match (expected_lines.next(), actual_lines.next()) {
            (Some(expected), Some(actual)) if expected == actual => continue,
            (expected, actual) => {
                return Some(format!(
                    "line {number}:\n  expected: {expected:?}\n  actual:   {actual:?}"
                ));
            }
        }
    }
    unreachable!("two unequal texts differ at a line")
}

#[test]
fn a_difference_names_its_first_line_and_both_texts_there() {
    assert_eq!(first_difference("a\nb\n", "a\nb\n"), None);
    assert_eq!(
        first_difference("a\nb\nc\n", "a\nB\nc\n"),
        Some("line 2:\n  expected: Some(\"b\\n\")\n  actual:   Some(\"B\\n\")".to_owned())
    );
    assert_eq!(
        first_difference("a\n", "a\nextra\n"),
        Some("line 2:\n  expected: None\n  actual:   Some(\"extra\\n\")".to_owned())
    );
}

/// The output of every surface, in [`Surface::ALL`] order, written once per
/// test process by one child process.
fn contract_outputs() -> &'static Result<Vec<String>, String> {
    static OUTPUTS: OnceLock<Result<Vec<String>, String>> = OnceLock::new();
    OUTPUTS.get_or_init(run_child_process)
}

fn run_child_process() -> Result<Vec<String>, String> {
    require_the_time_zone()?;
    let run = tempfile::tempdir().map_err(|error| error.to_string())?;
    let home = run.path().join("home");
    let output = run.path().join("output");
    std::fs::create_dir_all(&output).map_err(|error| error.to_string())?;
    // Kept alive until the child process has read them: some fixtures build
    // their session in a temporary directory.
    let _sessions = write_every_record_sessions(&home);
    let mut child = Command::new(std::env::current_exe().map_err(|error| error.to_string())?);
    child
        .args([CHILD_TEST, "--exact", "--ignored", "--test-threads=1"])
        .current_dir(&home)
        .env(OUTPUT_DIR_VARIABLE, &output)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("REARVIEW_CACHE_DIR", run.path().join("cache"))
        .env("TZ", TIME_ZONE);
    for variable in crate::test_support::storage_env_vars()
        .into_iter()
        .chain(crate::test_support::current_session_variables())
    {
        child.env_remove(variable);
    }
    let finished = child.output().map_err(|error| error.to_string())?;
    if !finished.status.success() {
        return Err(format!(
            "the child process failed: {}\n{}{}",
            finished.status,
            String::from_utf8_lossy(&finished.stdout),
            String::from_utf8_lossy(&finished.stderr)
        ));
    }
    Surface::ALL
        .iter()
        .map(|surface| {
            std::fs::read_to_string(output.join(surface.file_name())).map_err(|error| {
                format!(
                    "the child process wrote no {}: {error}; does {CHILD_TEST} name the child test?",
                    surface.file_name()
                )
            })
        })
        .collect()
}

/// On Windows, an error unless the system time zone is US Eastern Time:
/// chrono reads it there and ignores `TZ`.
fn require_the_time_zone() -> Result<(), String> {
    if cfg!(unix) {
        return Ok(());
    }
    let offset_hours = |rfc_3339| {
        chrono::Local
            .from_utc_datetime(&fixed_time(rfc_3339).naive_utc())
            .offset()
            .local_minus_utc()
            / 3600
    };
    if (
        offset_hours("2026-01-15T12:00:00Z"),
        offset_hours("2026-07-15T12:00:00Z"),
    ) == (-5, -4)
    {
        return Ok(());
    }
    Err(format!(
        "the output contract prints times in {TIME_ZONE}; this system's time zone is \
         another, and Windows ignores TZ"
    ))
}

/// Writes every provider's every-record session at the provider's first
/// default root under `home`, each provider's transcripts modified an hour
/// after the previous provider's. Returns the sessions, which keep their
/// fixtures' temporary directories alive.
fn write_every_record_sessions(home: &Path) -> Vec<AgentSession> {
    let no_variables = |_: &str| None;
    let environment = RootEnvironment::new(
        &no_variables,
        Some(home.to_path_buf()),
        Some(home.to_path_buf()),
    );
    let first_modified = fixed_time(FIRST_MODIFIED);
    registered_fixtures()
        .into_iter()
        .enumerate()
        .map(|(index, fixture)| {
            let storage: &dyn SessionStorage = fixture.storage();
            let roots = storage.roots_in(&environment).unwrap();
            let root = &roots[0];
            let provider_home = root.path.parent().unwrap();
            assert_eq!(
                fixture.root_under(provider_home).path,
                root.path,
                "{:?}'s fixture writes under its default root",
                storage.source()
            );
            let session = fixture.every_record_session();
            fixture.write_every_record_session(provider_home, &session);
            let modified = first_modified + chrono::Duration::hours(index as i64);
            set_modified_under(provider_home, modified.into());
            session
        })
        .collect()
}

fn set_modified_under(directory: &Path, modified: std::time::SystemTime) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            set_modified_under(&entry.path(), modified);
        } else {
            std::fs::File::options()
                .write(true)
                .open(entry.path())
                .unwrap()
                .set_modified(modified)
                .unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// The child process
// ---------------------------------------------------------------------------

/// Writes every surface's output to the directory [`OUTPUT_DIR_VARIABLE`]
/// names. The surface tests run it in a child process with its environment
/// fixed; without the variable it does nothing.
#[test]
#[ignore = "the output contract tests run it in a child process"]
fn child_process_writes_the_outputs() {
    let Some(output) = std::env::var_os(OUTPUT_DIR_VARIABLE) else {
        return;
    };
    let output = PathBuf::from(output);
    let agent = AgentOutputs::run();
    let tui = crate::tui::contract_outputs::tui_outputs(
        TUI_QUERIES,
        crate::history::Environment::of_user(fixed_clock()),
    );
    for (surface, text) in [
        (Surface::AgentSessions, agent.sessions),
        (Surface::AgentSearch, agent.search),
        (Surface::AgentWithin, agent.within),
        (Surface::AgentRead, agent.read),
        (Surface::AgentOutline, agent.outline),
        (Surface::TuiList, tui.list),
        (Surface::TuiSearch, tui.search),
        (Surface::TuiViewer, tui.viewer),
    ] {
        std::fs::write(output.join(surface.file_name()), text).unwrap();
    }
}

/// The TUI's search queries: a word every agent's session holds, a phrase,
/// a quoted phrase, a sub-agent's marker, and a word no session holds.
const TUI_QUERIES: &[&str] = &[
    "answer",
    "tool output",
    "\"answer searchable\"",
    "SPLICED_SUBAGENT_SENTINEL",
    "zzqqxx",
];

/// The `agent sessions` cases: budgets, pages, windows and an inverted
/// window.
const SESSIONS_CASES: &[&[&str]] = &[
    &["agent", "sessions"],
    &["agent", "sessions", "--no-budget"],
    &["agent", "sessions", "--limit", "3"],
    &["agent", "sessions", "--offset", "3", "--limit", "3"],
    &["agent", "sessions", "--budget", "500"],
    &["agent", "sessions", "--budget", "1200"],
    &[
        "agent",
        "sessions",
        "--after",
        "2026-08-01",
        "--before",
        "2026-09-02",
    ],
    &[
        "agent",
        "sessions",
        "--after",
        "2026-08-01T06:01",
        "--before",
        "2026-08-01T06:02",
    ],
    &["agent", "sessions", "--since", "60d"],
    &[
        "agent",
        "sessions",
        "--after",
        "2026-08-02",
        "--before",
        "2026-08-01",
    ],
];

/// The `agent search` cases: lexical and exact queries, each kind of text a
/// marker sits in, the hit and page options, budgets, windows, a query
/// without hits, and errors.
const SEARCH_CASES: &[&[&str]] = &[
    &["agent", "search", "answer"],
    &["agent", "search", "searchable", "--top", "3"],
    &["agent", "search", "searchable", "--flat"],
    &["agent", "search", "searchable", "--all-hits"],
    &["agent", "search", "searchable", "--hits-per-conv", "1"],
    &["agent", "search", "searchable", "--offset", "2"],
    &["agent", "search", "searchable", "--budget", "700"],
    &["agent", "search", "searchable", "--no-budget"],
    &["agent", "search", "tool output searchable"],
    &["agent", "search", "\"tool output searchable\""],
    &["agent", "search", "--mode", "exact", "answer searchable"],
    &["agent", "search", "--mode", "exact", "sentinel"],
    &["agent", "search", "SPLICED_SUBAGENT_SENTINEL"],
    &["agent", "search", "TOOL_CALL_SENTINEL"],
    &["agent", "search", "THINKING_SENTINEL"],
    &["agent", "search", "LAUNCH_PROMPT_SENTINEL", "--no-budget"],
    &[
        "agent",
        "search",
        "question",
        "--after",
        "2026-08-01",
        "--before",
        "2026-09-02",
    ],
    &[
        "agent",
        "search",
        "codex",
        "--after",
        "2026-08-01T06:01",
        "--before",
        "2026-08-01T06:02",
    ],
    &["agent", "search", "zzqqxx"],
    &[
        "agent",
        "search",
        "x",
        "--after",
        "2026-08-02",
        "--before",
        "2026-08-01",
    ],
    &["agent", "search"],
];

/// A handle no session's digest starts with.
const UNKNOWN_HANDLE: &str = "ch_000000000000";

/// The output of each agent command, one file's worth per command.
struct AgentOutputs {
    sessions: String,
    search: String,
    within: String,
    read: String,
    outline: String,
}

impl AgentOutputs {
    /// Runs every case. The `within`, `read` and `outline` cases run on each
    /// session `agent sessions` lists, then inside [`WINDOWS`], then on
    /// refs that resolve to no session.
    fn run() -> Self {
        let mut outputs = Self {
            sessions: records_of(SESSIONS_CASES),
            search: records_of(SEARCH_CASES),
            within: String::new(),
            read: String::new(),
            outline: String::new(),
        };
        let listed = listed_sessions();
        for session in &listed {
            let reference = session.reference.as_str();
            outputs.within.push_str(&within_records_of(reference));
            outputs.read.push_str(&read_records_of(reference));
            outputs.outline.push_str(&outline_records_of(reference));
        }
        for window in &WINDOWS {
            let session = listed
                .iter()
                .find(|session| session.agent == window.agent)
                .unwrap_or_else(|| panic!("agent sessions lists a {} session", window.agent));
            let [within, read, outline] = windowed_records_of(&session.reference, window);
            outputs.within.push_str(&within);
            outputs.read.push_str(&read);
            outputs.outline.push_str(&outline);
        }
        outputs
            .within
            .push_str(&agent_command(&["agent", "within", UNKNOWN_HANDLE, "answer"]).record);
        outputs.read.push_str(&records_of(&[
            &["agent", "read", UNKNOWN_HANDLE],
            &["agent", "read", "ch_zz"],
        ]));
        outputs
            .outline
            .push_str(&agent_command(&["agent", "outline", UNKNOWN_HANDLE]).record);
        outputs
    }
}

/// The records of every case in `cases`, in order.
fn records_of(cases: &[&[&str]]) -> String {
    cases
        .iter()
        .map(|args| agent_command(args).record)
        .collect()
}

/// Every `agent within` case of the session `reference` names.
fn within_records_of(reference: &str) -> String {
    [
        vec!["agent", "within", reference, "answer"],
        vec!["agent", "within", reference, "searchable", "--top", "2"],
        vec!["agent", "within", reference, "--mode", "exact", "Sentinel"],
        vec!["agent", "within", reference, "the", "--budget", "400"],
    ]
    .iter()
    .map(|args| agent_command(args).record)
    .collect()
}

/// Every `agent read` case of the session `reference` names.
fn read_records_of(reference: &str) -> String {
    let mut records = String::new();
    let mut run = |args: &[&str]| {
        let command = agent_command(args);
        records.push_str(&command.record);
        command.stdout
    };
    run(&["agent", "read", reference]);
    let full = run(&[
        "agent",
        "read",
        reference,
        "--tools",
        "--tool-results",
        "--thinking",
        "--subagents",
        "--no-budget",
    ]);
    run(&["agent", "read", &format!("{reference}:m2..m4")]);
    run(&[
        "agent",
        "read",
        &format!("{reference}:m3"),
        "--lines",
        "1..2",
    ]);
    run(&[
        "agent",
        "read",
        &format!("{reference}:m3"),
        "--match",
        "the",
    ]);
    run(&["agent", "read", reference, "--tail", "3"]);
    run(&[
        "agent",
        "read",
        reference,
        "--tools",
        "--subagents",
        "--tail",
        "2",
    ]);
    if let Some(call) = first_call_address(&full) {
        run(&["agent", "read", &format!("{reference}:{call}")]);
    }
    if full.lines().any(|line| line.starts_with("subagent s1 ")) {
        run(&["agent", "read", &format!("{reference}:s1"), "--tools"]);
    }
    run(&[
        "agent",
        "read",
        &format!("{reference}:m1"),
        &format!("{reference}:m3"),
    ]);
    run(&[
        "agent",
        "read",
        &format!("{reference}:m1..m8"),
        "--focus",
        &format!("{reference}:m5"),
        "--budget",
        "700",
    ]);
    let budgeted = run(&["agent", "read", reference, "--budget", "900"]);
    if let Some(continued) = continue_read_args(&budgeted) {
        let continued: Vec<&str> = continued.iter().map(String::as_str).collect();
        run(&continued);
    }
    run(&["agent", "read", &format!("{reference}:m9999")]);
    run(&["agent", "read", &format!("{reference}:m5..m2")]);
    run(&["agent", "read", &format!("{reference}:s99")]);
    records
}

/// Every `agent outline` case of the session `reference` names.
fn outline_records_of(reference: &str) -> String {
    let mut records = String::new();
    let mut run = |args: &[&str]| {
        let command = agent_command(args);
        records.push_str(&command.record);
        command.stdout
    };
    let outline = run(&["agent", "outline", reference]);
    run(&[
        "agent",
        "outline",
        reference,
        "--tools",
        "--tool-results",
        "--thinking",
        "--subagents",
        "--no-budget",
    ]);
    run(&["agent", "outline", reference, "--budget", "500"]);
    run(&["agent", "outline", &format!("{reference}:m2..m5")]);
    if outline.lines().any(|line| line.starts_with("subagent s1 ")) {
        run(&["agent", "outline", &format!("{reference}:s1")]);
    }
    records
}

/// The first `mN.K` call address a read prints, as a record's address.
fn first_call_address(read: &str) -> Option<String> {
    read.lines()
        .filter_map(|line| line.split_whitespace().next())
        .find(|address| {
            address.strip_prefix('m').is_some_and(|rest| {
                rest.split_once('.').is_some_and(|(message, call)| {
                    !message.is_empty()
                        && message.bytes().all(|byte| byte.is_ascii_digit())
                        && !call.is_empty()
                        && call.bytes().all(|byte| byte.is_ascii_digit())
                })
            })
        })
        .map(str::to_owned)
}

/// The `agent read` arguments a read's `continue read` record names.
fn continue_read_args(read: &str) -> Option<Vec<String>> {
    let record = read
        .lines()
        .find(|line| line.starts_with("continue read "))?;
    let mut args = vec!["agent".to_owned(), "read".to_owned()];
    args.extend(atom(record, "refs")?.split(';').map(str::to_owned));
    for flag in ["tools", "tool-results", "thinking", "subagents"] {
        if atom(record, flag) == Some("true") {
            args.push(format!("--{flag}"));
        }
    }
    Some(args)
}

/// A session `agent sessions` lists.
struct ListedSession {
    reference: String,
    agent: String,
}

/// Every session `agent sessions --no-budget` lists, in its order.
fn listed_sessions() -> Vec<ListedSession> {
    let listing = agent_command(&["agent", "sessions", "--no-budget"]).stdout;
    listing
        .lines()
        .filter(|line| line.starts_with("session "))
        .map(|line| ListedSession {
            reference: atom(line, "ref").unwrap().to_owned(),
            agent: atom(line, "agent").unwrap().to_owned(),
        })
        .collect()
}

/// A window inside one fixture session, as `--after` and `--before` take it.
struct Window {
    agent: &'static str,
    after: &'static str,
    before: &'static str,
}

/// A few minutes inside the Claude Code and the Codex session, each holding
/// some of its messages.
const WINDOWS: [Window; 2] = [
    Window {
        agent: "claude",
        after: "2026-09-01T06:00:30",
        before: "2026-09-01T06:05",
    },
    Window {
        agent: "codex",
        after: "2026-08-01T06:01",
        before: "2026-08-01T06:02",
    },
];

/// Every windowed `within`, `read` and `outline` case of the session
/// `reference` names, inside `window`.
fn windowed_records_of(reference: &str, window: &Window) -> [String; 3] {
    let windowed = |args: &[&str]| {
        let mut args = args.to_vec();
        args.extend(["--after", window.after, "--before", window.before]);
        agent_command(&args).record
    };
    [
        windowed(&["agent", "within", reference, "the"]),
        windowed(&["agent", "read", reference, "--tools", "--tool-results"]),
        windowed(&["agent", "outline", reference]),
    ]
}

/// One agent command's run: what the binary prints for it, under a line
/// naming the command.
struct AgentCommandRun {
    record: String,
    stdout: String,
}

/// Runs `args` as the binary runs its arguments, and records what it prints:
/// its standard output, or the error it prints to standard error.
fn agent_command(args: &[&str]) -> AgentCommandRun {
    let mut record = format!("$ rearview {}\n", quoted(args));
    let mut stdout = String::new();
    let argv = std::iter::once("rearview").chain(args.iter().copied());
    match <crate::cli::Args as clap::Parser>::try_parse_from(argv) {
        Err(error) => {
            record.push_str("--- error, exit 2\n");
            record.push_str(&error.render().to_string());
        }
        Ok(parsed) => {
            let Some(crate::cli::Commands::Agent { command }) = parsed.command else {
                panic!("{args:?} is an agent command");
            };
            match crate::agent::service::execute(
                *command,
                crate::history::Environment::of_user(fixed_clock()),
            ) {
                Ok(output) => {
                    record.push_str("--- stdout\n");
                    record.push_str(&output);
                    stdout = output;
                }
                Err(error) => {
                    record.push_str("--- stderr, exit 1\n");
                    record.push_str(&crate::error_report(error));
                }
            }
        }
    }
    if !record.ends_with('\n') {
        record.push_str("\n--- no newline at the end\n");
    }
    AgentCommandRun { record, stdout }
}

/// `args` as a shell line: an argument with a space or a quote is quoted.
fn quoted(args: &[&str]) -> String {
    args.iter()
        .map(|arg| {
            if arg.is_empty() || arg.contains([' ', '"', '\'']) {
                format!("'{}'", arg.replace('\'', "'\\''"))
            } else {
                (*arg).to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
