use chrono::TimeZone;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rearview"))
}

fn run(config: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .env("CLAUDE_CONFIG_DIR", config)
        .env(
            "PI_CODING_AGENT_SESSION_DIR",
            config.join("empty-agent-sessions"),
        )
        // The spawned binary would otherwise write session caches for this
        // test's throwaway roots into the user's real cache directory.
        .env("REARVIEW_CACHE_DIR", config.join("cache"))
        .env("CODEX_HOME", config.join("empty-codex-home"))
        .env("KIMI_CODE_HOME", config.join("empty-kimi-home"))
        .env("OPENCODE_DB", config.join("empty-opencode.db"))
        .args(args)
        .output()
        .expect("run rearview")
}

fn run_pi(config: &Path, sessions: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .env("CLAUDE_CONFIG_DIR", config)
        .env("PI_CODING_AGENT_SESSION_DIR", sessions)
        .env("REARVIEW_CACHE_DIR", config.join("cache"))
        .env("CODEX_HOME", config.join("empty-codex-home"))
        .env("KIMI_CODE_HOME", config.join("empty-kimi-home"))
        .env("OPENCODE_DB", config.join("empty-opencode.db"))
        .args(args)
        .output()
        .expect("run rearview with Pi sessions")
}

fn run_codex(config: &Path, codex_home: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .env("CLAUDE_CONFIG_DIR", config)
        .env(
            "PI_CODING_AGENT_SESSION_DIR",
            config.join("empty-agent-sessions"),
        )
        .env("REARVIEW_CACHE_DIR", config.join("cache"))
        .env("CODEX_HOME", codex_home)
        .env("KIMI_CODE_HOME", config.join("empty-kimi-home"))
        .env("OPENCODE_DB", config.join("empty-opencode.db"))
        .args(args)
        .output()
        .expect("run rearview with Codex sessions")
}

fn run_kimi(config: &Path, kimi_home: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .env("CLAUDE_CONFIG_DIR", config)
        .env(
            "PI_CODING_AGENT_SESSION_DIR",
            config.join("empty-agent-sessions"),
        )
        .env("REARVIEW_CACHE_DIR", config.join("cache"))
        .env("CODEX_HOME", config.join("empty-codex-home"))
        .env("KIMI_CODE_HOME", kimi_home)
        .env("OPENCODE_DB", config.join("empty-opencode.db"))
        .args(args)
        .output()
        .expect("run rearview with Kimi sessions")
}

fn run_opencode(config: &Path, database: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .env("CLAUDE_CONFIG_DIR", config)
        .env(
            "PI_CODING_AGENT_SESSION_DIR",
            config.join("empty-agent-sessions"),
        )
        .env("REARVIEW_CACHE_DIR", config.join("cache"))
        .env("CODEX_HOME", config.join("empty-codex-home"))
        .env("KIMI_CODE_HOME", config.join("empty-kimi-home"))
        .env("OPENCODE_DB", database)
        .args(args)
        .output()
        .expect("run rearview with an OpenCode database")
}

const CODEX_THREAD: &str = "019f0000-0000-7000-8000-00000000000a";
const CODEX_SUBAGENT_THREAD: &str = "019f0000-0000-7000-8000-00000000000b";
const CODEX_MIGRATED_SUBAGENT_THREAD: &str = "019f0000-0000-7000-8000-00000000000e";

/// The dated directory a Codex rollout has to sit in: discovery lists only
/// files exactly three levels below `sessions/`.
fn codex_sessions_day(codex_home: &Path) -> PathBuf {
    let day = codex_home.join("sessions/2026/08/01");
    std::fs::create_dir_all(&day).expect("create Codex sessions tree");
    day
}

/// Copies a Codex fixture into `day` under the name Codex gives a rollout.
/// Discovery reads the thread from that name, so the name is built from the
/// id and start time in the fixture's own header.
fn copy_codex_fixture(day: &Path, fixture: &str) -> PathBuf {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/codex")
        .join(fixture);
    let text = std::fs::read_to_string(&source).expect("read the Codex fixture");
    let header: serde_json::Value =
        serde_json::from_str(text.lines().next().expect("the fixture's first line"))
            .expect("the fixture opens with its session_meta line");
    let thread_id = header["payload"]["id"]
        .as_str()
        .expect("the header names a thread");
    let stamp = header["payload"]["timestamp"]
        .as_str()
        .and_then(|started| started.get(..19))
        .expect("the header carries an RFC 3339 start time")
        .replace(':', "-");
    let rollout = day.join(format!("rollout-{stamp}-{thread_id}.jsonl"));
    std::fs::copy(&source, &rollout).expect("copy the Codex fixture");
    rollout
}

fn project(config: &Path) -> PathBuf {
    let project = config.join("projects").join("-tmp-agent-phase3-tests");
    std::fs::create_dir_all(&project).expect("create project");
    project
}

fn write_transcript(path: &Path, needle: &str) {
    write_transcript_at(
        path,
        needle,
        &chrono::Local::now().format("%Y-%m-%d").to_string(),
    );
}

fn set_modified(path: &Path, date: &str) {
    let midnight = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .expect("parse date")
        .and_hms_opt(0, 0, 0)
        .expect("midnight is a valid time");
    let at = chrono::Local
        .from_local_datetime(&midnight)
        .single()
        .expect("local midnight is unambiguous on these dates");
    std::fs::File::options()
        // Windows sets the timestamp through the handle, so a read-only one
        // cannot.
        .write(true)
        .open(path)
        .expect("open transcript")
        .set_modified(std::time::SystemTime::from(at))
        .expect("set modification time");
}

