//! A changed Grok session read on from its stored copy writes what a fresh
//! write of its files holds, as the Codex tests beside it check: each update
//! asserts the listed row and the stored session against a fresh parse and
//! write.

use super::*;
use crate::history::format::grok::fixture::{
    call, envelope, reply, summary, turn_completed, user, write_session, write_subagent_meta,
};

const SESSION: &str = "019a0000-0000-7000-8000-0000000000a8";
const CHILD: &str = "019a0000-0000-7000-8000-0000000000c8";
const CWD_DIR: &str = "%2Ftmp%2Fgrok-project";

/// A Grok session directory under a sessions root, and the cache holding the
/// last write of the session.
struct GrokSession {
    home: tempfile::TempDir,
    cache: TemporaryCache,
    own: PathBuf,
    subagents: Vec<PathBuf>,
    /// The stub's modification time, in seconds, moved on with each update.
    updates: u64,
}

impl GrokSession {
    /// The session whose transcript holds `own`, summarised with `extra`,
    /// with a sub-agent holding `child` when there is one; stored as a load
    /// stores its parse.
    fn stored(own: &[Value], extra: Value, child: Option<&[Value]>) -> Self {
        let home = tempfile::tempdir().unwrap();
        let sessions = home.path().join("sessions");
        let own_path = write_session(
            &sessions,
            CWD_DIR,
            SESSION,
            &summary(SESSION, "/tmp/grok-project", extra),
            own,
        );
        let subagents = child
            .map(|records| {
                let path = write_session(
                    &sessions,
                    CWD_DIR,
                    CHILD,
                    &summary(
                        CHILD,
                        "/tmp/grok-project",
                        json!({"session_kind": "subagent", "agent_name": "explore"}),
                    ),
                    records,
                );
                write_subagent_meta(own_path.parent().unwrap(), CHILD, "explore");
                path
            })
            .into_iter()
            .collect();
        let cache = TemporaryCache::admitted();
        cache.admit_storage(storage_of(Source::Grok)).unwrap();
        let session = Self {
            home,
            cache,
            own: own_path,
            subagents,
            updates: 1,
        };
        let fixture = session.fixture();
        let record = parsed_record(Source::Grok, &fixture.root, &fixture.stub);
        session.cache.write(&record).unwrap();
        session
    }

    /// Rewrites the session's `summary.json`, recorded in `cwd`, with
    /// `extra`.
    fn write_summary(&self, cwd: &str, extra: Value) {
        std::fs::write(
            self.own.with_file_name("summary.json"),
            summary(SESSION, cwd, extra).to_string(),
        )
        .unwrap();
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
            source: Source::Grok,
            root: SessionRoot::new(self.home.path().join("sessions")),
            stub,
            _session: None,
        }
    }

    fn update(&mut self) -> Update {
        self.updates += 1;
        let fixture = self.fixture();
        update_stored(&mut self.cache, &fixture)
    }

    fn bytes_after_stable_cursor(&self, path: &Path) -> u64 {
        bytes_after_stable_cursor(&self.cache, &self.fixture(), path)
    }
}

fn write_records(path: &Path, records: &[Value]) {
    write_lines(
        path,
        &records.iter().map(Value::to_string).collect::<Vec<_>>(),
    );
}

/// The session's opening turn: a prompt, a reply, a call and its result,
/// and the turn's usage.
fn opening() -> Vec<Value> {
    let mut records = vec![
        user(1_000, "Start the work", 0),
        reply(1_100, "Starting.", "p1"),
    ];
    records.extend(call(
        1_200,
        "read-1",
        "read_file",
        "read",
        json!({"target_file": "/tmp/grok-project/notes.md"}),
        "completed",
        json!({"type": "ReadFile", "FileContent": {"content": "the notes"}}),
    ));
    records.push(turn_completed(1_300, "grok-4.6", 100, 20));
    records
}

fn with(records: &[Value], more: &[Value]) -> Vec<Value> {
    records.iter().chain(more).cloned().collect()
}

/// A second turn: a prompt, a reply and the turn's usage.
fn second_turn() -> Vec<Value> {
    vec![
        user(2_000, "What next?", 1),
        reply(2_100, "The release.", "p2"),
        turn_completed(2_200, "grok-4.6", 50, 10),
    ]
}

#[test]
fn records_appended_to_transcript_are_read_from_its_stable_cursor() {
    let mut session = GrokSession::stored(&opening(), json!({"generated_title": "Release"}), None);
    let after_cursor_before = session.bytes_after_stable_cursor(&session.own.clone());
    write_records(&session.own, &with(&opening(), &second_turn()));

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 1,
        "the read opens the changed transcript alone"
    );
    let appended = second_turn()
        .iter()
        .map(|record| record.to_string().len() as u64 + 1)
        .sum::<u64>();
    assert_eq!(
        update.bytes_read,
        after_cursor_before + appended,
        "the read reads from the stable cursor on"
    );
}

#[test]
fn turn_appended_to_subagent_transcript_is_read_as_its_window() {
    let child = [
        user(1_320, "the spawn prompt", 0),
        reply(1_350, "Reading the notes.", "c1"),
        turn_completed(1_360, "grok-4.6", 40, 8),
    ];
    let mut session = GrokSession::stored(&opening(), json!({}), Some(&child));
    write_records(
        &session.subagents[0].clone(),
        &with(&child, &[reply(1_370, "The notes list the steps.", "c2")]),
    );

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 1,
        "the read opens the changed sub-agent transcript alone"
    );
}

