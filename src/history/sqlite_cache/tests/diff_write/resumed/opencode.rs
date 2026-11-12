//! A changed OpenCode session read on from its stored copy writes what a
//! fresh write of its rows holds, as the Codex tests beside it check: each
//! update asserts the listed row and the stored session against a fresh
//! parse and write.

use super::*;
use crate::history::format::opencode::{fixture, session_ref};

const SESSION: &str = "ses_019f0000000aResumedSessA";
const CHILD: &str = "ses_019f0000000bResumedChild";

/// An OpenCode session in a database, and the cache holding the last write
/// of the session.
struct OpenCodeSession {
    database_directory: tempfile::TempDir,
    base: tempfile::TempDir,
    cache: OwnedCache,
    subagents: Vec<String>,
    /// The stub's modification time, in seconds, moved on with each update.
    updates: u64,
}

impl OpenCodeSession {
    /// The fixture session in OpenCode `version`'s tables, with a sub-agent
    /// session beneath it when `with_subagent`; stored as a load stores its
    /// parse.
    fn stored(version: u8, with_subagent: bool) -> Self {
        let database_directory = tempfile::tempdir().unwrap();
        let database = database_directory.path().join("opencode.db");
        let connection = if version == 1 {
            let connection = fixture::create_database(&database);
            fixture::standard_session(&connection, SESSION);
            connection
        } else {
            let connection = fixture::create_opencode_2_database(&database);
            fixture::standard_opencode_2_session(&connection, SESSION);
            connection
        };
        let mut subagents = Vec::new();
        if with_subagent {
            fixture::insert_session(
                &connection,
                &fixture::SessionSpec {
                    id: CHILD,
                    parent_id: Some(SESSION),
                    directory: "/tmp/opencode-project",
                    title: "scout",
                    created_ms: 1755000115000,
                    updated_ms: 1755000115000,
                    archived_ms: None,
                },
            );
            connection
                .execute_batch(&reply(
                    CHILD,
                    "child_1",
                    1755000115000,
                    "Reading the notes.",
                ))
                .unwrap();
            subagents.push(CHILD.to_owned());
        }
        drop(connection);
        let base = tempfile::tempdir().unwrap();
        let cache = admitted_cache(base.path());
        cache.admit_storage(storage_of(Source::OpenCode)).unwrap();
        let mut session = Self {
            database_directory,
            base,
            cache,
            subagents,
            updates: 1,
        };
        let fixture = session.fixture();
        let record = parsed_record(Source::OpenCode, &fixture.root, &fixture.stub);
        session.cache.write(&record).unwrap();
        session
    }

    fn database(&self) -> PathBuf {
        self.database_directory.path().join("opencode.db")
    }

    fn fixture(&self) -> Fixture {
        let database = self.database();
        let locator = session_ref(&database, SESSION);
        let subagents: Vec<PathBuf> = self
            .subagents
            .iter()
            .map(|id| session_ref(&database, id))
            .collect();
        let root = SessionRoot::new(database).in_agent_tree();
        // Give the stub the fingerprint the rows hold now: a stored OpenCode
        // session validates against it.
        let discovered = Source::OpenCode
            .provider()
            .storage()
            .rediscover_unlisted_session(
                &root,
                &locator,
                &crate::history::provider::walk::SystemLister,
            )
            .unwrap()
            .unwrap();
        let stub = SessionStub {
            locator,
            subagents,
            cache_key: CACHE_KEY.to_owned(),
            fingerprint: discovered.fingerprint,
        };
        Fixture {
            source: Source::OpenCode,
            root,
            stub,
            _session: None,
        }
    }

    /// Runs `sql` on the database.
    fn change(&self, sql: &str) {
        Connection::open(self.database())
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }

    fn update(&mut self) -> Update {
        self.updates += 1;
        let fixture = self.fixture();
        update_stored(&mut self.cache, self.base.path(), &fixture)
    }
}

/// An OpenCode 1 reply of the session `session_id`, a message and its text
/// part at `time`.
fn reply(session_id: &str, id: &str, time: i64, text: &str) -> String {
    format!(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES \
         ('msg_{id}', '{session_id}', {time}, {time}, '{{\"role\":\"assistant\",\"time\":{{\"created\":{time}}},\"modelID\":\"claude-opus-4-6\"}}'); \
         INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES \
         ('prt_{id}', 'msg_{id}', '{session_id}', {time}, {time}, '{{\"type\":\"text\",\"text\":\"{text}\"}}');"
    )
}

