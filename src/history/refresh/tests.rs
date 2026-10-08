use super::*;
use crate::history::provider::Fingerprint;
use crate::search::test_fixtures::one_message_conversation;
use chrono::{Local, TimeZone};
use std::cell::RefCell;
use std::time::{Duration, UNIX_EPOCH};

/// The outcome of reading a session on the fake disk.
#[derive(Clone, Copy)]
enum Content {
    /// A conversation active on the given day of October 2026.
    Conversation {
        day: u32,
    },
    Empty,
    Unreadable,
}

/// The sessions each agent's discovery finds, and every read the refresh
/// makes.
#[derive(Default)]
struct Disk {
    sessions: Vec<(Source, SessionStub, Content)>,
    failing: Vec<Source>,
    reads: RefCell<Vec<(Source, PathBuf)>>,
}

impl Disk {
    fn with(self, source: Source, name: &str, size: u64, content: Content) -> Self {
        self.with_locator(source, locator(name), size, content)
    }

    fn with_locator(
        mut self,
        source: Source,
        locator: PathBuf,
        size: u64,
        content: Content,
    ) -> Self {
        self.sessions
            .push((source, stub_at(locator, size), content));
        self
    }

    fn failing(mut self, source: Source) -> Self {
        self.failing.push(source);
        self
    }

    fn rediscover(
        &self,
        source: Source,
        needs_reading: &dyn Fn(&SessionStub) -> bool,
    ) -> Result<RediscoveredSessions> {
        if self.failing.contains(&source) {
            return Err(AppError::SessionListUnreadable {
                reason: "session database locked",
                detail: "locked".to_owned(),
            });
        }
        let mut rediscovered = RediscoveredSessions::default();
        for (_, stub, content) in self.sessions.iter().filter(|(of, ..)| *of == source) {
            rediscovered
                .found
                .push((stub.locator.clone(), stub.fingerprint.stamp()));
            if !needs_reading(stub) {
                continue;
            }
            self.reads.borrow_mut().push((source, stub.locator.clone()));
            let read = match *content {
                Content::Conversation { day } => {
                    SessionRead::Listed(Box::new(row(source, stub, day)))
                }
                Content::Empty => SessionRead::Empty,
                Content::Unreadable => SessionRead::Unreadable,
            };
            rediscovered.read.push((stub.locator.clone(), read));
        }
        Ok(rediscovered)
    }

    fn refresh(&self, known: &KnownSessions) -> Result<SessionChanges> {
        self.refresh_within(known, TimeFilter::default())
    }

    fn refresh_within(&self, known: &KnownSessions, time: TimeFilter) -> Result<SessionChanges> {
        refresh_sources(
            known,
            &[Source::Claude, Source::Pi],
            RefreshOptions {
                time,
                ..Default::default()
            },
            &|source, needs_reading| self.rediscover(source, needs_reading),
        )
    }

    fn read_names(&self) -> Vec<(Source, String)> {
        self.reads
            .borrow()
            .iter()
            .map(|(source, path)| (*source, name_of(path)))
            .collect()
    }
}

fn locator(name: &str) -> PathBuf {
    PathBuf::from(format!("/sessions/{name}.jsonl"))
}

fn name_of(path: &Path) -> String {
    path.file_stem().unwrap().to_string_lossy().into_owned()
}

/// A session whose transcripts are `size` bytes, last written `size`
/// seconds after the epoch.
fn stub(name: &str, size: u64) -> SessionStub {
    stub_at(locator(name), size)
}

fn stub_at(locator: PathBuf, size: u64) -> SessionStub {
    SessionStub {
        cache_key: locator.to_string_lossy().into_owned(),
        locator,
        subagents: Vec::new(),
        fingerprint: Fingerprint {
            size,
            modified: Some(UNIX_EPOCH + Duration::from_secs(size)),
        },
    }
}

fn found(source: Source, size: u64) -> FoundSession {
    FoundSession {
        source,
        fingerprint: stub("any", size).fingerprint.stamp(),
    }
}

