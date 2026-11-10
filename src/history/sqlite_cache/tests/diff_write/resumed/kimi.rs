//! A changed Kimi session read on from its stored copy writes what a fresh
//! write of its files holds, as the Codex tests beside it check: each update
//! asserts the listed row and the stored session against a fresh parse and
//! write.

use super::*;

const SESSION: &str = "session_0f000000-0000-4000-8000-0000000000a8";
const WORKSPACE: &str = "wd_kimi-project_000000000000";

/// A Kimi session directory under a sessions root, and the cache holding the
/// last write of the session.
struct KimiSession {
    home: tempfile::TempDir,
    base: tempfile::TempDir,
    cache: OwnedCache,
    own: PathBuf,
    subagents: Vec<PathBuf>,
    /// The stub's modification time, in seconds, moved on with each update.
    updates: u64,
}

impl KimiSession {
    /// The session whose main wire holds `own` and whose sub-agent wires are
    /// `subagents`, under a title Kimi generated; stored as a load stores
    /// its parse.
    fn stored(own: &[String], subagents: &[Vec<String>]) -> Self {
        let home = tempfile::tempdir().unwrap();
        let session_dir = home.path().join("sessions").join(WORKSPACE).join(SESSION);
        let own_path = session_dir.join("agents/main/wire.jsonl");
        std::fs::create_dir_all(own_path.parent().unwrap()).unwrap();
        write_lines(&own_path, own);
        let subagents = subagents
            .iter()
            .enumerate()
            .map(|(index, lines)| {
                let path = session_dir.join(format!("agents/agent-{index}/wire.jsonl"));
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                write_lines(&path, lines);
                path
            })
            .collect();
        let base = tempfile::tempdir().unwrap();
        let cache = admitted_cache(base.path());
        cache.admit_storage(storage_of(Source::Kimi)).unwrap();
        let mut session = Self {
            home,
            base,
            cache,
            own: own_path,
            subagents,
            updates: 1,
        };
        session.write_state(
            "/tmp/kimi-project",
            "kimi generated title",
            "replaceable",
            1,
        );
        let fixture = session.fixture();
        let record = parsed_record(Source::Kimi, &fixture.root, &fixture.stub);
        session.cache.write(&record).unwrap();
        session
    }

    fn session_dir(&self) -> PathBuf {
        self.home
            .path()
            .join("sessions")
            .join(WORKSPACE)
            .join(SESSION)
    }

    /// Writes the session's `state.json`: recorded in `cwd`, titled `title`
    /// of `kind`, last updated at `updated`.
    fn write_state(&self, cwd: &str, title: &str, kind: &str, updated: u64) {
        let mut agents = json!({"main": {"type": "main"}});
        for index in 0..self.subagents.len() {
            agents[format!("agent-{index}")] = json!({"type": "sub", "parentAgentId": "main"});
        }
        let state = json!({
            "id": SESSION, "version": 2, "cwd": cwd,
            "createdAt": 1786010400000_i64, "updatedAt": 1786010400000_i64 + updated as i64,
            "agents": agents, "title": title, "titleKind": kind,
        });
        std::fs::write(self.session_dir().join("state.json"), state.to_string()).unwrap();
    }

    fn fixture(&self) -> Fixture {
        let stub = SessionStub {
            locator: self.own.clone(),
            subagents: self.subagents.clone(),
            cache_key: CACHE_KEY.to_owned(),
            fingerprint: Fingerprint {
                size: 1_000,
                modified: Some(UNIX_EPOCH + Duration::from_secs(self.updates)),
            },
        };
        Fixture {
            source: Source::Kimi,
            root: SessionRoot::new(self.home.path().join("sessions")),
            stub,
            _session: None,
        }
    }

    fn update(&mut self) -> Update {
        self.updates += 1;
        let fixture = self.fixture();
        update_stored(&mut self.cache, self.base.path(), &fixture)
    }

    fn bytes_after_stable_cursor(&self, path: &Path) -> u64 {
        bytes_after_stable_cursor(&self.cache, &self.fixture(), path)
    }
}

fn metadata() -> String {
    json!({"type": "metadata", "protocol_version": "1.5", "created_at": 1786010400000_i64})
        .to_string()
}

fn message(seconds: i64, text: &str, kind: &str, extra: Value) -> String {
    let mut origin = json!({"kind": kind});
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        origin[key] = value;
    }
    json!({
        "type": "context.append_message",
        "message": {"role": "user", "content": [{"type": "text", "text": text}], "origin": origin},
        "time": 1786010400000_i64 + seconds * 1000,
    })
    .to_string()
}

fn prompt(seconds: i64, text: &str) -> String {
    message(seconds, text, "user", json!({}))
}