#[test]
fn rewritten_summary_changes_row_as_full_read_does() {
    let titled = json!({"generated_title": "Release", "updated_at": "1"});
    let mut session = GrokSession::stored(&opening(), titled, None);
    let turn = second_turn();
    let mut records = opening();

    session.write_summary(
        "/tmp/grok-project",
        json!({"generated_title": "Release", "updated_at": "2"}),
    );
    records.push(turn[0].clone());
    write_records(&session.own, &records);
    assert_eq!(
        session.update().outcome,
        Ok(()),
        "a summary rewritten with the same title resumes"
    );

    session.write_summary(
        "/tmp/grok-project",
        json!({"generated_title": "Release notes"}),
    );
    records.push(turn[1].clone());
    write_records(&session.own, &records);
    assert_eq!(
        session.update().outcome,
        Ok(()),
        "a title Grok generated again resumes with the new title"
    );

    session.write_summary(
        "/tmp/grok-project",
        json!({"generated_title": "Chosen", "title_is_manual": true}),
    );
    records.push(turn[2].clone());
    write_records(&session.own, &records);
    assert_eq!(
        session.update().outcome,
        Ok(()),
        "a generated title replaced by a chosen one resumes with the chosen one"
    );

    session.write_summary(
        "/tmp/grok-project",
        json!({"generated_title": "Chosen again", "title_is_manual": true}),
    );
    records.push(user(3_000, "And then?", 2));
    write_records(&session.own, &records);
    assert_eq!(
        session.update().outcome,
        Ok(()),
        "a chosen title renamed resumes with its new name"
    );

    session.write_summary(
        "/tmp/elsewhere",
        json!({"generated_title": "Chosen again", "title_is_manual": true}),
    );
    records.push(reply(3_100, "Then the tag.", "p3"));
    write_records(&session.own, &records);
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnRewritten),
        "a working directory summary.json no longer names is another identity"
    );
}

#[test]
fn summarys_model_is_replaced_once_a_turn_names_one() {
    let asked = [
        user(1_000, "Start the work", 0),
        reply(1_100, "Starting.", "p1"),
    ];
    let mut session = GrokSession::stored(&asked, json!({}), None);
    let mut records = asked.to_vec();

    records.push(reply(1_150, "Still starting.", "p1"));
    write_records(&session.own, &records);
    assert_eq!(
        session.update().outcome,
        Ok(()),
        "a session without a completed turn resumes under the summary's model"
    );

    records.push(turn_completed(1_300, "grok-4.7", 100, 20));
    write_records(&session.own, &records);
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::LeadingChanged),
        "the turn naming a model removes the summary's model row"
    );

    records.extend(second_turn());
    write_records(&session.own, &records);
    assert_eq!(
        session.update().outcome,
        Ok(()),
        "the session resumes again"
    );
}

/// A stored copy with fewer leading rows than the read's leading entries, its
/// model row moved off line 0, is read whole: a resumed write rewrites the
/// leading rows in place and never moves the entries after them. The write
/// matches a fresh write, semantic turns included.
#[test]
fn leading_row_count_other_than_the_reads_reads_session_whole() {
    let asked = [
        user(1_000, "Start the work", 0),
        reply(1_100, "Starting.", "p1"),
    ];
    let mut session = GrokSession::stored(&asked, json!({}), None);
    let id = session.fixture().session_id(&session.cache);
    let moved = session
        .cache
        .connection
        .execute(
            "UPDATE entries SET record_line = 1 \
             WHERE session = ?1 AND sequence = 0 AND record_line = 0",
            [id],
        )
        .unwrap();
    assert_eq!(
        moved, 1,
        "the stored copy opens with the summary's model row"
    );
    let mut records = asked.to_vec();
    records.push(reply(1_150, "Still starting.", "p1"));
    write_records(&session.own, &records);

    assert_eq!(
        session.update().outcome,
        Err(WholeRead::LeadingChanged),
        "a leading row count other than the read's reads the session whole"
    );
}

#[test]
fn input_completed_for_call_before_checkpoint_is_read_whole() {
    let mut open = call(
        1_200,
        "late",
        "read_file",
        "read",
        json!({"target_file": "/tmp/a.rs"}),
        "completed",
        json!({"type": "ReadFile", "FileContent": {"content": "a"}}),
    );
    open.truncate(1);
    let asked = with(
        &[user(1_000, "Start the work", 0)],
        &[
            open[0].clone(),
            user(2_000, "Next", 1),
            turn_completed(2_100, "grok-4.6", 1, 1),
        ],
    );
    let mut session = GrokSession::stored(&asked, json!({}), None);
    let late_input = envelope(
        2_200,
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "late",
               "rawInput": {"target_file": "/tmp/b.rs"}}),
        json!({}),
    );
    write_records(&session.own, &with(&asked, &[late_input]));

    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnChangesEarlierEntry),
        "an update completing a call's input after its turn ended reads the session whole"
    );
}

#[test]
fn rewritten_or_shrunk_transcript_is_read_whole() {
    let mut session = GrokSession::stored(&opening(), json!({}), None);
    let mut rewritten = opening();
    rewritten[1] = reply(1_100, "Starting at once.", "p1");
    write_records(&session.own, &rewritten);
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnRewritten),
        "a rewritten transcript is read whole"
    );

    write_records(&session.own, &opening()[..2]);
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnRewritten),
        "a shrunk transcript is read whole"
    );
}
