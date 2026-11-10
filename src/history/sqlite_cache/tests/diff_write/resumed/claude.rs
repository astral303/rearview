//! A changed Claude session read on from its stored copy writes what a fresh
//! write of its files holds, as the Codex tests beside it check: each update
//! asserts the listed row and the stored session against a fresh parse and
//! write.

use super::*;

const SESSION: &str = "5d1e2f3a-4b5c-4d6e-8f7a-8b9c0d1e2f3a";
const SCOUT: &str = "a1111111111111111";
const SCOUT_CALL: &str = "toolu_01SCOUTAAAAAAAAAAAAAAAA";

/// A Claude session's files under a project directory, and the cache
/// holding the last write of the session.
struct ClaudeSession {
    home: tempfile::TempDir,
    base: tempfile::TempDir,
    cache: OwnedCache,
    own: PathBuf,
    subagents: Vec<PathBuf>,
    /// The stub's modification time, in seconds, moved on with each update.
    updates: u64,
    /// Another spelling of `home`, which later reads name the session's
    /// files through; `None` for `home` as it is.
    home_spelling: Option<PathBuf>,
}

impl ClaudeSession {
    /// The session whose own transcript holds `own` and whose sub-agents are
    /// `subagents`, each an agent id and its lines, launched by
    /// [`SCOUT_CALL`]; stored as a load stores its parse.
    fn stored(own: &[String], subagents: &[(&str, Vec<String>)]) -> Self {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("projects").join("-tmp-project");
        std::fs::create_dir_all(&project).unwrap();
        let own_path = project.join(format!("{SESSION}.jsonl"));
        write_lines(&own_path, own);
        let base = tempfile::tempdir().unwrap();
        let cache = admitted_cache(base.path());
        cache.admit_storage(storage_of(Source::Claude)).unwrap();
        let mut session = Self {
            home,
            base,
            cache,
            own: own_path,
            subagents: Vec::new(),
            updates: 1,
            home_spelling: None,
        };
        for (agent, lines) in subagents {
            session.add_subagent(agent, lines);
        }
        session.store();
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
        Fixture {
            source: Source::Claude,
            root: SessionRoot::new(spelled(self.own.parent().unwrap())),
            stub,
            _session: None,
        }
    }

    /// Name the session's files from now on through `home` spelled through
    /// a directory inside it and `..`, which the file system canonicalises
    /// to `home`.
    fn spell_home_through_parent(&mut self) {
        let elsewhere = self.home.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        self.home_spelling = Some(elsewhere.join(".."));
    }

    /// Adds the transcript of sub-agent `agent`, launched by
    /// [`SCOUT_CALL`], holding `lines`, after the others.
    fn add_subagent(&mut self, agent: &str, lines: &[String]) {
        let mut text = lines.join("\n");
        text.push('\n');
        self.add_subagent_bytes(agent, text.as_bytes());
        let sidecar = json!({"agentType": "general-purpose", "description": "Scout the notes", "toolUseId": SCOUT_CALL});
        std::fs::write(
            self.subagent(agent).with_extension("meta.json"),
            sidecar.to_string(),
        )
        .unwrap();
    }

    /// Adds the transcript of sub-agent `agent`, holding `bytes`, without a
    /// sidecar, after the others.
    fn add_subagent_bytes(&mut self, agent: &str, bytes: &[u8]) {
        let path = self.subagent(agent);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        self.subagents.push(path);
    }

    /// Stores the parse of the session's files as they are now, as a load
    /// stores it.
    fn store(&mut self) {
        let fixture = self.fixture();
        let record = parsed_record(Source::Claude, &fixture.root, &fixture.stub);
        self.cache.write(&record).unwrap();
    }