fn reply(seconds: i64, text: &str) -> String {
    json!({
        "type": "context.append_loop_event",
        "event": {"type": "content.part", "part": {"type": "text", "text": text}},
        "time": 1786010400000_i64 + seconds * 1000,
    })
    .to_string()
}

fn usage(seconds: i64, model: &str) -> String {
    json!({
        "type": "usage.record", "usageScope": "turn", "model": model,
        "usage": {"inputOther": 100, "output": 20}, "time": 1786010400000_i64 + seconds * 1000,
    })
    .to_string()
}

/// The main wire's opening turns: two prompts, each answered.
fn opening() -> Vec<String> {
    vec![
        metadata(),
        prompt(1, "Start the work"),
        reply(2, "Starting."),
        usage(3, "kimi-k3"),
        prompt(10, "What next?"),
        reply(11, "The release."),
        usage(12, "kimi-k3"),
    ]
}

fn with(lines: &[String], more: &[String]) -> Vec<String> {
    lines.iter().chain(more).cloned().collect()
}

#[test]
fn records_appended_to_main_wire_are_read_from_its_stable_cursor() {
    let mut session = KimiSession::stored(&opening(), &[]);
    let after_cursor_before = session.bytes_after_stable_cursor(&session.own.clone());
    let more = [
        prompt(20, "Go on"),
        reply(21, "Going on."),
        usage(22, "kimi-k3-next"),
    ];
    write_lines(&session.own, &with(&opening(), &more));

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 1,
        "the read opens the changed wire alone"
    );
    let appended = more.iter().map(|line| line.len() as u64 + 1).sum::<u64>();
    assert_eq!(
        update.bytes_read,
        after_cursor_before + appended,
        "the read reads from the stable cursor on"
    );
}

#[test]
fn turn_appended_to_subagent_wire_is_read_as_its_window() {
    let subagent = vec![
        metadata(),
        message(
            13,
            "Scout the notes",
            "system_trigger",
            json!({"name": "subagent"}),
        ),
        reply(14, "Reading the notes."),
    ];
    let mut session = KimiSession::stored(&opening(), std::slice::from_ref(&subagent));
    write_lines(
        &session.subagents[0].clone(),
        &with(&subagent, &[reply(15, "The notes list the steps.")]),
    );

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 1,
        "the read opens the changed sub-agent wire alone"
    );
}

#[test]
fn shell_command_answered_in_later_update_resumes() {
    let command = message(
        20,
        "<bash-input>\ncargo test\n</bash-input>",
        "shell_command",
        json!({"phase": "input"}),
    );
    let mut session = KimiSession::stored(&with(&opening(), std::slice::from_ref(&command)), &[]);
    let output = message(
        30,
        "<bash-stdout>ok\n</bash-stdout>",
        "shell_command",
        json!({"phase": "output"}),
    );
    write_lines(&session.own, &with(&opening(), &[command, output]));

    assert_eq!(
        session.update().outcome,
        Ok(()),
        "the stable cursor stands before the command awaiting its output"
    );
}

#[test]
fn rewritten_state_changes_row_as_full_read_does() {
    let mut session = KimiSession::stored(&opening(), &[]);
    let mut lines = opening();

    session.write_state(
        "/tmp/kimi-project",
        "kimi generated title",
        "replaceable",
        2,
    );
    lines.push(prompt(20, "Go on"));
    write_lines(&session.own, &lines);
    assert_eq!(
        session.update().outcome,
        Ok(()),
        "a state rewritten with the same title resumes"
    );

    session.write_state("/tmp/kimi-project", "Chosen name", "custom", 3);
    lines.push(reply(21, "Going on."));
    write_lines(&session.own, &lines);
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::LeadingChanged),
        "a generated title replaced by a chosen one reads the session whole"
    );

    session.write_state("/tmp/kimi-project", "Another name", "custom", 4);
    lines.push(prompt(30, "And then?"));
    write_lines(&session.own, &lines);
    assert_eq!(
        session.update().outcome,
        Ok(()),
        "a chosen title renamed resumes with its new name"
    );

    session.write_state("/tmp/elsewhere", "Another name", "custom", 5);
    lines.push(reply(31, "Then the release."));
    write_lines(&session.own, &lines);
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnRewritten),
        "a working directory state.json no longer names is another identity"
    );
}

#[test]
fn rewritten_or_shrunk_main_wire_is_read_whole() {
    let mut session = KimiSession::stored(&opening(), &[]);
    let mut rewritten = opening();
    rewritten[2] = reply(2, "Starting at once.");
    write_lines(&session.own, &rewritten);
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnRewritten),
        "a rewritten wire is read whole"
    );

    write_lines(&session.own, &opening()[..4]);
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnRewritten),
        "a shrunk wire is read whole"
    );
}