/// Write a transcript dated `date`, in its records and in its modification
/// time — the mtime being where a conversation's timestamp comes from, rather
/// than the records inside it.
fn write_transcript_at(path: &Path, needle: &str, date: &str) {
    let user = serde_json::json!({
        "type": "user",
        "timestamp": format!("{date}T00:00:00Z"),
        "cwd": "/tmp/agent-phase3-tests",
        "message": {"role": "user", "content": needle}
    });
    let assistant = serde_json::json!({
        "type": "assistant",
        "timestamp": format!("{date}T00:00:01Z"),
        "message": {"role": "assistant", "content": [{"type": "text", "text": "answer"}]}
    });
    std::fs::write(path, format!("{user}\n{assistant}\n")).expect("write transcript");
    set_modified(path, date);
}

fn first_ref(output: &str) -> String {
    output
        .split_whitespace()
        .find_map(|field| field.strip_prefix("ref="))
        .expect("search ref")
        .trim_end_matches(|character: char| !character.is_ascii_hexdigit())
        .to_string()
}

/// The command's stdout. A run that failed is reported as its stderr, which
/// carries the diagnostic the CLI printed.
#[track_caller]
fn stdout_of(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A missing or leaked needle prints the output that was searched;
/// `assert!(text.contains(needle))` alone prints neither.
#[track_caller]
fn assert_shows(output: &str, needle: &str) {
    assert!(
        output.contains(needle),
        "{needle:?} missing from:\n{output}"
    );
}

#[track_caller]
fn assert_hides(output: &str, needle: &str) {
    assert!(
        !output.contains(needle),
        "{needle:?} leaked into:\n{output}"
    );
}

#[test]
fn pi_sessions_support_agent_search_read_and_outline_without_claude_storage() {
    let config = tempfile::tempdir().expect("config");
    let sessions = tempfile::tempdir().expect("sessions");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl"),
        sessions.path().join("pi.jsonl"),
    )
    .expect("copy Pi fixture");

    let search_text = stdout_of(&run_pi(
        config.path(),
        sessions.path(),
        &["agent", "search", "--lexical", "active root question"],
    ));
    assert_shows(&search_text, "uuid=01912345-6789-7abc-8def-0123456789ab");
    assert_hides(&search_text, "ABANDONED_BRANCH_SENTINEL");
    let reference = first_ref(&search_text);

    let read_text = stdout_of(&run_pi(
        config.path(),
        sessions.path(),
        &["agent", "read", &reference],
    ));
    assert_shows(&read_text, "active root question");
    assert_hides(&read_text, "compaction summary searchable");
    assert_hides(&read_text, "branch summary searchable");
    assert_hides(&read_text, "ABANDONED_BRANCH_SENTINEL");

    let outline_text = stdout_of(&run_pi(
        config.path(),
        sessions.path(),
        &["agent", "outline", &reference],
    ));
    assert_shows(&outline_text, "active root question");
}

#[test]
fn omp_sessions_support_agent_search_and_direct_render() {
    let config = tempfile::tempdir().expect("config");
    let sessions = tempfile::tempdir().expect("sessions");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/v3.jsonl");
    std::fs::copy(&fixture, sessions.path().join("omp.jsonl")).expect("copy OMP fixture");

    let search_text = stdout_of(&run_pi(
        config.path(),
        sessions.path(),
        &["agent", "search", "--lexical", "OMP active question"],
    ));
    assert_shows(&search_text, "uuid=omp_session_custom_id");
    assert_hides(&search_text, "OMP_ABANDONED_SENTINEL");
    let reference = first_ref(&search_text);

    let read_text = stdout_of(&run_pi(
        config.path(),
        sessions.path(),
        &["agent", "read", &reference],
    ));
    assert_shows(&read_text, "OMP active answer");

    let rendered = stdout_of(
        &Command::new(binary())
            .args(["--no-color", "--render"])
            .arg(fixture)
            .output()
            .expect("render OMP fixture"),
    );
    assert_shows(&rendered, "OMP");
    assert_shows(&rendered, "OMP active question");
    assert_hides(&rendered, "OMP_ABANDONED_SENTINEL");
    assert_hides(&rendered, "Mode change");
}