    fn subagent(&self, agent: &str) -> PathBuf {
        self.own
            .with_extension("")
            .join("subagents")
            .join(format!("agent-{agent}.jsonl"))
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

fn record(time: &str, mut fields: Value) -> String {
    fields["timestamp"] = json!(format!("2026-10-01T{time}Z"));
    fields["cwd"] = json!("/tmp/project");
    fields.to_string()
}

fn prompt(time: &str, text: &str) -> String {
    record(
        time,
        json!({"type": "user", "message": {"role": "user", "content": text}}),
    )
}

fn assistant(time: &str, id: &str, block: Value) -> String {
    record(
        time,
        json!({"type": "assistant", "message": {"role": "assistant", "id": id, "model": "claude-opus-5", "content": [block], "usage": {"input_tokens": 10, "output_tokens": 5}}}),
    )
}

fn reply(time: &str, id: &str, text: &str) -> String {
    assistant(time, id, json!({"type": "text", "text": text}))
}

fn call(time: &str, id: &str, call_id: &str, name: &str, input: Value) -> String {
    assistant(
        time,
        id,
        json!({"type": "tool_use", "id": call_id, "name": name, "input": input}),
    )
}

fn result(time: &str, call_id: &str, text: &str) -> String {
    record(
        time,
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": call_id, "content": text}]}}),
    )
}

/// A hook's message on the call `call_id`, for the hook event `event`.
fn hook(time: &str, call_id: &str, event: &str, text: &str) -> String {
    record(
        time,
        json!({"type": "attachment", "attachment": {"type": "hook_system_message", "content": text, "hookName": format!("{event}:Bash"), "toolUseID": call_id, "hookEvent": event}}),
    )
}

/// The session's [`opening`], then a reply after the sub-agent's turns and
/// a third prompt.
fn opening_past_subagent() -> Vec<String> {
    with(
        &opening(),
        &[
            reply("10:01:01", "msg_a3", "Next is the release."),
            prompt("10:02:00", "Go on"),
        ],
    )
}

/// The session's opening turns: a prompt, the call launching
/// [`SCOUT`], its result, and a second prompt.
fn opening() -> Vec<String> {
    vec![
        prompt("10:00:00", "Start the work"),
        reply("10:00:01", "msg_a1", "Starting."),
        call(
            "10:00:02",
            "msg_a2",
            SCOUT_CALL,
            "Agent",
            json!({"description": "Scout the notes", "prompt": "Scout the notes", "subagent_type": "general-purpose"}),
        ),
        result("10:00:30", SCOUT_CALL, "The notes list the steps."),
        prompt("10:01:00", "What next?"),
    ]
}

/// [`SCOUT`]'s transcript: its prompt, repeating its call's, then `turns`.
fn scout(turns: &[String]) -> Vec<String> {
    let mut lines = vec![
        prompt("10:00:03", "Scout the notes"),
        reply("10:00:04", "msg_s1", "Reading the notes."),
    ];
    lines.extend_from_slice(turns);
    lines
}

fn with(lines: &[String], more: &[String]) -> Vec<String> {
    lines.iter().chain(more).cloned().collect()
}

fn bytes_of(lines: &[String]) -> u64 {
    lines.iter().map(|line| line.len() as u64 + 1).sum()
}

#[test]
fn turn_appended_to_own_transcript_is_read_from_its_stable_cursor() {
    let mut session = ClaudeSession::stored(&opening(), &[(SCOUT, scout(&[]))]);
    let own = session.own.clone();
    let after_cursor_before = session.bytes_after_stable_cursor(&own);
    let appended = [reply("10:01:01", "msg_a3", "Run the tests.")];
    write_lines(&own, &with(&opening(), &appended));

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 1,
        "the read opens the changed transcript alone"
    );
    assert_eq!(
        update.bytes_read,
        after_cursor_before + bytes_of(&appended),
        "the read reads from the stable cursor on"
    );
}

#[test]
fn hook_message_whose_call_is_answered_in_next_update_resumes() {
    let hooked = with(
        &opening(),
        &[
            call(
                "10:01:01",
                "msg_b1",
                "toolu_ls",
                "Bash",
                json!({"command": "ls"}),
            ),
            hook("10:01:02", "toolu_ls", "PreToolUse", "SAFE: a listing"),
        ],
    );
    let mut session = ClaudeSession::stored(&hooked, &[(SCOUT, scout(&[]))]);
    write_lines(
        &session.own.clone(),
        &with(&hooked, &[result("10:01:03", "toolu_ls", "a.rs")]),
    );

    assert_eq!(
        session.update().outcome,
        Ok(()),
        "the hook's message moves into the result read in the next update"
    );
}

#[test]
fn delivered_handback_in_next_update_folds_its_call() {
    let handback = call(
        "10:00:05",
        "msg_s2",
        "toolu_handback",
        "SubagentHandback",
        json!({"report": "The notes list the steps."}),
    );
    let mut session = ClaudeSession::stored(
        &opening(),
        &[(SCOUT, scout(std::slice::from_ref(&handback)))],
    );
    write_lines(
        &session.subagent(SCOUT),
        &scout(&[
            handback,
            result("10:00:06", "toolu_handback", "{\"success\":true}"),
        ]),
    );

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 1,
        "the read opens the sub-agent transcript alone"
    );
}