fn row(source: Source, stub: &SessionStub, day: u32) -> Conversation {
    let mut row = one_message_conversation(
        "text",
        Local.with_ymd_and_hms(2026, 10, day, 12, 0, 0).unwrap(),
        None,
        None,
        None,
    );
    row.source = source;
    row.path = stub.locator.clone();
    row.fingerprint = stub.fingerprint.stamp();
    row
}

fn known(listed: &[(&str, FoundSession)], unlisted: &[(&str, FoundSession)]) -> KnownSessions {
    let by_locator = |sessions: &[(&str, FoundSession)]| {
        sessions
            .iter()
            .map(|(name, found)| (locator(name), *found))
            .collect()
    };
    KnownSessions {
        listed: by_locator(listed),
        unlisted: by_locator(unlisted),
    }
}

/// Each updated row's session name, and whether it replaces a listed row.
fn updated_names(changes: &SessionChanges) -> Vec<(String, bool)> {
    changes
        .updated
        .iter()
        .map(|updated| (name_of(&updated.row.path), updated.replaces_listed_row))
        .collect()
}

fn removed_names(changes: &SessionChanges) -> Vec<String> {
    let mut names = changes
        .removed
        .iter()
        .map(|path| name_of(path))
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn unlisted_names(changes: &SessionChanges) -> Vec<String> {
    let mut names = changes
        .unlisted
        .keys()
        .map(|path| name_of(path))
        .collect::<Vec<_>>();
    names.sort();
    names
}

const ROW: Content = Content::Conversation { day: 5 };

#[test]
fn only_new_and_changed_sessions_are_read() {
    let disk = Disk::default()
        .with(Source::Claude, "same", 1, ROW)
        .with(Source::Claude, "grown", 2, ROW)
        .with(Source::Claude, "new", 1, ROW);
    let known = known(
        &[
            ("same", found(Source::Claude, 1)),
            ("grown", found(Source::Claude, 1)),
        ],
        &[],
    );

    let changes = disk.refresh(&known).unwrap();

    assert_eq!(
        disk.read_names(),
        [
            (Source::Claude, "grown".to_owned()),
            (Source::Claude, "new".to_owned())
        ]
    );
    assert_eq!(
        updated_names(&changes),
        [("grown".to_owned(), true), ("new".to_owned(), false)]
    );
    assert!(changes.removed.is_empty());
}

#[test]
fn a_listed_session_gone_from_disk_is_removed() {
    let disk = Disk::default().with(Source::Claude, "kept", 1, ROW);
    let known = known(
        &[
            ("kept", found(Source::Claude, 1)),
            ("gone", found(Source::Claude, 1)),
        ],
        &[],
    );

    let changes = disk.refresh(&known).unwrap();

    assert_eq!(removed_names(&changes), ["gone"]);
    assert!(disk.read_names().is_empty());
}

#[test]
fn a_listed_session_that_now_holds_no_conversation_is_removed_and_unlisted() {
    let disk = Disk::default().with(Source::Claude, "emptied", 2, Content::Empty);
    let known = known(&[("emptied", found(Source::Claude, 1))], &[]);

    let changes = disk.refresh(&known).unwrap();

    assert_eq!(removed_names(&changes), ["emptied"]);
    assert_eq!(
        changes.unlisted.get(&locator("emptied")),
        Some(&found(Source::Claude, 2))
    );
}

#[test]
fn a_session_holding_no_conversation_isnt_read_until_its_fingerprint_changes() {
    let disk = Disk::default()
        .with(Source::Claude, "empty", 1, Content::Empty)
        .with(Source::Claude, "filled", 2, ROW);
    let known = known(
        &[],
        &[
            ("empty", found(Source::Claude, 1)),
            ("filled", found(Source::Claude, 1)),
        ],
    );

    let changes = disk.refresh(&known).unwrap();

    assert_eq!(disk.read_names(), [(Source::Claude, "filled".to_owned())]);
    assert_eq!(unlisted_names(&changes), ["empty"]);
    assert_eq!(updated_names(&changes), [("filled".to_owned(), false)]);
}

#[test]
fn a_new_session_outside_the_time_filter_is_unlisted_and_a_listed_one_stays_listed() {
    let disk = Disk::default()
        .with(
            Source::Claude,
            "old_new",
            1,
            Content::Conversation { day: 1 },
        )
        .with(
            Source::Claude,
            "listed",
            2,
            Content::Conversation { day: 1 },
        );
    let known = known(&[("listed", found(Source::Claude, 1))], &[]);
    let since_october_3 = TimeFilter {
        after: Some(Local.with_ymd_and_hms(2026, 10, 3, 0, 0, 0).unwrap()),
        before: None,
    };

    let changes = disk.refresh_within(&known, since_october_3).unwrap();

    assert_eq!(updated_names(&changes), [("listed".to_owned(), true)]);
    assert_eq!(unlisted_names(&changes), ["old_new"]);
}

#[test]
fn a_session_another_agent_lists_is_left_to_it() {
    let disk =
        Disk::default()
            .with(Source::Claude, "shared", 2, ROW)
            .with(Source::Pi, "shared", 2, ROW);
    let known = known(&[("shared", found(Source::Claude, 1))], &[]);

    let changes = disk.refresh(&known).unwrap();

    assert_eq!(disk.read_names(), [(Source::Claude, "shared".to_owned())]);
    assert_eq!(updated_names(&changes).len(), 1);
    assert!(changes.removed.is_empty());
}

#[test]
fn the_first_agent_to_find_a_new_session_lists_it() {
    let disk =
        Disk::default()
            .with(Source::Claude, "both", 1, ROW)
            .with(Source::Pi, "both", 1, ROW);

    let changes = disk.refresh(&KnownSessions::default()).unwrap();

    assert_eq!(disk.read_names(), [(Source::Claude, "both".to_owned())]);
    assert_eq!(changes.updated.len(), 1);
    assert_eq!(changes.updated[0].row.source, Source::Claude);
}

#[test]
fn a_new_file_two_agents_reach_under_two_spellings_lists_once_under_the_first() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("sub")).unwrap();
    let file = directory.path().join("shared.jsonl");
    std::fs::write(&file, "{}").unwrap();
    let respelled = directory.path().join("sub").join("..").join("shared.jsonl");
    let disk = Disk::default()
        .with_locator(Source::Claude, file.clone(), 1, ROW)
        .with_locator(Source::Pi, respelled.clone(), 1, ROW);

    let changes = disk.refresh(&KnownSessions::default()).unwrap();

    assert_eq!(
        changes
            .updated
            .iter()
            .map(|updated| (updated.row.source, &updated.row.path))
            .collect::<Vec<_>>(),
        [(Source::Claude, &file)]
    );
    assert_eq!(
        changes.unlisted.get(&respelled).map(|found| found.source),
        Some(Source::Pi)
    );
}

