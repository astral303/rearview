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
        .env("CLAUDE_HISTORY_CACHE_DIR", config.join("cache"))
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
        .env("CLAUDE_HISTORY_CACHE_DIR", config.join("cache"))
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
        .env("CLAUDE_HISTORY_CACHE_DIR", config.join("cache"))
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
        .env("CLAUDE_HISTORY_CACHE_DIR", config.join("cache"))
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
        .env("CLAUDE_HISTORY_CACHE_DIR", config.join("cache"))
        .env("CODEX_HOME", config.join("empty-codex-home"))
        .env("KIMI_CODE_HOME", config.join("empty-kimi-home"))
        .env("OPENCODE_DB", database)
        .args(args)
        .output()
        .expect("run rearview with an OpenCode database")
}

fn project(config: &Path) -> PathBuf {
    let project = config.join("projects").join("-tmp-agent-phase3-tests");
    std::fs::create_dir_all(&project).expect("create project");
    project
}

fn write_transcript(path: &Path, needle: &str) {
    write_transcript_at(path, needle, "2026-07-20");
}

/// Backdate a transcript's modification time.
///
/// Conversation timestamps come from the file's mtime (see
/// `history::parser`), not from the records inside it, so time filtering can
/// only be exercised by changing the mtime. `stamp` is `YYYYMMDDhhmm`.
fn set_modified(path: &Path, stamp: &str) {
    let status = Command::new("touch")
        .args(["-t", stamp])
        .arg(path)
        .status()
        .expect("run touch");
    assert!(status.success(), "touch -t {stamp} failed");
}

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
}