#[test]
fn reply_whose_blocks_span_two_updates_resumes_as_one_message() {
    let started = with(&opening(), &[reply("10:01:01", "msg_b", "First block.")]);
    let scout_started = scout(&[reply("10:00:05", "msg_s2", "First finding.")]);
    let mut session = ClaudeSession::stored(&started, &[(SCOUT, scout_started.clone())]);
    write_lines(
        &session.own.clone(),
        &with(&started, &[reply("10:01:02", "msg_b", "Second block.")]),
    );
    write_lines(
        &session.subagent(SCOUT),
        &with(
            &scout_started,
            &[reply("10:00:06", "msg_s2", "Second finding.")],
        ),
    );

    assert_eq!(
        session.update().outcome,
        Ok(()),
        "a reply continued in the session and in its sub-agent resumes"
    );
}

#[test]
fn compaction_appended_resumes() {
    let mut session = ClaudeSession::stored(&opening(), &[(SCOUT, scout(&[]))]);
    let compacted = [
        record(
            "10:02:00",
            json!({"type": "system", "subtype": "compact_boundary", "content": "Conversation compacted"}),
        ),
        record(
            "10:02:00",
            json!({"type": "user", "isCompactSummary": true, "message": {"role": "user", "content": "This session is being continued from a previous conversation."}}),
        ),
        reply("10:02:01", "msg_c1", "Continuing."),
    ];
    write_lines(&session.own.clone(), &with(&opening(), &compacted));

    assert_eq!(session.update().outcome, Ok(()), "the compaction resumes");
}

#[test]
fn malformed_line_near_end_resumes_and_gains_its_context_after() {
    let broken = with(&opening(), &["{\"type\": \"user\", truncated".to_owned()]);
    let mut session = ClaudeSession::stored(&broken, &[(SCOUT, scout(&[]))]);
    write_lines(
        &session.own.clone(),
        &with(
            &broken,
            &[
                reply("10:01:01", "msg_a3", "Run the tests."),
                prompt("10:01:02", "Done?"),
            ],
        ),
    );

    assert_eq!(
        session.update().outcome,
        Ok(()),
        "the malformed line is read again with the lines after it"
    );
}

#[test]
fn new_subagent_transcript_resumes() {
    let mut session = ClaudeSession::stored(&opening(), &[]);
    session.add_subagent(SCOUT, &scout(&[]));

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 1,
        "the read opens the new sub-agent transcript alone"
    );
}