#[test]
fn appended_message_resumes_in_both_versions() {
    for version in [1, 2] {
        let mut session = OpenCodeSession::stored(version, false);
        session.change(&if version == 1 {
            reply(SESSION, "zz", 1755000500000, "a later answer")
        } else {
            format!(
                "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) \
                 VALUES ('msg_zz', '{SESSION}', 'user', 6, 1755000500000, 1755000500000, \
                 '{{\"text\":\"a later question\",\"time\":{{\"created\":1755000500000}}}}')"
            )
        });

        let update = session.update();

        assert_eq!(
            update.outcome,
            Ok(()),
            "OpenCode {version}: the session resumes"
        );
        assert_eq!(
            update.transcripts_read, 1,
            "OpenCode {version}: the read reads the session alone"
        );
    }
}

#[test]
fn part_appended_or_completing_in_place_resumes() {
    let mut session = OpenCodeSession::stored(1, false);
    session.change(&format!(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES \
         ('prt_asst_0006_{SESSION}', 'msg_asst_{SESSION}', '{SESSION}', 1755000250000, 1755000250000, \
         '{{\"type\":\"text\",\"text\":\"one more thing\"}}')"
    ));
    assert_eq!(
        session.update().outcome,
        Ok(()),
        "a part appended to the last message resumes"
    );

    session.change(&format!(
        "UPDATE part SET time_updated = 1755000260000, \
         data = '{{\"type\":\"text\",\"text\":\"the defect hides in lib.rs, line 4\"}}' \
         WHERE id = 'prt_asst_0004_{SESSION}'"
    ));
    assert_eq!(
        session.update().outcome,
        Ok(()),
        "a part of the last message written again in place resumes"
    );

    let mut session = OpenCodeSession::stored(2, false);
    session.change(&format!(
        "UPDATE session_message SET time_updated = 1755000160000, \
         data = replace(data, 'OPENCODE_2_COMPACTION_SUMMARY', 'a longer summary') \
         WHERE session_id = '{SESSION}' AND seq = 5"
    ));
    assert_eq!(
        session.update().outcome,
        Ok(()),
        "OpenCode 2's last message written again in place resumes"
    );
}

#[test]
fn edited_or_deleted_earlier_row_or_another_session_row_reads_whole() {
    for (version, change) in [
        (
            1,
            format!(
                "UPDATE part SET time_updated = 1755000500000, \
                 data = '{{\"type\":\"text\",\"text\":\"an edited question\"}}' \
                 WHERE id = 'prt_user_{SESSION}'"
            ),
        ),
        (
            1,
            format!("DELETE FROM part WHERE id = 'prt_user_a_note_{SESSION}'"),
        ),
        (
            1,
            "UPDATE session SET directory = '/tmp/elsewhere'".to_owned(),
        ),
        (
            2,
            format!(
                "UPDATE session_message SET time_updated = 1755000500000, \
                 data = replace(data, 'find the opencode 2 defect', 'an edited question') \
                 WHERE session_id = '{SESSION}' AND seq = 0"
            ),
        ),
        (
            2,
            format!("DELETE FROM session_message WHERE session_id = '{SESSION}' AND seq = 2"),
        ),
    ] {
        let mut session = OpenCodeSession::stored(version, false);
        session.change(&change);

        assert_eq!(
            session.update().outcome,
            Err(WholeRead::OwnRewritten),
            "OpenCode {version}, {change}: the session is read whole"
        );
    }
}

#[test]
fn renamed_session_resumes_in_both_versions() {
    for version in [1, 2] {
        let mut session = OpenCodeSession::stored(version, false);
        let table = if version == 1 {
            "session"
        } else {
            "session_v2"
        };
        session.change(&format!(
            "UPDATE {table} SET title = 'Release notes', time_updated = 1755000900000 \
             WHERE id = '{SESSION}'"
        ));

        let update = session.update();

        assert_eq!(
            update.outcome,
            Ok(()),
            "OpenCode {version}: a renamed session resumes"
        );
        assert_eq!(
            update.transcripts_read, 1,
            "OpenCode {version}: the read reads the session alone"
        );
    }
}

#[test]
fn message_appended_to_subagent_session_resumes() {
    let mut session = OpenCodeSession::stored(1, true);
    session.change(&reply(
        CHILD,
        "child_2",
        1755000116000,
        "The notes list the steps.",
    ));

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 1,
        "the read opens the changed sub-agent session alone"
    );
}
