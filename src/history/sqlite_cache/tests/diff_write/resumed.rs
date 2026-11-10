//! A changed Codex session read on from its stored copy writes what a fresh
//! write of its files holds. Each test stores the parse of the session's
//! files as an earlier state left them, changes the files, reads the session
//! on from the stored copy and writes that read, or the parse when the
//! session is read whole.

mod claude;
mod grok;
mod kimi;
mod opencode;
mod pi_log;

use super::*;
use crate::clock::TestClock;
use crate::history::resumed::{ResumeContext, WholeRead, read_resumed};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

const OWN_THREAD: &str = "019f0000-0000-7000-8000-00000000000a";
const SCOUT_THREAD: &str = "019f0000-0000-7000-8000-00000000000c";
const SCOUT: &str = "rollout-2026-08-01T10-05-03-019f0000-0000-7000-8000-00000000000c.jsonl";

/// A Codex session's files under a sessions tree, and the cache holding the
/// last write of the session.
struct CodexSession {
    home: tempfile::TempDir,
    base: tempfile::TempDir,
    cache: OwnedCache,
    own: PathBuf,
    subagents: Vec<PathBuf>,
    /// The stub's modification time, in seconds, moved on with each update.
    updates: u64,
}

/// One update's read.
struct Update {
    outcome: std::result::Result<(), WholeRead>,
    transcripts_read: usize,
    bytes_read: u64,
    /// The entries a resumed read derives from the checkpoint on.
    tail_entries: usize,
}