/// A session cached through one spelling of its project directory, read on
/// through another after its sub-agent and its own transcript grew: the read
/// resumes with the stored launch ledger, opens the two changed transcripts
/// alone, and writes what a fresh parse holds.
#[test]
fn append_after_root_spelling_changes_resumes_as_fresh_parse() {
    let mut session = ClaudeSession::stored(&opening(), &[(SCOUT, scout(&[]))]);
    write_lines(
        &session.subagent(SCOUT),
        &scout(&[reply("10:00:40", "msg_s2", "The notes list the steps.")]),
    );
    write_lines(
        &session.own.clone(),
        &with(&opening(), &[reply("10:01:01", "msg_a3", "Run the tests.")]),
    );
    session.spell_home_through_parent();

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 2,
        "the read opens the session's transcript and the changed sub-agent's"
    );
}

#[test]
fn ai_title_appended_takes_the_summary_search_reads_from_one_before_checkpoint() {
    // Before the reply the checkpoint stands before, the last of the turn
    // before the stable cursor.
    let mut titled = opening();
    titled.insert(
        1,
        record(
            "10:00:00.500",
            json!({"type": "ai-title", "aiTitle": "Scouting the notes"}),
        ),
    );
    let mut session = ClaudeSession::stored(&titled, &[(SCOUT, scout(&[]))]);
    write_lines(
        &session.own.clone(),
        &with(
            &titled,
            &[
                prompt("10:02:00", "And the release?"),
                record(
                    "10:02:01",
                    json!({"type": "ai-title", "aiTitle": "Release steps"}),
                ),
            ],
        ),
    );

    assert_eq!(
        session.update().outcome,
        Ok(()),
        "a later AI title resumes and replaces the stored one in search"
    );
}

/// A sub-agent's turns with a shell call answered before its second prompt.
fn scout_with_answered_call() -> Vec<String> {
    scout(&[
        call(
            "10:00:05",
            "msg_s2",
            "toolu_scout_ls",
            "Bash",
            json!({"command": "ls"}),
        ),
        result("10:00:06", "toolu_scout_ls", "a.rs"),
        prompt("10:00:07", "Now the tests"),
    ])
}

#[test]
fn hook_naming_call_before_stable_cursor_reads_that_file_whole() {
    let late_hook = hook("10:00:08", "toolu_scout_ls", "PostToolUse", "listed");
    let mut session = ClaudeSession::stored(&opening(), &[(SCOUT, scout_with_answered_call())]);
    write_lines(
        &session.subagent(SCOUT),
        &with(
            &scout_with_answered_call(),
            std::slice::from_ref(&late_hook),
        ),
    );

    let update = session.update();

    assert_eq!(
        update.outcome,
        Ok(()),
        "the session resumes, its sub-agent read whole"
    );
    assert_eq!(update.transcripts_read, 1);

    let answered = with(
        &opening(),
        &[
            call(
                "10:01:01",
                "msg_b1",
                "toolu_ls",
                "Bash",
                json!({"command": "ls"}),
            ),
            result("10:01:02", "toolu_ls", "a.rs"),
            prompt("10:01:03", "Now the tests"),
        ],
    );
    write_lines(&session.own.clone(), &answered);
    session.update();
    write_lines(
        &session.own.clone(),
        &with(
            &answered,
            &[hook("10:01:04", "toolu_ls", "PostToolUse", "listed")],
        ),
    );
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnChangesEarlierEntry),
        "a hook naming a call before the session's stable cursor reads the session whole"
    );
}

/// A sub-agent whose transcript holds no record: NUL bytes without a line
/// ending, as a crash leaves one. A read leaves it out of the session.
const LEFT_OUT: &str = "a2222222222222222";

/// The session of [`opening`] and [`SCOUT`], with [`LEFT_OUT`]'s transcript
/// of NUL bytes, stored.
fn session_with_left_out_subagent() -> ClaudeSession {
    let mut session = ClaudeSession::stored(&opening(), &[(SCOUT, scout(&[]))]);
    session.add_subagent_bytes(LEFT_OUT, &[0; 64]);
    session.store();
    session
}