fn first_ref(output: &[u8]) -> String {
    String::from_utf8_lossy(output)
        .split_whitespace()
        .find_map(|field| field.strip_prefix("ref="))
        .expect("search ref")
        .trim_end_matches(|character: char| !character.is_ascii_hexdigit())
        .to_string()
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

    let search = run_pi(
        config.path(),
        sessions.path(),
        &["agent", "search", "--lexical", "active root question"],
    );
    assert!(
        search.status.success(),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    let search_text = String::from_utf8_lossy(&search.stdout);
    assert!(search_text.contains("uuid=01912345-6789-7abc-8def-0123456789ab"));
    assert!(!search_text.contains("ABANDONED_BRANCH_SENTINEL"));
    let reference = first_ref(&search.stdout);

    let read = run_pi(
        config.path(),
        sessions.path(),
        &["agent", "read", &reference],
    );
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    let read_text = String::from_utf8_lossy(&read.stdout);
    assert!(read_text.contains("active root question"));
    assert!(!read_text.contains("compaction summary searchable"));
    assert!(!read_text.contains("branch summary searchable"));
    assert!(!read_text.contains("ABANDONED_BRANCH_SENTINEL"));

    let outline = run_pi(
        config.path(),
        sessions.path(),
        &["agent", "outline", &reference],
    );
    assert!(
        outline.status.success(),
        "{}",
        String::from_utf8_lossy(&outline.stderr)
    );
    assert!(String::from_utf8_lossy(&outline.stdout).contains("active root question"));
}

#[test]
fn omp_sessions_support_agent_search_and_direct_render() {
    let config = tempfile::tempdir().expect("config");
    let sessions = tempfile::tempdir().expect("sessions");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp/v3.jsonl");
    std::fs::copy(&fixture, sessions.path().join("omp.jsonl")).expect("copy OMP fixture");

    let search = run_pi(
        config.path(),
        sessions.path(),
        &["agent", "search", "--lexical", "OMP active question"],
    );
    assert!(
        search.status.success(),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    let search_text = String::from_utf8_lossy(&search.stdout);
    assert!(search_text.contains("uuid=omp_session_custom_id"));
    assert!(!search_text.contains("OMP_ABANDONED_SENTINEL"));
    let reference = first_ref(&search.stdout);

    let read = run_pi(
        config.path(),
        sessions.path(),
        &["agent", "read", &reference],
    );
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert!(String::from_utf8_lossy(&read.stdout).contains("OMP active answer"));

    let rendered = Command::new(binary())
        .args(["--no-color", "--render"])
        .arg(fixture)
        .output()
        .expect("render OMP fixture");
    assert!(rendered.status.success());
    let rendered = String::from_utf8_lossy(&rendered.stdout);
    assert!(rendered.contains("OMP"));
    assert!(rendered.contains("OMP active question"));
    assert!(!rendered.contains("OMP_ABANDONED_SENTINEL"));
    assert!(!rendered.contains("Mode change"));
}

#[test]
fn codex_sessions_support_agent_search_read_and_direct_render() {
    let config = tempfile::tempdir().expect("config");
    let codex_home = tempfile::tempdir().expect("codex home");
    let day = codex_home.path().join("sessions/2026/08/01");
    std::fs::create_dir_all(&day).expect("create sessions tree");
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex/rollout.jsonl");
    let transcript =
        day.join("rollout-2026-08-01T10-00-00-019f0000-0000-7000-8000-00000000000a.jsonl");
    std::fs::copy(&fixture, &transcript).expect("copy Codex fixture");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex/subagent.jsonl"),
        day.join("rollout-2026-08-02T10-00-00-019f0000-0000-7000-8000-00000000000b.jsonl"),
    )
    .expect("copy Codex sub-agent fixture");

    let search = run_codex(
        config.path(),
        codex_home.path(),
        &["agent", "search", "--lexical", "active codex question"],
    );
    assert!(
        search.status.success(),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    let search_text = String::from_utf8_lossy(&search.stdout);
    assert!(search_text.contains("uuid=019f0000-0000-7000-8000-00000000000a"));
    assert!(!search_text.contains("ENV_CONTEXT_SENTINEL"));
    assert!(
        !search_text.contains("kind=skipped"),
        "a folded sub-agent thread is reachable through its parent, not skipped: {search_text}"
    );
    let reference = first_ref(&search.stdout);

    // The sub-agent thread has no row and no key of its own; its text hits
    // through the session that spawned it, anchored to the spliced entries.
    let folded = run_codex(
        config.path(),
        codex_home.path(),
        &["agent", "search", "--lexical", "child answer searchable"],
    );
    let folded_text = String::from_utf8_lossy(&folded.stdout);
    assert!(
        folded_text.contains("uuid=019f0000-0000-7000-8000-00000000000a"),
        "{folded_text}"
    );
    assert!(!folded_text.contains("uuid=019f0000-0000-7000-8000-00000000000b"));

    let read = run_codex(
        config.path(),
        codex_home.path(),
        &["agent", "read", &reference],
    );
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert!(String::from_utf8_lossy(&read.stdout).contains("codex answer searchable"));

    // Without any cache — the searches above populated it under the config
    // directory — a read must still resolve the ref by parsing, the cost the
    // first load pays anyway.
    std::fs::remove_dir_all(config.path().join("cache")).expect("drop cache");
    let cold_read = run_codex(
        config.path(),
        codex_home.path(),
        &["agent", "read", &reference],
    );
    assert!(
        cold_read.status.success(),
        "{}",
        String::from_utf8_lossy(&cold_read.stderr)
    );
    assert!(String::from_utf8_lossy(&cold_read.stdout).contains("codex answer searchable"));

    let rendered = Command::new(binary())
        .args(["--no-color", "--render"])
        .arg(&transcript)
        .output()
        .expect("render Codex fixture");
    assert!(rendered.status.success());
    let rendered = String::from_utf8_lossy(&rendered.stdout);
    assert!(rendered.contains("active codex question"));
    assert!(rendered.contains("codex answer searchable"));
    assert!(!rendered.contains("ENV_CONTEXT_SENTINEL"));
    assert!(!rendered.contains("DEVELOPER_SENTINEL"));
    assert!(
        !rendered.contains("child answer searchable"),
        "spliced sub-agent turns hide behind the thinking toggle, as for Claude"
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

    let search = run_kimi(
        config.path(),
        kimi_home.path(),
        &["agent", "search", "--lexical", "active kimi question"],
    );
    assert!(
        search.status.success(),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    let search_text = String::from_utf8_lossy(&search.stdout);
    assert!(search_text.contains(&format!("uuid={SESSION}")));
    assert!(!search_text.contains("SYSTEM_REMINDER_SENTINEL"));
    assert!(
        !search_text.contains("kind=skipped"),
        "a folded sub-agent wire is reachable through its session, not skipped: {search_text}"
    );
    let reference = first_ref(&search.stdout);

    // The sub-agent wire has no row and no key of its own; its text hits
    // through the session that spawned it, anchored to the spliced entries.
    let folded = run_kimi(
        config.path(),
        kimi_home.path(),
        &[
            "agent",
            "search",
            "--lexical",
            "kimi child answer searchable",
        ],
    );
    let folded_text = String::from_utf8_lossy(&folded.stdout);
    assert!(
        folded_text.contains(&format!("uuid={SESSION}")),
        "{folded_text}"
    );
    assert!(!folded_text.contains("uuid=session_0f000000-0000-4000-8000-000000000001#agent-0"));

    let read = run_kimi(
        config.path(),
        kimi_home.path(),
        &["agent", "read", &reference],
    );
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert!(String::from_utf8_lossy(&read.stdout).contains("kimi answer searchable"));

    // Without any cache — the searches above populated it under the config
    // directory — a read must still resolve the ref by parsing, the cost the
    // first load pays anyway.
    std::fs::remove_dir_all(config.path().join("cache")).expect("drop cache");
    let cold_read = run_kimi(
        config.path(),
        kimi_home.path(),
        &["agent", "read", &reference],
    );
    assert!(
        cold_read.status.success(),
        "{}",
        String::from_utf8_lossy(&cold_read.stderr)
    );
    assert!(String::from_utf8_lossy(&cold_read.stdout).contains("kimi answer searchable"));

    let rendered = Command::new(binary())
        .args(["--no-color", "--render"])
        .arg(&transcript)
        .output()
        .expect("render Kimi fixture");
    assert!(rendered.status.success());
    let rendered = String::from_utf8_lossy(&rendered.stdout);
    assert!(rendered.contains("active kimi question"));
    assert!(rendered.contains("kimi answer searchable"));
    assert!(!rendered.contains("SYSTEM_REMINDER_SENTINEL"));
    assert!(!rendered.contains("NOTE_SENTINEL"));
    assert!(
        !rendered.contains("kimi child answer searchable"),
        "spliced sub-agent turns hide behind the thinking toggle, as for Claude"
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

    let search = run_opencode(
        config.path(),
        &database,
        &["agent", "search", "--lexical", "active opencode question"],
    );
    assert!(
        search.status.success(),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    let search_text = String::from_utf8_lossy(&search.stdout);
    assert!(search_text.contains("uuid=ses_e2e_parent"), "{search_text}");
    assert!(
        !search_text.contains("kind=skipped"),
        "a folded child session is reachable through its parent, not skipped: {search_text}"
    );
    let synthetic = run_opencode(
        config.path(),
        &database,
        &["agent", "search", "--lexical", "SYNTHETIC_SENTINEL"],
    );
    assert!(
        String::from_utf8_lossy(&synthetic.stdout).contains("uuid=ses_e2e_parent"),
        "an @-file injection indexes as tool output, like a real read; stdout: {} stderr: {}",
        String::from_utf8_lossy(&synthetic.stdout),
        String::from_utf8_lossy(&synthetic.stderr)
    );
    let reference = first_ref(&search.stdout);

    // The child session has no row and no key of its own; its text hits
    // through the session that spawned it.
    let folded = run_opencode(
        config.path(),
        &database,
        &[
            "agent",
            "search",
            "--lexical",
            "opencode child answer searchable",
        ],
    );
    let folded_text = String::from_utf8_lossy(&folded.stdout);
    assert!(folded_text.contains("uuid=ses_e2e_parent"), "{folded_text}");
    assert!(!folded_text.contains("uuid=ses_e2e_child"));

    let read = run_opencode(config.path(), &database, &["agent", "read", &reference]);
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert!(String::from_utf8_lossy(&read.stdout).contains("opencode answer searchable"));

    // Without any cache — the searches above populated it under the config
    // directory — a read must still resolve the ref by parsing.
    std::fs::remove_dir_all(config.path().join("cache")).expect("drop cache");
    let cold_read = run_opencode(config.path(), &database, &["agent", "read", &reference]);
    assert!(
        cold_read.status.success(),
        "{}",
        String::from_utf8_lossy(&cold_read.stderr)
    );
    assert!(String::from_utf8_lossy(&cold_read.stdout).contains("opencode answer searchable"));

    // A locator is renderable like a file: the sniffed path decodes it
    // because its parent is the database file.
    let rendered = Command::new(binary())
        .args(["--no-color", "--render"])
        .arg(database.join("ses_e2e_parent.jsonl"))
        .output()
        .expect("render OpenCode locator");
    assert!(
        rendered.status.success(),
        "{}",
        String::from_utf8_lossy(&rendered.stderr)
    );
    let rendered = String::from_utf8_lossy(&rendered.stdout);
    assert!(rendered.contains("active opencode question"));
    assert!(rendered.contains("opencode answer searchable"));
    assert!(
        !rendered.contains("SYNTHETIC_SENTINEL"),
        "an @-file injection hides with the other tool output by default"
    );

    // With tools shown, the injection renders as the read it narrates.
    let rendered_tools = Command::new(binary())
        .args(["--no-color", "--show-tools", "--render"])
        .arg(database.join("ses_e2e_parent.jsonl"))
        .output()
        .expect("render OpenCode locator with tools");
    let rendered_tools = String::from_utf8_lossy(&rendered_tools.stdout);
    assert!(
        rendered_tools.contains("SYNTHETIC_SENTINEL"),
        "{rendered_tools}"
    );
    assert!(
        !rendered_tools.contains("Called the Read tool"),
        "the narration is the call, not the user's words: {rendered_tools}"
    );
    assert!(!rendered.contains("SNAPSHOT_SENTINEL"));
    assert!(
        !rendered.contains("opencode child answer searchable"),
        "spliced child turns hide behind the thinking toggle, as for Claude"
    );
}

#[test]
fn direct_render_supports_pi_active_branch() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/v3-branched.jsonl");
    let output = Command::new(binary())
        .args(["--no-color", "--render"])
        .arg(path)
        .output()
        .expect("render Pi fixture");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("Pi"));
    assert!(rendered.contains("active root question"));
    for metadata in [
        "Branch summary",
        "Compaction",
        "Thinking level",
        "Model",
        "Label",
    ] {
        assert!(!rendered.contains(metadata));
    }
    assert!(!rendered.contains("ABANDONED_BRANCH_SENTINEL"));
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

    let search = run(
        config.path(),
        &["agent", "search", "--lexical", "phase three needle"],
    );
    assert!(
        search.status.success(),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    let reference = first_ref(&search.stdout);

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
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("protocol agent-search mode=lexical"));
    assert!(stdout.contains("protocol agent-warning kind=malformed-transcript ref=ch_"));
    assert!(stdout.contains("read ref=ch_"));
}

#[test]
fn ref_only_commands_parse_only_the_selected_transcript() {
    let config = tempfile::tempdir().expect("config");
    let project = project(config.path());
    let selected = project.join("12345678-1234-4234-9234-123456789abc.jsonl");
    write_transcript(&selected, "selected transcript needle");

    let search = run(
        config.path(),
        &["agent", "search", "--lexical", "selected transcript needle"],
    );
    assert!(search.status.success());
    let reference = first_ref(&search.stdout);
    std::fs::write(
        project.join("87654321-1234-4234-9234-123456789abc.jsonl"),
        "{malformed\n",
    )
    .expect("write unrelated malformed transcript");

    let outline = run(config.path(), &["agent", "outline", &reference]);

    assert!(
        outline.status.success(),
        "{}",
        String::from_utf8_lossy(&outline.stderr)
    );
    assert!(outline.stderr.is_empty());
    let stdout = String::from_utf8_lossy(&outline.stdout);
    assert!(stdout.contains("m1 role=user"));
    assert!(stdout.contains("m2 role=assistant"));
    assert!(!stdout.contains("malformed-transcript"));
}

#[test]
fn selected_partial_transcript_recovers_records_and_reports_warning() {
    let config = tempfile::tempdir().expect("config");
    let transcript = project(config.path()).join("12345678-1234-4234-9234-123456789abc.jsonl");
    write_transcript(&transcript, "partial transcript needle");
    let search = run(
        config.path(),
        &["agent", "search", "--lexical", "partial transcript needle"],
    );
    assert!(search.status.success());
    let reference = first_ref(&search.stdout);
    let content = std::fs::read_to_string(&transcript).expect("read transcript");
    let (first, second) = content.split_once('\n').expect("two records");
    std::fs::write(&transcript, format!("{first}\n{{malformed\n{second}"))
        .expect("write partial transcript");

    let recovered_search = run(
        config.path(),
        &["agent", "search", "--lexical", "partial transcript needle"],
    );
    assert!(recovered_search.status.success());
    let search_stdout = String::from_utf8_lossy(&recovered_search.stdout);
    assert!(search_stdout.contains("focus=m1..m1"));
    assert!(search_stdout.contains("kind=malformed-transcript"));

    let within = run(
        config.path(),
        &[
            "agent",
            "within",
            &reference,
            "partial transcript needle",
            "--lexical",
        ],
    );
    assert!(within.status.success());
    assert!(String::from_utf8_lossy(&within.stdout).contains("focus=m1..m1"));

    let read = run(
        config.path(),
        &["agent", "read", &format!("{reference}:m1")],
    );
    assert!(read.status.success());
    assert!(String::from_utf8_lossy(&read.stdout).contains("partial transcript needle"));

    let outline = run(config.path(), &["agent", "outline", &reference]);

    assert!(outline.status.success());
    let stdout = String::from_utf8_lossy(&outline.stdout);
    assert!(stdout.contains("warnings=1"));
    assert!(stdout.contains("kind=malformed-transcript"));
    assert!(stdout.contains("m1 role=user"));
    assert!(stdout.contains("m2 role=assistant"));

    let bounded = run(
        config.path(),
        &["agent", "read", &reference, "--budget", "180"],
    );
    assert!(bounded.status.success());
    let bounded_stdout = String::from_utf8_lossy(&bounded.stdout);
    assert!(bounded_stdout.chars().count() <= 180);
    assert!(bounded_stdout.contains("warnings=1"));
    assert!(bounded_stdout.contains("continue read"));
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
    set_modified(&recent, "202607200000");
    set_modified(&old, "202001150000");

    let unfiltered = run(
        config.path(),
        &["agent", "search", "--lexical", "time filter needle"],
    );
    assert!(
        unfiltered.status.success(),
        "{}",
        String::from_utf8_lossy(&unfiltered.stderr)
    );
    let unfiltered_stdout = String::from_utf8_lossy(&unfiltered.stdout);
    assert!(unfiltered_stdout.contains("uuid=11111111"));
    assert!(unfiltered_stdout.contains("uuid=22222222"));

    let filtered = run(
        config.path(),
        &[
            "agent",
            "search",
            "--lexical",
            "time filter needle",
            "--since",
            "2026-01-01",
        ],
    );
    assert!(
        filtered.status.success(),
        "{}",
        String::from_utf8_lossy(&filtered.stderr)
    );
    let filtered_stdout = String::from_utf8_lossy(&filtered.stdout);
    assert!(filtered_stdout.contains("uuid=11111111"));
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
    set_modified(&unparseable, "202607200000");

    let with_malformed = run(
        config.path(),
        &[
            "agent",
            "search",
            "--lexical",
            "time filter needle",
            "--since",
            "2026-01-01",
        ],
    );
    assert!(with_malformed.status.success());
    let with_malformed_stdout = String::from_utf8_lossy(&with_malformed.stdout);
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