impl CodexSession {
    /// The session whose own transcript holds `own` and whose sub-agent
    /// transcripts are `subagents`, each a file name and its lines, stored
    /// as a load stores its parse.
    fn stored(own: &[String], subagents: &[(&str, Vec<String>)]) -> Self {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join("sessions/2026/08/01");
        std::fs::create_dir_all(&day).unwrap();
        let own_path = day.join(format!("rollout-2026-08-01T10-00-00-{OWN_THREAD}.jsonl"));
        write_lines(&own_path, own);
        let subagents = subagents
            .iter()
            .map(|(name, lines)| {
                let path = day.join(name);
                write_lines(&path, lines);
                path
            })
            .collect();
        let base = tempfile::tempdir().unwrap();
        let cache = admitted_cache(base.path());
        cache.admit_storage(storage_of(Source::Codex)).unwrap();
        let mut session = Self {
            home,
            base,
            cache,
            own: own_path,
            subagents,
            updates: 1,
        };
        let fixture = session.fixture();
        let record = parsed_record(Source::Codex, &fixture.root, &fixture.stub);
        session.cache.write(&record).unwrap();
        session
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
            source: Source::Codex,
            root: SessionRoot::new(self.own.parent().unwrap()),
            stub,
            _session: None,
        }
    }

    /// Adds a sub-agent transcript named `name` holding `lines`, after the
    /// others.
    fn add_subagent(&mut self, name: &str, lines: &[String]) {
        let path = self.own.with_file_name(name);
        write_lines(&path, lines);
        self.subagents.push(path);
    }

    fn subagent(&self, name: &str) -> PathBuf {
        self.own.with_file_name(name)
    }

    /// Writes the title `name` for the session to the session index.
    fn name_in_index(&self, name: &str) {
        let record =
            json!({"id": OWN_THREAD, "thread_name": name, "updated_at": "2026-08-01T10:09:00Z"});
        write_lines(
            &self.home.path().join("session_index.jsonl"),
            &[record.to_string()],
        );
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

/// Reads `fixture`'s session on from the copy `cache`, in `base`, stores and
/// writes the read, or writes its parse when it is read whole, as a load
/// does; asserts the listed row and the stored session match a fresh parse
/// and write.
fn update_stored(cache: &mut OwnedCache, base: &Path, fixture: &Fixture) -> Update {
    let source = fixture.source();
    let stored = fixture.listed(cache);
    let reader = SqliteCache::open_waiting(&rearview_at(base))
        .unwrap()
        .unwrap();
    let reads = AtomicUsize::new(0);
    let on_transcript_read = || {
        reads.fetch_add(1, Ordering::Relaxed);
    };
    let clock = TestClock::default();
    let context = ResumeContext {
        storage: source.provider().storage(),
        cache: &reader,
        root: &fixture.root,
        stub: &fixture.stub,
        stored: &stored,
        on_transcript_read: &on_transcript_read,
        clock: &clock,
        debug_level: None,
    };
    let resumed = read_resumed(&context, crate::history::FOR_FIXTURES).unwrap();
    let (record, row) = parsed_record_and_row(source, &fixture.root, &fixture.stub);
    let mut bytes_read = 0;
    let mut tail_entries = 0;
    let outcome = match resumed {
        Ok(resumed) => {
            assert_eq!(
                listed_facts(&resumed.row),
                listed_facts(&row),
                "the resumed read lists the row a parse lists"
            );
            bytes_read = resumed.bytes_read;
            if let Some(Ok(tail)) = &resumed.record.entries {
                tail_entries = tail.entries.len();
            }
            cache.write(&resumed.record).unwrap();
            Ok(())
        }
        Err(whole) => {
            cache.write(&record).unwrap();
            Err(whole)
        }
    };
    assert_matches_fresh_write(cache, fixture, &record);
    Update {
        outcome,
        transcripts_read: reads.into_inner(),
        bytes_read,
        tail_entries,
    }
}

/// The bytes of the transcript at `path`, one of `fixture`'s session's, from
/// where the next read of the copy `cache` stores starts.
fn bytes_after_stable_cursor(cache: &OwnedCache, fixture: &Fixture, path: &Path) -> u64 {
    let session = fixture.session_id(cache);
    let (_, read) = stored_reads(cache, session)
        .into_iter()
        .find(|(stored, _)| stored == path)
        .unwrap();
    // Each line-delimited format's reader stores its cursor as the JSON of a
    // line cursor.
    let stable: Value = serde_json::from_slice(&read.stable_cursor).unwrap();
    std::fs::metadata(path).unwrap().len() - stable["offset"].as_u64().unwrap()
}

fn write_lines(path: &Path, lines: &[String]) {
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(path, text).unwrap();
}

/// The listed row's facts a fold derives.
fn listed_facts(row: &Conversation) -> String {
    format!(
        "{:?} {:?}",
        (
            &row.preview_first,
            &row.preview_last,
            &row.summary,
            &row.custom_title,
            &row.model,
            row.total_tokens,
            row.duration_minutes,
        ),
        (
            row.message_count,
            row.assistant_messages,
            row.timestamp,
            &row.parse_errors,
            row.is_missing_earlier_turns,
            &row.message_times,
            &row.subagents,
        )
    )
}

fn record(time: &str, kind: &str, payload: Value) -> String {
    json!({"timestamp": format!("2026-08-01T{time}Z"), "type": kind, "payload": payload})
        .to_string()
}

fn user(time: &str, id: &str, text: &str) -> String {
    record(
        time,
        "response_item",
        json!({"type": "message", "id": id, "role": "user", "content": [{"type": "input_text", "text": text}]}),
    )
}

fn reply(time: &str, id: &str, text: &str) -> String {
    record(
        time,
        "response_item",
        json!({"type": "message", "id": id, "role": "assistant", "content": [{"type": "output_text", "text": text}], "phase": "final_answer"}),
    )
}

fn tokens(time: &str, input: u64, output: u64) -> String {
    let usage = json!({"input_tokens": input, "cached_input_tokens": 0, "output_tokens": output, "total_tokens": input + output});
    record(
        time,
        "event_msg",
        json!({"type": "token_count", "info": {"total_token_usage": usage, "last_token_usage": usage}}),
    )
}

fn fixture_lines(name: &str) -> Vec<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/codex")
        .join(name);
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The session's own transcript as the fixture holds it, then `more`.
fn own_lines(more: &[String]) -> Vec<String> {
    let mut lines = fixture_lines("rollout.jsonl");
    lines.extend_from_slice(more);
    lines
}

/// The fixture's two sub-agent transcripts, each spliced mid-session.
fn fixture_subagents() -> Vec<(&'static str, Vec<String>)> {
    vec![
        (
            "rollout-2026-08-01T10-00-03-019f0000-0000-7000-8000-00000000000b.jsonl",
            fixture_lines("subagent.jsonl"),
        ),
        (
            "rollout-2026-08-01T10-00-11-019f0000-0000-7000-8000-000000000010.jsonl",
            fixture_lines("subagent-without-user-text.jsonl"),
        ),
    ]
}

/// The `spawn_agent` call that starts `/root/scout`, and its result.
fn scout_spawn() -> Vec<String> {
    vec![
        user("10:05:00.000", "msg_u_scout", "Scout the notes"),
        record(
            "10:05:01.000",
            "response_item",
            json!({"type": "function_call", "id": "fc_scout", "name": "spawn_agent", "arguments": "{\"task_name\":\"scout\",\"agent_type\":\"explorer\",\"message\":\"Read the notes.\"}", "call_id": "call_scout"}),
        ),
        record(
            "10:05:02.000",
            "response_item",
            json!({"type": "function_call_output", "id": "fco_scout", "call_id": "call_scout", "output": "{\"task_name\":\"/root/scout\"}"}),
        ),
    ]
}

/// `/root/scout`'s transcript: its header, then `turns`.
fn scout_lines(turns: &[String]) -> Vec<String> {
    let spawn = json!({"parent_thread_id": OWN_THREAD, "depth": 1, "agent_path": "/root/scout", "agent_nickname": "Scout", "agent_role": "explorer"});
    let mut lines = vec![record(
        "10:05:03.000",
        "session_meta",
        json!({
            "session_id": OWN_THREAD,
            "id": SCOUT_THREAD,
            "parent_thread_id": OWN_THREAD,
            "timestamp": "2026-08-01T10:05:03.000Z",
            "cwd": "/tmp/project",
            "originator": "codex-tui",
            "cli_version": "0.162.1",
            "source": {"subagent": {"thread_spawn": spawn}},
            "thread_source": "subagent",
            "agent_nickname": "Scout",
            "agent_role": "explorer",
            "agent_path": "/root/scout",
        }),
    )];
    lines.extend_from_slice(turns);
    lines
}

/// The report `/root/scout` hands back to the session.
fn scout_report(time: &str, text: &str) -> String {
    record(
        time,
        "response_item",
        json!({"type": "agent_message", "id": "amsg_scout", "author": "/root/scout", "recipient": "/root", "content": [{"type": "input_text", "text": format!("Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/scout\nPayload:\n{text}")}]}),
    )
}

#[test]
fn entries_appended_to_own_transcript_are_read_from_its_stable_cursor() {
    let asked = [user("10:05:00.000", "msg_u_more", "What changed?")];
    let mut session = CodexSession::stored(&own_lines(&asked), &fixture_subagents());
    let answered = own_lines(&[
        asked[0].clone(),
        reply("10:05:04.000", "msg_a_more", "The notes changed."),
        tokens("10:05:05.000", 2_000, 90),
    ]);
    let after_cursor_before = session.bytes_after_stable_cursor(&session.own.clone());
    let appended = answered
        .iter()
        .map(|line| line.len() as u64 + 1)
        .sum::<u64>()
        - own_lines(&asked)
            .iter()
            .map(|line| line.len() as u64 + 1)
            .sum::<u64>();
    write_lines(&session.own, &answered);

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 1,
        "the read opens the changed transcript alone"
    );
    assert_eq!(
        update.bytes_read,
        after_cursor_before + appended,
        "the read reads from the stable cursor on"
    );
}