#[test]
fn codex_sessions_support_agent_search_read_and_direct_render() {
    let config = tempfile::tempdir().expect("config");
    let codex_home = tempfile::tempdir().expect("codex home");
    let day = codex_sessions_day(codex_home.path());
    let transcript = copy_codex_fixture(&day, "rollout.jsonl");
    copy_codex_fixture(&day, "subagent.jsonl");

    let search_text = stdout_of(&run_codex(
        config.path(),
        codex_home.path(),
        &["agent", "search", "--lexical", "active codex question"],
    ));
    assert_shows(&search_text, &format!("uuid={CODEX_THREAD}"));
    assert_hides(&search_text, "ENV_CONTEXT_SENTINEL");
    assert!(
        !search_text.contains("kind=skipped"),
        "a folded sub-agent thread is reachable through its parent, not skipped: {search_text}"
    );
    let reference = first_ref(&search_text);

    // The sub-agent thread has no row and no key of its own; its text hits
    // through the session that spawned it, anchored to the spliced entries.
    let folded_text = stdout_of(&run_codex(
        config.path(),
        codex_home.path(),
        &["agent", "search", "--lexical", "child answer searchable"],
    ));
    assert_shows(&folded_text, &format!("uuid={CODEX_THREAD}"));
    assert_hides(&folded_text, &format!("uuid={CODEX_SUBAGENT_THREAD}"));

    let read_text = stdout_of(&run_codex(
        config.path(),
        codex_home.path(),
        &["agent", "read", &reference],
    ));
    assert_shows(&read_text, "codex answer searchable");

    // Without any cache — the searches above populated it under the config
    // directory — a read must still resolve the ref by parsing, the cost the
    // first load pays anyway.
    std::fs::remove_dir_all(config.path().join("cache")).expect("drop cache");
    let cold_read_text = stdout_of(&run_codex(
        config.path(),
        codex_home.path(),
        &["agent", "read", &reference],
    ));
    assert_shows(&cold_read_text, "codex answer searchable");

    let rendered = stdout_of(
        &Command::new(binary())
            .args(["--no-color", "--render"])
            .arg(&transcript)
            .output()
            .expect("render Codex fixture"),
    );
    assert_shows(&rendered, "active codex question");
    assert_shows(&rendered, "codex answer searchable");
    assert_hides(&rendered, "ENV_CONTEXT_SENTINEL");
    assert_hides(&rendered, "DEVELOPER_SENTINEL");
    assert!(
        !rendered.contains("child answer searchable"),
        "spliced sub-agent turns hide behind the thinking toggle, as for Claude: {rendered}"
    );
}

/// Codex's legacy-to-paginated migration marks a sub-agent rollout's whole
/// file as inherited context; the thread's text still hits through its parent.
#[test]
fn a_migrated_codex_subagent_thread_is_searchable_through_its_parent() {
    let config = tempfile::tempdir().expect("config");
    let codex_home = tempfile::tempdir().expect("codex home");
    let day = codex_sessions_day(codex_home.path());
    copy_codex_fixture(&day, "rollout.jsonl");
    copy_codex_fixture(&day, "subagent-migrated.jsonl");

    let search_text = stdout_of(&run_codex(
        config.path(),
        codex_home.path(),
        &["agent", "search", "--lexical", "migrated answer searchable"],
    ));

    assert_shows(&search_text, &format!("uuid={CODEX_THREAD}"));
    assert_hides(
        &search_text,
        &format!("uuid={CODEX_MIGRATED_SUBAGENT_THREAD}"),
    );
    assert_hides(&search_text, "kind=skipped");
}

/// A Codex sub-agent thread is a rollout of its own. Left behind, it would
/// list as a session the user never started.
#[test]
fn deleting_a_codex_thread_removes_its_subagent_threads() {
    let config = tempfile::tempdir().expect("config");
    let codex_home = tempfile::tempdir().expect("codex home");
    let day = codex_sessions_day(codex_home.path());
    let parent = copy_codex_fixture(&day, "rollout.jsonl");
    let subagent = copy_codex_fixture(&day, "subagent.jsonl");

    let delete = run_codex(
        config.path(),
        codex_home.path(),
        &["--delete", CODEX_THREAD],
    );
    let delete_stderr = String::from_utf8_lossy(&delete.stderr);
    assert!(delete.status.success(), "{delete_stderr}");
    assert!(
        delete_stderr.contains(&format!(
            "Deleted Codex session {CODEX_THREAD} (1 sub-agent session)"
        )),
        "{delete_stderr}"
    );
    assert!(!parent.exists());
    assert!(
        !subagent.exists(),
        "the sub-agent thread's rollout is deleted with its parent"
    );

    // The delete emptied the sessions tree, so this run exits with "no
    // history storage is available"; only its stdout carries the claim.
    let search = run_codex(
        config.path(),
        codex_home.path(),
        &["agent", "search", "--lexical", "child answer searchable"],
    );
    let search_text = String::from_utf8_lossy(&search.stdout);
    assert!(
        !search_text.contains("uuid="),
        "the sub-agent thread must not come back as a session of its own: {search_text}"
    );
}

const CLAUDE_SESSION_WITH_SUBAGENTS: &str = "7b2f3c1e-4a5d-4e6f-8a9b-0c1d2e3f4a5b";

/// Copies the fixture project that holds a Claude session with two
/// sub-agent transcripts, one of them nested, into `config`'s projects.
/// Returns the session's transcript.
fn copy_claude_subagent_fixture(config: &Path) -> PathBuf {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude/-tmp-claude-subagent-fixture");
    let project = config.join("projects").join("-tmp-claude-subagent-fixture");
    copy_dir(&source, &project);
    project.join(format!("{CLAUDE_SESSION_WITH_SUBAGENTS}.jsonl"))
}

fn copy_dir(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("create the directory");
    for entry in std::fs::read_dir(source).expect("read the fixture directory") {
        let entry = entry.expect("read the fixture entry");
        let target = destination.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy the fixture file");
        }
    }
}