#[test]
fn subagent_transcript_without_record_stays_left_out_of_resumed_read() {
    let mut session = session_with_left_out_subagent();
    write_lines(
        &session.own.clone(),
        &with(&opening(), &[reply("10:01:01", "msg_a3", "Run the tests.")]),
    );

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 1,
        "the read opens the changed transcript alone"
    );
}

#[test]
fn left_out_subagent_transcript_gaining_records_is_read_whole() {
    let mut session = session_with_left_out_subagent();
    write_lines(
        &session.subagent(LEFT_OUT),
        &[
            prompt("10:01:05", "Check the tests"),
            reply("10:01:06", "msg_l1", "The tests pass."),
        ],
    );

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 1,
        "the read opens the changed sub-agent transcript alone"
    );
}

#[test]
fn unreadable_stored_resume_state_is_named_as_whole_read_reason() {
    let mut session = ClaudeSession::stored(&opening(), &[(SCOUT, scout(&[]))]);
    let id = session.fixture().session_id(&session.cache);
    session
        .cache
        .connection
        .execute(
            "UPDATE transcript_reads SET fold_state = X'7B' WHERE session = ?1 AND position = 1",
            [id],
        )
        .unwrap();
    let appended = with(&opening(), &[reply("10:01:01", "msg_a3", "Run the tests.")]);
    write_lines(&session.own.clone(), &appended);
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::SubagentShareUnreadable),
        "a sub-agent's share that does not decode is named"
    );

    let id = session.fixture().session_id(&session.cache);
    session
        .cache
        .connection
        .execute(
            "UPDATE session_checkpoints SET checkpoint = X'7B' WHERE session = ?1",
            [id],
        )
        .unwrap();
    write_lines(
        &session.own.clone(),
        &with(
            &appended,
            &[reply("10:01:02", "msg_a3", "Then the release.")],
        ),
    );
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::CheckpointUnreadable),
        "a checkpoint that does not decode is named"
    );
}

#[test]
fn checkpoint_stands_past_spliced_subagent_turns() {
    let mut session = ClaudeSession::stored(&opening_past_subagent(), &[(SCOUT, scout(&[]))]);
    write_lines(
        &session.own.clone(),
        &with(
            &opening_past_subagent(),
            &[reply("10:02:01", "msg_a4", "Going on.")],
        ),
    );

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.tail_entries, 3,
        "the read derives the entries from the reply before the last prompt on"
    );
}

#[test]
fn subagent_reply_spanning_session_reply_stays_whole_after_checkpoint() {
    // The scout's reply has a block before the session's second prompt and
    // one after its reply to it, where the checkpoint would otherwise stand.
    let scout_reply = scout(&[
        reply("10:00:50", "msg_s2", "First finding."),
        reply("10:01:30", "msg_s2", "Second finding."),
    ]);
    let mut session = ClaudeSession::stored(&opening_past_subagent(), &[(SCOUT, scout_reply)]);
    write_lines(
        &session.own.clone(),
        &with(
            &opening_past_subagent(),
            &[reply("10:02:01", "msg_a4", "Going on.")],
        ),
    );

    assert_eq!(
        session.update().outcome,
        Ok(()),
        "the checkpoint stands before the sub-agent's reply, so no reply continues across it"
    );
}

/// `count` prompts, each answered by its own reply, from 10:00 on.
fn answered_prompts(count: usize) -> Vec<String> {
    (0..count)
        .flat_map(|index| {
            let (minute, second) = (index / 30, (index % 30) * 2);
            [
                prompt(&format!("10:{minute:02}:{second:02}"), "Next step?"),
                reply(
                    &format!("10:{minute:02}:{:02}", second + 1),
                    &format!("msg_h{index}"),
                    "Done.",
                ),
            ]
        })
        .collect()
}