#[test]
fn reply_continued_in_later_update_resumes_as_one_message() {
    let started = [
        user("10:05:00.000", "msg_u_more", "What changed?"),
        reply("10:05:04.000", "msg_a_more", "The notes changed."),
    ];
    let mut session = CodexSession::stored(&own_lines(&started), &fixture_subagents());
    let mut continued = started.to_vec();
    continued.push(reply(
        "10:05:05.000",
        "msg_a_more",
        "So did the release steps.",
    ));
    write_lines(&session.own, &own_lines(&continued));

    let update = session.update();

    assert_eq!(
        update.outcome,
        Ok(()),
        "the checkpoint stands before the open reply, so its continuation resumes"
    );
}

#[test]
fn turn_appended_to_subagent_is_read_as_its_window() {
    let mut subagents = fixture_subagents();
    let first = reply("10:05:04.000", "msg_s1", "Reading the notes.");
    subagents.push((SCOUT, scout_lines(std::slice::from_ref(&first))));
    let mut session = CodexSession::stored(&own_lines(&scout_spawn()), &subagents);
    write_lines(
        &session.subagent(SCOUT),
        &scout_lines(&[first, reply("10:05:06.000", "msg_s2", "Still reading.")]),
    );

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 1,
        "the read opens the changed sub-agent transcript alone"
    );
}

