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
use crate::history::provider::walk::test_support::link_directory;
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
    /// Another spelling of `home` for later reads to name the session's
    /// files through; `None` for `home` as it is.
    home_spelling: Option<PathBuf>,
}

/// One update's read.
pub(in crate::history::sqlite_cache::tests) struct Update {
    pub(in crate::history::sqlite_cache::tests) outcome: std::result::Result<(), WholeRead>,
    transcripts_read: usize,
    bytes_read: u64,
    /// The entries a resumed read derives from the checkpoint on.
    tail_entries: usize,
    /// The runs of the lookup of a sub-agent reply's stored turns
    /// ([`window::SUBAGENT_REPLY_STORED`]).
    reply_lookups: i32,
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
            home_spelling: None,
        };
        let fixture = session.fixture();
        let record = parsed_record(Source::Codex, &fixture.root, &fixture.stub);
        session.cache.write(&record).unwrap();
        session
    }

    fn fixture(&self) -> Fixture {
        let spelled = |path: &Path| match &self.home_spelling {
            Some(home) => home.join(path.strip_prefix(self.home.path()).unwrap()),
            None => path.to_path_buf(),
        };
        let stub = SessionStub {
            locator: spelled(&self.own),
            subagents: self.subagents.iter().map(|path| spelled(path)).collect(),
            cache_key: CACHE_KEY.to_owned(),
            fingerprint: Fingerprint {
                size: 1_000,
                modified: Some(UNIX_EPOCH + Duration::from_secs(self.updates)),
            },
        };
        let home = spelled(self.home.path());
        Fixture {
            source: Source::Codex,
            root: SessionRoot::new(home.join("sessions"))
                .with_directory_beside(home.join("archived_sessions")),
            stub,
            _session: None,
        }
    }

    /// Name the session's files from now on through `home` spelled through
    /// a directory inside it and `..`. The file system canonicalises that
    /// spelling to `home`.
    fn spell_home_through_parent(&mut self) {
        let elsewhere = self.home.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        self.home_spelling = Some(elsewhere.join(".."));
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
pub(in crate::history::sqlite_cache::tests) fn update_stored(
    cache: &mut OwnedCache,
    base: &Path,
    fixture: &Fixture,
) -> Update {
    let (update, record) = update_stored_changing_files(cache, base, fixture, &|| {});
    assert_matches_fresh_write(cache, fixture, &record);
    update
}

/// [`update_stored`], with `change` run as the read opens each transcript,
/// after it stamped the session's files, and without comparing the stored
/// session with a fresh write; also returns the record of a fresh parse.
fn update_stored_changing_files(
    cache: &mut OwnedCache,
    base: &Path,
    fixture: &Fixture,
    change: &(dyn Fn() + Sync),
) -> (Update, SessionRecord) {
    let source = fixture.source();
    let stored = fixture.listed(cache);
    let reader = SqliteCache::open_waiting(&rearview_at(base))
        .unwrap()
        .unwrap();
    let reads = AtomicUsize::new(0);
    let on_transcript_read = || {
        change();
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
        project_directories: &crate::history::path::CanonicalDirectories::of_system(),
    };
    let resumed = read_resumed(&context, crate::history::FOR_FIXTURES).unwrap();
    let reply_lookups = reader
        .connection
        .prepare_cached(window::SUBAGENT_REPLY_STORED)
        .unwrap()
        .get_status(rusqlite::StatementStatus::Run);
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
            if let Some(RecordEntries::Written(tail)) = &resumed.record.entries {
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
    let update = Update {
        outcome,
        transcripts_read: reads.into_inner(),
        bytes_read,
        tail_entries,
        reply_lookups,
    };
    (update, record)
}

/// The bytes of the transcript at `path`, one of `fixture`'s session's, from
/// where the next read of the copy `cache` stores starts.
fn bytes_after_stable_cursor(cache: &OwnedCache, fixture: &Fixture, path: &Path) -> u64 {
    let session = fixture.session_id(cache);
    let (_, read) = stored_reads(cache, session, &fixture.root.path)
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

/// The count of stored turns of `/root/scout`, after `reply_count` replies,
/// read from its last line, and the SQLite program steps the read took.
fn stored_turn_count_and_steps_from_last_line(reply_count: usize) -> (usize, u64) {
    let replies = (0..reply_count)
        .map(|index| {
            reply(
                &format!("10:{:02}:{:02}.000", 6 + index / 60, index % 60),
                &format!("msg_s{index}"),
                "Reading the notes.",
            )
        })
        .collect::<Vec<_>>();
    let mut subagents = fixture_subagents();
    subagents.push((SCOUT, scout_lines(&replies)));
    let mut session = CodexSession::stored(&own_lines(&scout_spawn()), &subagents);
    let fixture = session.fixture();
    let generation = fixture
        .listed(&session.cache)
        .cached_session
        .expect("the listed row holds its copy")
        .generation();
    let resume = session
        .cache
        .stored_resume(
            Source::Codex.provider().storage(),
            &fixture.root,
            &fixture.stub.cache_key,
            generation,
        )
        .unwrap()
        .unwrap();
    let steps = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counted = std::sync::Arc::clone(&steps);
    session
        .cache
        .connection
        .progress_handler(
            1,
            Some(move || {
                counted.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .unwrap();

    let turns = session
        .cache
        .stored_turns_from(&resume, &session.subagent(SCOUT), reply_count + 1)
        .unwrap();

    session
        .cache
        .connection
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    (turns.len(), steps.load(Ordering::Relaxed))
}

#[test]
fn subagents_stored_turns_read_from_line_cost_same_however_long_its_history() {
    let (short_turns, short_steps) = stored_turn_count_and_steps_from_last_line(10);
    let (long_turns, long_steps) = stored_turn_count_and_steps_from_last_line(400);

    assert_eq!(
        (short_turns, long_turns),
        (1, 1),
        "the read returns the turn at the line alone"
    );
    assert!(
        long_steps < short_steps * 2,
        "the read's steps stay flat as the history grows: {short_steps} after 10 replies, \
         {long_steps} after 400"
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

    // The session moves on past the sub-agent's answer. The answer then
    // stands before the checkpoint when the report folds it.
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

/// A parse reads the session's transcript and its sub-agents' at different
/// moments, so its copy can hold the sub-agent's report while the
/// sub-agent's transcript lacks the answer the report repeats.
#[test]
fn report_read_before_subagents_final_answer_resumes_as_fresh_parse() {
    let first = reply("10:05:04.000", "msg_s1", "Reading the notes.");
    let mut subagents = fixture_subagents();
    subagents.push((SCOUT, scout_lines(std::slice::from_ref(&first))));
    let mut own = scout_spawn();
    own.push(scout_report("10:05:30.000", "The notes list the steps."));
    own.push(reply("10:05:31.000", "msg_a_scout", "The scout is done."));
    let mut session = CodexSession::stored(&own_lines(&own), &subagents);
    let answer = reply("10:05:10.000", "msg_s2", "The notes list the steps.");
    write_lines(&session.subagent(SCOUT), &scout_lines(&[first, answer]));

    // The update asserts that the copy holds what a fresh write holds.
    session.update();
}

/// Appends `line` to the file at `path`.
fn append_line(path: &Path, line: &str) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes()).unwrap();
}

#[test]
fn transcript_growing_during_a_resumed_read_is_stored_and_read_on_next() {
    let mut session = CodexSession::stored(&own_lines(&[]), &fixture_subagents());
    write_lines(
        &session.own,
        &own_lines(&[user("10:05:00.000", "msg_u_more", "What changed?")]),
    );
    let own = session.own.clone();
    let answer = reply("10:05:04.000", "msg_a_more", "The notes changed.");
    let is_appended = std::sync::atomic::AtomicBool::new(false);
    session.updates += 1;
    let fixture = session.fixture();

    let (grown, _) =
        update_stored_changing_files(&mut session.cache, session.base.path(), &fixture, &|| {
            if !is_appended.swap(true, Ordering::SeqCst) {
                append_line(&own, &answer);
            }
        });

    assert_eq!(
        grown.outcome,
        Ok(()),
        "the read on from the copy is stored, not read whole"
    );
    // The update asserts that the copy holds what a fresh write holds.
    assert_eq!(
        session.update().outcome,
        Ok(()),
        "the next read reads on from the stored cursor"
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

/// Forty questions and answers, each about `topic`, from 10:03 on: enough
/// to put the first more than the rewrite check's 4 KiB before the end.
fn exchanges_about(topic: &str) -> Vec<String> {
    (0..40)
        .flat_map(|index| {
            [
                user(
                    &format!("10:03:{index:02}.000"),
                    &format!("msg_u_{topic}_{index}"),
                    &format!("Question {index} about the {topic}"),
                ),
                reply(
                    &format!("10:03:{index:02}.500"),
                    &format!("msg_a_{topic}_{index}"),
                    &format!("Answer {index} about the {topic}"),
                ),
            ]
        })
        .collect()
}

/// Rewrites the first `from` in the file at `path` as `to`, of the same
/// length, more than 4 KiB before the file's end, and moves the file's
/// modification time on by a second.
fn edit_early_in_place(path: &Path, from: &str, to: &str) {
    assert_eq!(from.len(), to.len(), "the edit keeps the file's size");
    let text = std::fs::read_to_string(path).unwrap();
    let at = text.find(from).unwrap();
    assert!(
        at + (crate::history::format::REWRITE_CHECK_BYTES as usize) < text.len(),
        "the edit stands before the bytes the rewrite check hashes: {at} of {}",
        text.len()
    );
    let modified = std::fs::metadata(path).unwrap().modified().unwrap();
    std::fs::write(path, text.replacen(from, to, 1)).unwrap();
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(modified + Duration::from_secs(1))
        .unwrap();
}

#[test]
fn own_transcript_edited_in_place_before_checked_tail_is_read_whole() {
    let mut session =
        CodexSession::stored(&own_lines(&exchanges_about("notes")), &fixture_subagents());

    edit_early_in_place(
        &session.own,
        "Question 0 about the notes",
        "Question 0 about the NOTES",
    );

    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnRewritten),
        "a transcript changed without growing is read whole"
    );
}

#[test]
fn subagent_transcript_edited_in_place_before_checked_tail_is_read_whole() {
    let mut subagents = fixture_subagents();
    subagents.push((SCOUT, scout_lines(&exchanges_about("steps"))));
    let mut session = CodexSession::stored(&own_lines(&scout_spawn()), &subagents);

    edit_early_in_place(
        &session.subagent(SCOUT),
        "Answer 0 about the steps",
        "Answer 0 about the STEPS",
    );
    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.bytes_read,
        std::fs::metadata(session.subagent(SCOUT)).unwrap().len(),
        "the sub-agent transcript changed without growing is read whole"
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

/// A session cached through one spelling of the agent's directory, read on
/// through another after its sub-agent and its own transcript grew: the
/// read resumes, opens the two changed transcripts alone, and writes what a
/// fresh parse holds, its numbering, splices and links included.
#[test]
fn append_after_root_spelling_changes_resumes_as_fresh_parse() {
    // The scout's first turn stands before the checkpoint, so the stored
    // checkpoint numbers its thread.
    let mut subagents = fixture_subagents();
    let first = reply("10:05:04.000", "msg_s1", "Reading the notes.");
    subagents.push((SCOUT, scout_lines(std::slice::from_ref(&first))));
    let mut own = scout_spawn();
    own.push(user("10:05:20.000", "msg_u_wait", "Any news?"));
    own.push(reply("10:05:21.000", "msg_a_wait", "Still waiting."));
    let mut session = CodexSession::stored(&own_lines(&own), &subagents);
    write_lines(
        &session.subagent(SCOUT),
        &scout_lines(&[
            first,
            reply("10:05:40.000", "msg_s2", "The notes list the steps."),
        ]),
    );
    own.push(scout_report("10:05:50.000", "The notes list the steps."));
    own.push(reply("10:05:51.000", "msg_a_scout", "The scout is done."));
    write_lines(&session.own, &own_lines(&own));
    session.spell_home_through_parent();

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 2,
        "the read opens the session's transcript and the changed sub-agent's"
    );
}

/// A Codex thread edited once, its rollouts before the edit archived, with
/// the archived tree linked into a store and cached by a load. The link
/// moves to a copy of its files before the next load. That load reads the
/// session whole, each rollout once, and its copy holds the archived turns.
#[test]
fn load_after_archived_tree_is_retargeted_reads_the_session_whole_once() {
    use crate::history::sqlite_cache::admitted_reads;
    let home = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let archive = |name: &str| store.path().join(name);
    std::fs::create_dir_all(archive("archive-one")).unwrap();
    let link = home.path().join("archived_sessions");
    link_directory(&archive("archive-one"), &link);
    let bases = crate::history::format::codex::test_support::write_session_with_archived_bases(
        home.path(),
        OWN_THREAD,
        SCOUT_THREAD,
    );
    let codex = crate::history::provider::contract_tests::registered_fixtures()
        .into_iter()
        .find(|fixture| fixture.provider().source() == Source::Codex)
        .unwrap();
    let storage = crate::history::provider::contract_tests::kept_storage_under(codex, home.path());
    let base = crate::tui::claude_home::admitted_cache();
    let environment = crate::history::environment::test_support::test_environment(
        vec![storage],
        Some(base.path()),
    );
    let load = || {
        let operation = crate::history::environment::test_support::waiting_operation(&environment);
        let rows = crate::history::provider::load_sessions(
            &environment,
            storage,
            crate::history::provider::LoadOptions::default(),
            &operation,
            &mut |_, _| {},
        )
        .unwrap()
        .conversations;
        operation.finish().unwrap();
        rows
    };
    load();

    std::fs::create_dir_all(archive("archive-two")).unwrap();
    for entry in std::fs::read_dir(&link).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), archive("archive-two").join(entry.file_name())).unwrap();
    }
    std::fs::remove_dir(&link).unwrap();
    link_directory(&archive("archive-two"), &link);
    let rollouts = [
        &bases.session,
        &bases.session_base,
        &bases.subagent,
        &bases.subagent_base,
    ];
    let before = rollouts.map(|rollout| admitted_reads::count(rollout));

    let rows = load();

    let reads: Vec<usize> = rollouts
        .iter()
        .zip(before)
        .map(|(rollout, before)| admitted_reads::count(rollout) - before)
        .collect();
    assert_eq!(
        reads,
        [1, 1, 1, 1],
        "the second load reads each rollout once"
    );
    let row = rows
        .iter()
        .find(|row| row.path == bases.session)
        .expect("the load lists the session");
    let entries = crate::history::sqlite_cache::read_copy_waiting(
        environment.cache_base(),
        row.cache_address.as_ref().unwrap(),
        |held| held.entries(),
    )
    .expect("the load wrote the session's copy");
    assert!(
        format!("{:?}", entries.entries).contains("alpha"),
        "the copy holds the archived rollout's turn"
    );
}