/// A Claude sub-agent transcript has no row of its own; its text hits
/// through the session that ran it, and `--render` splices it in behind
/// the thinking toggle.
#[test]
fn claude_subagent_transcripts_are_searchable_through_their_session() {
    let config = tempfile::tempdir().expect("config");
    let transcript = copy_claude_subagent_fixture(config.path());

    for sentinel in ["EXPLORE_SUBAGENT_SENTINEL", "NESTED_SUBAGENT_SENTINEL"] {
        let search_text = stdout_of(&run(
            config.path(),
            &["agent", "search", "--lexical", sentinel],
        ));
        assert_shows(
            &search_text,
            &format!("uuid={CLAUDE_SESSION_WITH_SUBAGENTS}"),
        );
        assert_hides(&search_text, "kind=skipped");
    }

    let rendered = stdout_of(
        &Command::new(binary())
            .args(["--no-color", "--render"])
            .arg(&transcript)
            .env("CLAUDE_CONFIG_DIR", config.path())
            .output()
            .expect("render the Claude fixture"),
    );
    assert_shows(&rendered, "PARENT_ANSWER_SENTINEL");
    assert!(
        !rendered.contains("EXPLORE_SUBAGENT_SENTINEL"),
        "spliced sub-agent turns hide behind the thinking toggle: {rendered}"
    );

    let rendered_with_thinking = stdout_of(
        &Command::new(binary())
            .args(["--no-color", "--show-thinking", "--render"])
            .arg(&transcript)
            .env("CLAUDE_CONFIG_DIR", config.path())
            .output()
            .expect("render the Claude fixture with thinking"),
    );
    assert_shows(&rendered_with_thinking, "EXPLORE_SUBAGENT_SENTINEL");
    assert_shows(&rendered_with_thinking, "NESTED_SUBAGENT_SENTINEL");
}

/// The session directory holds the sub-agent transcripts, so they are
/// deleted with the session, and the delete reports them.
#[test]
fn deleting_a_claude_session_removes_its_subagent_transcripts() {
    let config = tempfile::tempdir().expect("config");
    let transcript = copy_claude_subagent_fixture(config.path());
    let session_dir = transcript.with_extension("");
    assert!(session_dir.join("subagents").is_dir());

    let delete = run(config.path(), &["--delete", CLAUDE_SESSION_WITH_SUBAGENTS]);
    let delete_stderr = String::from_utf8_lossy(&delete.stderr);
    assert!(delete.status.success(), "{delete_stderr}");
    assert!(
        delete_stderr.contains(&format!(
            "Deleted Claude session {CLAUDE_SESSION_WITH_SUBAGENTS} (3 sub-agent sessions)"
        )),
        "{delete_stderr}"
    );
    assert!(!transcript.exists());
    assert!(
        !session_dir.exists(),
        "the session directory is deleted with the transcript"
    );
}

/// A Guardian review is a thread Codex runs on a session for itself, and its
/// rollout restates the session's conversation. Reading it would index the
/// session twice.
#[test]
fn a_codex_guardian_review_is_neither_listed_nor_searched() {
    let config = tempfile::tempdir().expect("config");
    let codex_home = tempfile::tempdir().expect("codex home");
    let day = codex_sessions_day(codex_home.path());
    copy_codex_fixture(&day, "rollout.jsonl");
    copy_codex_fixture(&day, "guardian.jsonl");

    let search_text = stdout_of(&run_codex(
        config.path(),
        codex_home.path(),
        &["agent", "search", "--lexical", "active codex question"],
    ));
    assert_shows(&search_text, &format!("uuid={CODEX_THREAD}"));
    assert_hides(&search_text, "uuid=019f0000-0000-7000-8000-000000000010");
    assert_hides(&search_text, "kind=ignored");

    let review_text = stdout_of(&run_codex(
        config.path(),
        codex_home.path(),
        &["agent", "search", "--lexical", "GUARDIAN_REVIEW_SENTINEL"],
    ));
    assert!(
        !review_text.contains("uuid="),
        "a Guardian review's text is reachable through no session: {review_text}"
    );
}

/// Codex's compression feature rewrites rollouts older than a week to
/// `.jsonl.zst`, which rearview ignores. The search reports it, or an agent
/// would read a history that stops a week back as complete.
#[test]
fn a_codex_search_reports_the_compressed_sessions_it_ignored() {
    let config = tempfile::tempdir().expect("config");
    let codex_home = tempfile::tempdir().expect("codex home");
    let day = codex_sessions_day(codex_home.path());
    copy_codex_fixture(&day, "rollout.jsonl");
    std::fs::write(
        day.join("rollout-2026-07-01T10-00-00-019f0000-0000-7000-8000-00000000000c.jsonl.zst"),
        "never decoded",
    )
    .expect("write a compressed rollout");

    let search_text = stdout_of(&run_codex(
        config.path(),
        codex_home.path(),
        &["agent", "search", "--lexical", "active codex question"],
    ));
    assert!(
        search_text.contains(&format!("uuid={CODEX_THREAD}")),
        "the plain rollout is still searched: {search_text}"
    );
    assert_shows(
        &search_text,
        "protocol agent-warning kind=ignored detail=Codex:%201%20ignored:%20compressed%20sessions%20unsupported",
    );
}