/// The bytes of the session checkpoint stored after `reply_count` answered
/// prompts and one more appended.
fn checkpoint_bytes_after_append(reply_count: usize) -> usize {
    let mut session = ClaudeSession::stored(&answered_prompts(reply_count), &[]);
    write_lines(&session.own.clone(), &answered_prompts(reply_count + 1));
    assert_eq!(session.update().outcome, Ok(()), "the session resumes");
    session
        .cache
        .connection
        .query_row(
            "SELECT length(checkpoint) FROM session_checkpoints",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap() as usize
}

#[test]
fn session_checkpoint_size_stays_flat_as_replies_grow() {
    let short = checkpoint_bytes_after_append(10);
    let long = checkpoint_bytes_after_append(400);

    assert!(
        long < short + 32,
        "the checkpoint doesn't grow with each earlier reply: {short} bytes after 10 replies, \
         {long} after 400"
    );
}

/// Claude Code continues a reply after a `Read` of an image: the call, its
/// result, a meta user record describing the image, then the reply's next call
/// under the same message id. A sub-agent reply whose next call is appended
/// after that record was stored reads the sub-agent whole: the sub-agent's
/// stored share holds one token total for the replies before its stable
/// cursor, and the reply's usage counts once.
#[test]
fn subagent_reply_continued_after_an_image_record_reads_subagent_whole() {
    let checked = [
        prompt("10:03:00", "Check the screenshot too"),
        call(
            "10:03:01",
            "msg_s3",
            "toolu_read",
            "Read",
            json!({"file_path": "/tmp/project/screen.png"}),
        ),
        result("10:03:02", "toolu_read", "Read the image"),
        record(
            "10:03:02",
            json!({"type": "user", "isMeta": true, "turnCompanion": true, "message": {"role": "user", "content": "[Image: original 3200x1008, displayed at 2000x630. Multiply coordinates by 1.60 to map to original image.]"}}),
        ),
    ];
    let mut session = ClaudeSession::stored(&opening_past_subagent(), &[(SCOUT, scout(&checked))]);
    write_lines(
        &session.subagent(SCOUT),
        &scout(&with(
            &checked,
            &[call(
                "10:03:03",
                "msg_s3",
                "toolu_tests",
                "Bash",
                json!({"command": "cargo test"}),
            )],
        )),
    );

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.bytes_read,
        std::fs::metadata(session.subagent(SCOUT)).unwrap().len(),
        "the sub-agent transcript is read whole"
    );
    assert_eq!(
        session.fixture().listed(&session.cache).total_tokens,
        75,
        "each of the five replies' 15 tokens counts once"
    );
}

/// Claude Code writes each block of a reply as a record of its own under the
/// reply's message id. A sub-agent window holding a new reply of two
/// records, after the open reply the window starts at, looks each of its two
/// reply ids up once, finds neither stored before it, and resumes.
#[test]
fn subagent_window_looks_each_reply_id_up_once() {
    let mut session = ClaudeSession::stored(&opening_past_subagent(), &[(SCOUT, scout(&[]))]);
    write_lines(
        &session.subagent(SCOUT),
        &scout(&[
            prompt("10:03:00", "Check the tests too"),
            reply("10:03:01", "msg_s3", "Running them."),
            call(
                "10:03:02",
                "msg_s3",
                "toolu_tests",
                "Bash",
                json!({"command": "cargo test"}),
            ),
        ]),
    );

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert!(
        update.bytes_read < std::fs::metadata(session.subagent(SCOUT)).unwrap().len(),
        "the sub-agent transcript is read from its stable cursor"
    );
    assert_eq!(
        update.reply_lookups, 2,
        "each of the window's two reply ids is looked up once"
    );
}

#[test]
fn subagent_continued_after_checkpoint_resumes_its_turns_before_it_in_place() {
    let mut session = ClaudeSession::stored(&opening_past_subagent(), &[(SCOUT, scout(&[]))]);
    write_lines(
        &session.subagent(SCOUT),
        &scout(&[
            prompt("10:03:00", "Check the tests too"),
            reply("10:03:01", "msg_s3", "The tests pass."),
        ]),
    );

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 1,
        "the read opens the sub-agent transcript alone"
    );
}