#[test]
fn an_agent_whose_discovery_fails_keeps_what_the_list_holds_of_it() {
    let disk = Disk::default()
        .with(Source::Claude, "claude", 1, ROW)
        .failing(Source::Pi);
    let known = known(
        &[
            ("claude", found(Source::Claude, 1)),
            ("pi", found(Source::Pi, 1)),
        ],
        &[("pi_empty", found(Source::Pi, 1))],
    );

    let changes = disk.refresh(&known).unwrap();

    assert!(changes.removed.is_empty());
    assert_eq!(unlisted_names(&changes), ["pi_empty"]);
    assert_eq!(
        changes.ignored,
        [FilterTerm::new(
            "Pi",
            "session database locked: sessions not loaded"
        )]
    );
}

#[test]
fn the_refresh_fails_when_every_agent_does() {
    let disk = Disk::default().failing(Source::Claude).failing(Source::Pi);

    let outcome = disk.refresh(&KnownSessions::default());

    assert!(matches!(
        outcome,
        Err(AppError::SessionListUnreadable { .. })
    ));
}

#[test]
fn an_unreadable_changed_session_keeps_its_row_and_is_read_again_next_time() {
    let disk = Disk::default().with(Source::Claude, "locked", 2, Content::Unreadable);
    let known = known(&[("locked", found(Source::Claude, 1))], &[]);

    let changes = disk.refresh(&known).unwrap();

    assert!(changes.updated.is_empty());
    assert!(changes.removed.is_empty());
    assert!(changes.unlisted.is_empty());
}