#[test]
fn kimi_sessions_support_agent_search_read_and_direct_render() {
    const SESSION: &str = "session_0f000000-0000-4000-8000-000000000001";
    let config = tempfile::tempdir().expect("config");
    let kimi_home = tempfile::tempdir().expect("kimi home");
    let session_dir = kimi_home
        .path()
        .join("sessions/wd_kimi-project_abc123")
        .join(SESSION);
    std::fs::create_dir_all(session_dir.join("agents/main")).expect("create session tree");
    std::fs::create_dir_all(session_dir.join("agents/agent-0")).expect("create sub-agent dir");
    std::fs::write(
        session_dir.join("state.json"),
        format!(
            concat!(
                "{{\"id\":\"{id}\",\"version\":2,\"cwd\":\"/tmp/kimi-project\",",
                "\"createdAt\":1786010400000,",
                "\"agents\":{{\"main\":{{\"type\":\"main\"}},",
                "\"agent-0\":{{\"type\":\"sub\",\"parentAgentId\":\"main\"}}}},",
                "\"title\":\"kimi e2e session\",\"isCustomTitle\":false}}",
            ),
            id = SESSION
        ),
    )
    .expect("write state.json");
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kimi");
    let transcript = session_dir.join("agents/main/wire.jsonl");
    std::fs::copy(fixtures.join("wire.jsonl"), &transcript).expect("copy Kimi fixture");
    std::fs::copy(
        fixtures.join("subagent-wire.jsonl"),
        session_dir.join("agents/agent-0/wire.jsonl"),
    )
    .expect("copy Kimi sub-agent fixture");

    let search_text = stdout_of(&run_kimi(
        config.path(),
        kimi_home.path(),
        &["agent", "search", "--lexical", "active kimi question"],
    ));
    assert_shows(&search_text, &format!("uuid={SESSION}"));
    assert_hides(&search_text, "SYSTEM_REMINDER_SENTINEL");
    assert!(
        !search_text.contains("kind=skipped"),
        "a folded sub-agent wire is reachable through its session, not skipped: {search_text}"
    );
    let reference = first_ref(&search_text);

    // The sub-agent wire has no row and no key of its own; its text hits
    // through the session that spawned it, anchored to the spliced entries.
    let folded_text = stdout_of(&run_kimi(
        config.path(),
        kimi_home.path(),
        &[
            "agent",
            "search",
            "--lexical",
            "kimi child answer searchable",
        ],
    ));
    assert_shows(&folded_text, &format!("uuid={SESSION}"));
    assert_hides(
        &folded_text,
        "uuid=session_0f000000-0000-4000-8000-000000000001#agent-0",
    );

    let read_text = stdout_of(&run_kimi(
        config.path(),
        kimi_home.path(),
        &["agent", "read", &reference],
    ));
    assert_shows(&read_text, "kimi answer searchable");

    // Without any cache — the searches above populated it under the config
    // directory — a read must still resolve the ref by parsing, the cost the
    // first load pays anyway.
    std::fs::remove_dir_all(config.path().join("cache")).expect("drop cache");
    let cold_read_text = stdout_of(&run_kimi(
        config.path(),
        kimi_home.path(),
        &["agent", "read", &reference],
    ));
    assert_shows(&cold_read_text, "kimi answer searchable");

    let rendered = stdout_of(
        &Command::new(binary())
            .args(["--no-color", "--render"])
            .arg(&transcript)
            .output()
            .expect("render Kimi fixture"),
    );
    assert_shows(&rendered, "active kimi question");
    assert_shows(&rendered, "kimi answer searchable");
    assert_hides(&rendered, "SYSTEM_REMINDER_SENTINEL");
    assert_hides(&rendered, "NOTE_SENTINEL");
    assert!(
        !rendered.contains("kimi child answer searchable"),
        "spliced sub-agent turns hide behind the thinking toggle, as for Claude: {rendered}"
    );
}

