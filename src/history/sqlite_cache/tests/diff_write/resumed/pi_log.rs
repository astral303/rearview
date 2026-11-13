//! A changed Pi or OMP session read on from its stored copy writes what a
//! fresh write of its files holds, as the Codex tests beside it check: each
//! update asserts the listed row and the stored session against a fresh
//! parse and write.

use super::*;

/// A Pi or OMP log under a sessions root, and the cache holding the last
/// write of its session.
struct LogSession {
    source: Source,
    home: tempfile::TempDir,
    cache: TemporaryCache,
    own: PathBuf,
    subagents: Vec<PathBuf>,
    /// The stub's modification time, in seconds, moved on with each update.
    updates: u64,
}

impl LogSession {
    /// The `source` session whose log holds `own` and whose sub-agent logs,
    /// in its artifacts directory, hold `subagents`, each a name and its
    /// lines; stored as a load stores its parse.
    fn stored(source: Source, own: &[String], subagents: &[(&str, Vec<String>)]) -> Self {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("sessions/--tmp-project--");
        std::fs::create_dir_all(&project).unwrap();
        let own_path = project.join("2026-08-01T10-00-00-000Z_session.jsonl");
        write_lines(&own_path, own);
        let subagents = subagents
            .iter()
            .map(|(name, lines)| {
                let path = own_path.with_extension("").join(name);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                write_lines(&path, lines);
                path
            })
            .collect();
        let cache = TemporaryCache::admitted();
        cache.admit_storage(storage_of(source)).unwrap();
        let mut session = Self {
            source,
            home,
            cache,
            own: own_path,
            subagents,
            updates: 1,
        };
        session.store();
        session
    }

    /// Writes the parse of the session's files as they are now.
    fn store(&mut self) {
        let fixture = self.fixture();
        let record = parsed_record(self.source, &fixture.root, &fixture.stub);
        self.cache.write(&record).unwrap();
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
            source: self.source,
            root: SessionRoot::new(self.home.path().join("sessions")).in_agent_tree(),
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

    /// Appends `lines` to the log at `path`.
    fn append(&self, path: &Path, lines: &[String]) {
        let mut text = std::fs::read_to_string(path).unwrap();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        std::fs::write(path, text).unwrap();
    }
}

fn header(id: &str) -> String {
    json!({"type": "session", "version": 3, "id": id, "timestamp": "2026-08-01T10:00:00.000Z", "cwd": "/tmp/project"})
        .to_string()
}

fn timestamp(seconds: u64) -> String {
    format!("2026-08-01T10:{:02}:{:02}.000Z", seconds / 60, seconds % 60)
}

fn prompt(id: &str, parent: Option<&str>, seconds: u64, text: &str) -> String {
    json!({
        "type": "message", "id": id, "parentId": parent, "timestamp": timestamp(seconds),
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
    })
    .to_string()
}

fn answer(id: &str, parent: &str, seconds: u64, text: &str) -> String {
    json!({
        "type": "message", "id": id, "parentId": parent, "timestamp": timestamp(seconds),
        "message": {
            "role": "assistant", "content": [{"type": "text", "text": text}],
            "model": "claude-opus", "usage": {"input": 100, "output": 20, "cacheRead": 0, "cacheWrite": 0},
        },
    })
    .to_string()
}

/// A log's header, then two prompts, each answered.
fn opening(id: &str) -> Vec<String> {
    vec![
        header(id),
        prompt("u1", None, 1, "Start the work"),
        answer("a1", "u1", 2, "Starting."),
        prompt("u2", Some("a1"), 10, "What next?"),
        answer("a2", "u2", 11, "The release."),
    ]
}

#[test]
fn entries_continuing_shown_branch_are_read_from_its_stable_cursor() {
    for source in [Source::Pi, Source::Omp] {
        let mut session = LogSession::stored(source, &opening("session"), &[]);
        let after_cursor_before = session.bytes_after_stable_cursor(&session.own.clone());
        let more = [
            prompt("u3", Some("a2"), 20, "Go on"),
            answer("a3", "u3", 21, "Going on."),
        ];
        session.append(&session.own, &more);

        let update = session.update();

        assert_eq!(update.outcome, Ok(()), "{source:?}: the session resumes");
        assert_eq!(
            update.transcripts_read, 1,
            "{source:?}: the read opens the changed log alone"
        );
        let appended = more.iter().map(|line| line.len() as u64 + 1).sum::<u64>();
        assert_eq!(
            update.bytes_read,
            after_cursor_before + appended,
            "{source:?}: the read reads from the stable cursor on"
        );
    }
}

#[test]
fn entry_returning_to_earlier_entry_reads_whole() {
    let mut session = LogSession::stored(Source::Pi, &opening("session"), &[]);
    session.append(
        &session.own,
        &[prompt("u3", Some("a1"), 20, "Another way instead")],
    );

    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnChangesEarlierEntry),
        "a branch from the first answer hides the second prompt and its answer"
    );
}

#[test]
fn pi_rename_appended_to_log_resumes() {
    let mut session = LogSession::stored(Source::Pi, &opening("session"), &[]);
    let rename = json!({"type": "session_info", "id": "n1", "parentId": "a2", "timestamp": timestamp(20), "name": "Release work"});
    session.append(&session.own, &[rename.to_string()]);

    assert_eq!(
        session.update().outcome,
        Ok(()),
        "a session_info record continues the branch"
    );
}

#[test]
fn turn_appended_to_omp_subagent_log_is_read_as_its_window() {
    let subagent = vec![
        header("subagent"),
        prompt("su1", None, 12, "Scout the notes"),
        answer("sa1", "su1", 13, "Reading the notes."),
    ];
    let mut session = LogSession::stored(
        Source::Omp,
        &opening("session"),
        &[("worker.jsonl", subagent)],
    );
    let worker = session.subagents[0].clone();
    session.append(
        &worker,
        &[answer("sa2", "sa1", 14, "The notes list the steps.")],
    );

    let update = session.update();

    assert_eq!(update.outcome, Ok(()), "the session resumes");
    assert_eq!(
        update.transcripts_read, 1,
        "the read opens the changed sub-agent log alone"
    );
}

#[test]
fn rewritten_or_shrunk_log_is_read_whole() {
    let mut session = LogSession::stored(Source::Pi, &opening("session"), &[]);
    let mut rewritten = opening("session");
    rewritten[2] = answer("a1", "u1", 2, "Starting at once.");
    write_lines(&session.own, &rewritten);
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnRewritten),
        "a rewritten log is read whole"
    );

    write_lines(&session.own, &opening("session")[..3]);
    assert_eq!(
        session.update().outcome,
        Err(WholeRead::OwnRewritten),
        "a shrunk log is read whole"
    );
}