#[test]
fn subagent_spawned_and_reporting_over_later_updates_resumes_each_time() {
    let mut session = CodexSession::stored(&own_lines(&scout_spawn()), &fixture_subagents());
    let first = reply("10:05:04.000", "msg_s1", "Reading the notes.");
    session.add_subagent(SCOUT, &scout_lines(std::slice::from_ref(&first)));

    let spawned = session.update();
    assert_eq!(spawned.outcome, Ok(()), "a new sub-agent resumes");
    assert_eq!(
        spawned.transcripts_read, 1,
        "the read opens the new sub-agent transcript alone"
    );

    // The session moves on past the sub-agent's answer, which then stands
    // before the checkpoint when the report folds it.
    let answer = reply("10:05:10.000", "msg_s2", "The notes list the steps.");
    write_lines(&session.subagent(SCOUT), &scout_lines(&[first, answer]));
    let mut more = scout_spawn();
    more.push(user("10:05:20.000", "msg_u_wait", "Any news?"));
    more.push(reply("10:05:21.000", "msg_a_wait", "Still waiting."));
    write_lines(&session.own, &own_lines(&more));
    let answered = session.update();
    assert_eq!(answered.outcome, Ok(()), "the sub-agent's answer resumes");

    more.push(scout_report("10:05:30.000", "The notes list the steps."));
    more.push(reply("10:05:31.000", "msg_a_scout", "The scout is done."));
    write_lines(&session.own, &own_lines(&more));
    let reported = session.update();
    assert_eq!(
        reported.outcome,
        Ok(()),
        "a report that folds the sub-agent's earlier answer resumes"
    );
    assert_eq!(
        reported.transcripts_read, 2,
        "the read opens the session's transcript and the sub-agent the report names"
    );
}

#[test]
fn title_added_to_session_index_replaces_leading_entries() {
    let asked = [user("10:05:00.000", "msg_u_more", "What changed?")];
    let mut session = CodexSession::stored(&own_lines(&asked), &fixture_subagents());
    session.name_in_index("Release notes");
    write_lines(
        &session.own,
        &own_lines(&[
            asked[0].clone(),
            reply("10:05:04.000", "msg_a_more", "The notes changed."),
        ]),
    );

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
}

#[test]
fn rewritten_or_shrunk_own_transcript_is_read_whole() {
    let asked = [
        user("10:05:00.000", "msg_u_more", "What changed?"),
        reply("10:05:04.000", "msg_a_more", "The notes changed."),
    ];
    let mut session = CodexSession::stored(&own_lines(&asked), &fixture_subagents());
    let mut rewritten = own_lines(&asked);
    rewritten[18] = user("10:02:00.000", "msg_u2", "a longer follow-up question");
    write_lines(&session.own, &rewritten);
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnRewritten),
        "a rewritten transcript is read whole"
    );

    write_lines(&session.own, &own_lines(&asked[..1]));
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnRewritten),
        "a shrunk transcript is read whole"
    );
}

#[test]
fn session_moved_to_another_rollout_is_read_whole() {
    let mut session = CodexSession::stored(&own_lines(&[]), &fixture_subagents());
    let edited = session
        .own
        .with_file_name(format!("rollout-2026-08-01T10-06-00-{OWN_THREAD}.jsonl"));
    write_lines(
        &edited,
        &own_lines(&[user("10:06:00.000", "msg_u_edit", "An edited question")]),
    );
    session.own = edited;

    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnDoesNotResume),
        "a session whose own transcript moved is read whole"
    );
}