#[test]
fn opencode_sessions_support_agent_search_read_and_direct_render() {
    let config = tempfile::tempdir().expect("config");
    let store = tempfile::tempdir().expect("opencode data dir");
    let database = store.path().join("opencode.db");

    // A deliberate twin of the schema transcription in
    // src/history/format/opencode.rs's fixture module, which this
    // integration crate cannot reach; both transcribe ../opencode's
    // schema.gen.ts.
    let connection = rusqlite::Connection::open(&database).expect("create database");
    connection
        .execute_batch(
            r#"
            CREATE TABLE `project` (
              `id` text PRIMARY KEY, `worktree` text NOT NULL, `vcs` text,
              `name` text, `icon_url` text, `icon_url_override` text,
              `icon_color` text, `time_created` integer NOT NULL,
              `time_updated` integer NOT NULL, `time_initialized` integer,
              `sandboxes` text NOT NULL, `commands` text
            );
            CREATE TABLE `session` (
              `id` text PRIMARY KEY, `project_id` text NOT NULL,
              `workspace_id` text, `parent_id` text, `slug` text NOT NULL,
              `directory` text NOT NULL, `path` text, `title` text NOT NULL,
              `version` text NOT NULL, `share_url` text,
              `summary_additions` integer, `summary_deletions` integer,
              `summary_files` integer, `summary_diffs` text, `metadata` text,
              `cost` real DEFAULT 0 NOT NULL,
              `tokens_input` integer DEFAULT 0 NOT NULL,
              `tokens_output` integer DEFAULT 0 NOT NULL,
              `tokens_reasoning` integer DEFAULT 0 NOT NULL,
              `tokens_cache_read` integer DEFAULT 0 NOT NULL,
              `tokens_cache_write` integer DEFAULT 0 NOT NULL,
              `revert` text, `permission` text, `agent` text, `model` text,
              `time_created` integer NOT NULL, `time_updated` integer NOT NULL,
              `time_compacting` integer, `time_archived` integer
            );
            CREATE TABLE `message` (
              `id` text PRIMARY KEY, `session_id` text NOT NULL,
              `time_created` integer NOT NULL, `time_updated` integer NOT NULL,
              `data` text NOT NULL
            );
            CREATE TABLE `part` (
              `id` text PRIMARY KEY, `message_id` text NOT NULL,
              `session_id` text NOT NULL, `time_created` integer NOT NULL,
              `time_updated` integer NOT NULL, `data` text NOT NULL
            );
            INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
            VALUES ('proj_e2e', '/tmp/opencode-project', 1755000000000, 1755000000000, '[]');
            INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated)
            VALUES ('ses_e2e_parent', 'proj_e2e', NULL, 'e2e', '/tmp/opencode-project', 'opencode e2e session', '1.0.0', 1755000100000, 1755000400000),
                   ('ses_e2e_child', 'proj_e2e', 'ses_e2e_parent', 'e2e-child', '/tmp/opencode-project', '', '1.0.0', 1755000150000, 1755000300000);
            INSERT INTO message (id, session_id, time_created, time_updated, data)
            VALUES ('msg_user', 'ses_e2e_parent', 1755000100000, 1755000100000,
                    '{"role":"user","time":{"created":1755000100000}}'),
                   ('msg_asst', 'ses_e2e_parent', 1755000200000, 1755000200000,
                    '{"role":"assistant","time":{"created":1755000200000},"modelID":"claude-opus-4-6","providerID":"anthropic"}'),
                   ('msg_child_user', 'ses_e2e_child', 1755000150000, 1755000150000,
                    '{"role":"user","time":{"created":1755000150000}}'),
                   ('msg_child_asst', 'ses_e2e_child', 1755000160000, 1755000160000,
                    '{"role":"assistant","time":{"created":1755000160000}}');
            INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
            VALUES ('prt_user', 'msg_user', 'ses_e2e_parent', 1755000100000, 1755000100000,
                    '{"type":"text","text":"active opencode question","time":{"start":1755000100000}}'),
                   ('prt_user_s1_call', 'msg_user', 'ses_e2e_parent', 1755000100000, 1755000100000,
                    '{"type":"text","synthetic":true,"text":"Called the Read tool with the following input: {\"filePath\":\"/tmp/opencode-project/README.md\"}"}'),
                   ('prt_user_s2_body', 'msg_user', 'ses_e2e_parent', 1755000100000, 1755000100000,
                    '{"type":"text","synthetic":true,"text":"SYNTHETIC_SENTINEL dumped file body"}'),
                   ('prt_framing', 'msg_asst', 'ses_e2e_parent', 1755000200000, 1755000200000,
                    '{"type":"step-start","snapshot":"SNAPSHOT_SENTINEL"}'),
                   ('prt_answer', 'msg_asst', 'ses_e2e_parent', 1755000210000, 1755000210000,
                    '{"type":"text","text":"opencode answer searchable","time":{"start":1755000210000}}'),
                   ('prt_child_user', 'msg_child_user', 'ses_e2e_child', 1755000150000, 1755000150000,
                    '{"type":"text","text":"child prompt","time":{"start":1755000150000}}'),
                   ('prt_child_answer', 'msg_child_asst', 'ses_e2e_child', 1755000160000, 1755000160000,
                    '{"type":"text","text":"opencode child answer searchable","time":{"start":1755000160000}}');
            "#,
        )
        .expect("populate database");
    drop(connection);

    let search_text = stdout_of(&run_opencode(
        config.path(),
        &database,
        &["agent", "search", "--lexical", "active opencode question"],
    ));
    assert_shows(&search_text, "uuid=ses_e2e_parent");
    assert!(
        !search_text.contains("kind=skipped"),
        "a folded child session is reachable through its parent, not skipped: {search_text}"
    );
    let synthetic = stdout_of(&run_opencode(
        config.path(),
        &database,
        &["agent", "search", "--lexical", "SYNTHETIC_SENTINEL"],
    ));
    assert!(
        synthetic.contains("uuid=ses_e2e_parent"),
        "an @-file injection indexes as tool output, like a real read: {synthetic}"
    );
    let reference = first_ref(&search_text);

    // The child session has no row and no key of its own; its text hits
    // through the session that spawned it.
    let folded_text = stdout_of(&run_opencode(
        config.path(),
        &database,
        &[
            "agent",
            "search",
            "--lexical",
            "opencode child answer searchable",
        ],
    ));
    assert_shows(&folded_text, "uuid=ses_e2e_parent");
    assert_hides(&folded_text, "uuid=ses_e2e_child");

    let read_text = stdout_of(&run_opencode(
        config.path(),
        &database,
        &["agent", "read", &reference],
    ));
    assert_shows(&read_text, "opencode answer searchable");

    // Without any cache — the searches above populated it under the config
    // directory — a read must still resolve the ref by parsing.
    std::fs::remove_dir_all(config.path().join("cache")).expect("drop cache");
    let cold_read_text = stdout_of(&run_opencode(
        config.path(),
        &database,
        &["agent", "read", &reference],
    ));
    assert_shows(&cold_read_text, "opencode answer searchable");

    // A locator is renderable like a file: the sniffed path decodes it
    // because its parent is the database file.
    let rendered = stdout_of(
        &Command::new(binary())
            .args(["--no-color", "--render"])
            .arg(database.join("ses_e2e_parent.jsonl"))
            .output()
            .expect("render OpenCode locator"),
    );
    assert_shows(&rendered, "active opencode question");
    assert_shows(&rendered, "opencode answer searchable");
    assert!(
        !rendered.contains("SYNTHETIC_SENTINEL"),
        "an @-file injection hides with the other tool output by default: {rendered}"
    );

    // With tools shown, the injection renders as the read it narrates.
    let rendered_tools = stdout_of(
        &Command::new(binary())
            .args(["--no-color", "--show-tools", "--render"])
            .arg(database.join("ses_e2e_parent.jsonl"))
            .output()
            .expect("render OpenCode locator with tools"),
    );
    assert_shows(&rendered_tools, "SYNTHETIC_SENTINEL");
    assert!(
        !rendered_tools.contains("Called the Read tool"),
        "the narration is the call, not the user's words: {rendered_tools}"
    );
    assert_hides(&rendered, "SNAPSHOT_SENTINEL");
    assert!(
        !rendered.contains("opencode child answer searchable"),
        "spliced child turns hide behind the thinking toggle, as for Claude: {rendered}"
    );
}

#[test]
fn direct_render_supports_pi_active_branch() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl");
    let rendered = stdout_of(
        &Command::new(binary())
            .args(["--no-color", "--render"])
            .arg(path)
            .output()
            .expect("render Pi fixture"),
    );

    assert_shows(&rendered, "Pi");
    assert_shows(&rendered, "active root question");
    for metadata in [
        "Branch summary",
        "Compaction",
        "Thinking level",
        "Model",
        "Label",
    ] {
        assert_hides(&rendered, metadata);
    }
    assert_hides(&rendered, "ABANDONED_BRANCH_SENTINEL");
}

#[test]
fn malformed_and_missing_refs_have_structured_stderr_and_nonzero_exit() {
    let config = tempfile::tempdir().expect("config");
    project(config.path());

    let invalid = run(config.path(), &["agent", "read", "not-a-ref"]);
    assert!(!invalid.status.success());
    assert!(invalid.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&invalid.stderr)
            .starts_with("protocol agent-error kind=invalid-ref ref=not-a-ref")
    );

    let missing = run(config.path(), &["agent", "read", "ch_12345678"]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .starts_with("protocol agent-error kind=not-found ref=ch_12345678")
    );
}

#[test]
fn target_transcript_and_range_failures_have_precise_kinds() {
    let config = tempfile::tempdir().expect("config");
    let transcript = project(config.path()).join("12345678-1234-4234-9234-123456789abc.jsonl");
    write_transcript(&transcript, "phase three needle");

    let search_text = stdout_of(&run(
        config.path(),
        &["agent", "search", "--lexical", "phase three needle"],
    ));
    let reference = first_ref(&search_text);

    let range = run(
        config.path(),
        &["agent", "read", &format!("{reference}:m99")],
    );
    assert!(!range.status.success());
    assert!(String::from_utf8_lossy(&range.stderr).starts_with(&format!(
        "protocol agent-error kind=out-of-range ref={reference}"
    )));

    std::fs::write(&transcript, "{malformed\n").expect("malform transcript");
    let malformed = run(config.path(), &["agent", "read", &reference]);
    assert!(!malformed.status.success());
    assert!(
        String::from_utf8_lossy(&malformed.stderr).starts_with(&format!(
            "protocol agent-error kind=malformed-transcript ref={reference}"
        ))
    );
}

#[test]
fn search_reports_partial_warnings_and_preserves_compact_success_output() {
    let config = tempfile::tempdir().expect("config");
    let project = project(config.path());
    write_transcript(
        &project.join("12345678-1234-4234-9234-123456789abc.jsonl"),
        "warning contract needle",
    );
    std::fs::write(
        project.join("87654321-1234-4234-9234-123456789abc.jsonl"),
        "{malformed\n",
    )
    .expect("write malformed transcript");

    let output = run(
        config.path(),
        &["agent", "search", "--lexical", "warning contract needle"],
    );
    let stdout = stdout_of(&output);
    assert!(output.stderr.is_empty());
    assert!(stdout.starts_with("protocol agent-search mode=lexical"));
    assert_shows(
        &stdout,
        "protocol agent-warning kind=malformed-transcript ref=ch_",
    );
    assert_shows(&stdout, "read ref=ch_");
}

#[test]
fn ref_only_commands_parse_only_the_selected_transcript() {
    let config = tempfile::tempdir().expect("config");
    let project = project(config.path());
    let selected = project.join("12345678-1234-4234-9234-123456789abc.jsonl");
    write_transcript(&selected, "selected transcript needle");

    let search_text = stdout_of(&run(
        config.path(),
        &["agent", "search", "--lexical", "selected transcript needle"],
    ));
    let reference = first_ref(&search_text);
    std::fs::write(
        project.join("87654321-1234-4234-9234-123456789abc.jsonl"),
        "{malformed\n",
    )
    .expect("write unrelated malformed transcript");

    let outline = run(config.path(), &["agent", "outline", &reference]);

    let stdout = stdout_of(&outline);
    assert!(outline.stderr.is_empty());
    assert_shows(&stdout, "m1 role=user");
    assert_shows(&stdout, "m2 role=assistant");
    assert_hides(&stdout, "malformed-transcript");
}

#[test]
fn selected_partial_transcript_recovers_records_and_reports_warning() {
    let config = tempfile::tempdir().expect("config");
    let transcript = project(config.path()).join("12345678-1234-4234-9234-123456789abc.jsonl");
    write_transcript(&transcript, "partial transcript needle");
    let search_text = stdout_of(&run(
        config.path(),
        &["agent", "search", "--lexical", "partial transcript needle"],
    ));
    let reference = first_ref(&search_text);
    let content = std::fs::read_to_string(&transcript).expect("read transcript");
    let (first, second) = content.split_once('\n').expect("two records");
    std::fs::write(&transcript, format!("{first}\n{{malformed\n{second}"))
        .expect("write partial transcript");

    let search_stdout = stdout_of(&run(
        config.path(),
        &["agent", "search", "--lexical", "partial transcript needle"],
    ));
    assert_shows(&search_stdout, "focus=m1..m1");
    assert_shows(&search_stdout, "kind=malformed-transcript");

    let within_text = stdout_of(&run(
        config.path(),
        &[
            "agent",
            "within",
            &reference,
            "partial transcript needle",
            "--lexical",
        ],
    ));
    assert_shows(&within_text, "focus=m1..m1");

    let read_text = stdout_of(&run(
        config.path(),
        &["agent", "read", &format!("{reference}:m1")],
    ));
    assert_shows(&read_text, "partial transcript needle");

    let stdout = stdout_of(&run(config.path(), &["agent", "outline", &reference]));

    assert_shows(&stdout, "warnings=1");
    assert_shows(&stdout, "kind=malformed-transcript");
    assert_shows(&stdout, "m1 role=user");
    assert_shows(&stdout, "m2 role=assistant");

    let bounded_stdout = stdout_of(&run(
        config.path(),
        &["agent", "read", &reference, "--budget", "180"],
    ));
    assert!(bounded_stdout.chars().count() <= 180);
    assert_shows(&bounded_stdout, "warnings=1");
    assert_shows(&bounded_stdout, "continue read");
    assert_eq!(bounded_stdout.lines().count(), 2);
}

#[test]
fn agent_filesystem_failures_use_io_envelope() {
    let config = tempfile::tempdir().expect("config");
    std::fs::write(config.path().join("projects"), "not a directory").expect("write projects file");

    let output = run(config.path(), &["agent", "search", "--lexical", "needle"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("protocol agent-error kind=io"));
}

#[test]
fn search_time_range_narrows_the_corpus_without_reporting_skips() {
    let config = tempfile::tempdir().expect("config");
    let project = project(config.path());

    let recent = project.join("11111111-1111-4111-9111-111111111111.jsonl");
    let old = project.join("22222222-2222-4222-9222-222222222222.jsonl");
    write_transcript_at(&recent, "time filter needle", "2026-07-20");
    write_transcript_at(&old, "time filter needle", "2020-01-15");

    let unfiltered_stdout = stdout_of(&run(
        config.path(),
        &["agent", "search", "--lexical", "time filter needle"],
    ));
    assert_shows(&unfiltered_stdout, "uuid=11111111");
    assert_shows(&unfiltered_stdout, "uuid=22222222");

    let filtered_stdout = stdout_of(&run(
        config.path(),
        &[
            "agent",
            "search",
            "--lexical",
            "time filter needle",
            "--since",
            "2026-01-01",
        ],
    ));
    assert_shows(&filtered_stdout, "uuid=11111111");
    assert!(
        !filtered_stdout.contains("uuid=22222222"),
        "out-of-window conversation still returned: {filtered_stdout}"
    );

    // Key discovery walks the projects directory independently of the time
    // filter, so an unfiltered key list would report every excluded
    // conversation as a skipped transcript and claim partial coverage.
    assert!(
        !filtered_stdout.contains("kind=skipped"),
        "filtered-out conversations were reported as skipped: {filtered_stdout}"
    );

    // The converse: narrowing the key list must not hide diagnostics for files
    // that are inside the window but failed to parse, or a filtered search would
    // claim full coverage it does not have.
    let unparseable = project.join("33333333-3333-4333-9333-333333333333.jsonl");
    std::fs::write(&unparseable, "{malformed\n").expect("write malformed transcript");
    set_modified(&unparseable, "2026-07-20");

    let with_malformed_stdout = stdout_of(&run(
        config.path(),
        &[
            "agent",
            "search",
            "--lexical",
            "time filter needle",
            "--since",
            "2026-01-01",
        ],
    ));
    assert!(
        with_malformed_stdout.contains("kind=malformed-transcript"),
        "in-window malformed transcript was silently dropped: {with_malformed_stdout}"
    );
}

#[test]
fn search_rejects_an_inverted_time_range() {
    let config = tempfile::tempdir().expect("config");
    let transcript = project(config.path()).join("33333333-3333-4333-9333-333333333333.jsonl");
    write_transcript(&transcript, "inverted range needle");

    let output = run(
        config.path(),
        &[
            "agent",
            "search",
            "--lexical",
            "inverted range needle",
            "--after",
            "2026-07-20",
            "--before",
            "2026-01-01",
        ],
    );

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .starts_with("protocol agent-error kind=out-of-range"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
