//! Claude Code sessions written to a temporary configuration directory, for
//! app and load tests that read sessions again through the production entry
//! points: Claude's storage pinned to that directory with its parses watched,
//! a lister the test can make refuse a directory, and the test's own session
//! cache unless it gives one.

use super::{App, ToolDisplayMode, TuiSearchOptions};
use crate::clock::{Clock, TestClock};
use crate::config::KeyBindings;
use crate::history::environment::test_support::waiting_operation;
use crate::history::load_meter::AgentMeter;
use crate::history::provider::walk::test_support::ScriptedLister;
use crate::history::provider::{
    LeftOutRows, ParseWatch, SessionRoot, SessionStorage, WatchedStorage, load_sessions,
};
use crate::history::sqlite_cache::SqliteCache;
use crate::history::{CacheBase, Conversation, Environment, ProcessIdentity, Source};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The project directory [`ClaudeHome::write`] writes to.
const PROJECT: &str = "-tmp-project";

pub(crate) struct ClaudeHome {
    directory: tempfile::TempDir,
    lister: Arc<ScriptedLister>,
    storage: &'static dyn SessionStorage,
    watch: ParseWatch,
}

impl ClaudeHome {
    pub(crate) fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let watch = ParseWatch::default();
        let storage: &'static dyn SessionStorage = Box::leak(Box::new(WatchedStorage {
            inner: Source::Claude.provider().storage(),
            roots: vec![SessionRoot::new(directory.path().join("projects"))],
            watch: watch.clone(),
        }));
        Self {
            directory,
            lister: Arc::new(ScriptedLister::default()),
            storage,
            watch,
        }
    }

    /// Claude's storage pinned to this home's one root.
    pub(crate) fn storage(&self) -> &'static dyn SessionStorage {
        self.storage
    }

    /// The root holding this home's project directories.
    pub(crate) fn root(&self) -> SessionRoot {
        SessionRoot::new(self.directory.path().join("projects"))
    }

    /// The parses of this home's sessions.
    pub(crate) fn watch(&self) -> &ParseWatch {
        &self.watch
    }

    pub(crate) fn project(&self) -> PathBuf {
        self.project_named(PROJECT)
    }

    fn project_named(&self, project: &str) -> PathBuf {
        self.directory.path().join("projects").join(project)
    }

    /// The transcript of session `name`.
    pub(crate) fn transcript(&self, name: &str) -> PathBuf {
        self.project().join(format!("{name}.jsonl"))
    }

    /// Write session `name`: one user message per text, sent `minute` past
    /// noon a second apart. Returns its transcript.
    pub(crate) fn write(&self, name: &str, minute: u32, texts: &[&str]) -> PathBuf {
        self.write_in(PROJECT, name, minute, texts)
    }

    /// [`Self::write`] into the project directory `project`.
    pub(crate) fn write_in(
        &self,
        project: &str,
        name: &str,
        minute: u32,
        texts: &[&str],
    ) -> PathBuf {
        let directory = self.project_named(project);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{name}.jsonl"));
        std::fs::write(&path, transcript_lines(minute, texts)).unwrap();
        path
    }

    /// Write the transcript of sub-agent `agent` of the session at
    /// `transcript`, in that session's `subagents/` directory: one user
    /// message per text, as [`Self::write`] writes them. Returns its path.
    pub(crate) fn write_subagent(
        &self,
        transcript: &Path,
        agent: &str,
        minute: u32,
        texts: &[&str],
    ) -> PathBuf {
        let directory = transcript.with_extension("").join("subagents");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("agent-{agent}.jsonl"));
        std::fs::write(&path, transcript_lines(minute, texts)).unwrap();
        path
    }

    /// Add a user message with `text`, sent `minute` past noon, to the
    /// transcript at `path`.
    pub(crate) fn append(&self, path: &Path, minute: u32, text: &str) {
        let mut transcript = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        let line = user_line(minute, 0, text) + "\n";
        std::io::Write::write_all(&mut transcript, line.as_bytes()).unwrap();
    }

    /// Add a model reply with `text` that used `tokens` tokens, sent
    /// `minute` past noon, to the transcript at `path`.
    pub(crate) fn append_reply(&self, path: &Path, minute: u32, text: &str, tokens: u64) {
        let mut transcript = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        let line = serde_json::json!({
            "type": "assistant",
            "timestamp": format!("2026-10-01T12:{minute:02}:59Z"),
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": text}],
                "usage": {"input_tokens": tokens, "output_tokens": 0},
            }
        })
        .to_string()
            + "\n";
        std::io::Write::write_all(&mut transcript, line.as_bytes()).unwrap();
    }

    /// Refuse every later listing of the project directory, as a directory
    /// the user cannot read refuses.
    pub(crate) fn refuse_project(&self) {
        self.refuse(&self.project());
    }

    /// Refuse every later listing of `directory`.
    pub(crate) fn refuse(&self, directory: &Path) {
        self.lister.refuse(directory);
    }

    /// Refuse the `listing`th listing of `directory` and every one after it.
    pub(crate) fn refuse_from_listing(&self, directory: &Path, listing: usize) {
        self.lister.refuse_from_listing(directory, listing);
    }

    /// Run `action` before the `listing`th listing of `directory`, as
    /// between one discovery and the next.
    pub(crate) fn run_before_listing(
        &self,
        directory: &Path,
        listing: usize,
        action: impl Fn() + Send + Sync + 'static,
    ) {
        self.lister.run_before_listing(directory, listing, action);
    }

    /// How many times discovery listed the project directory.
    pub(crate) fn times_project_listed(&self) -> usize {
        self.lister.times_listed(&self.project())
    }

    /// How many times discovery listed `directory`.
    pub(crate) fn times_listed(&self, directory: &Path) -> usize {
        self.lister.times_listed(directory)
    }

    /// Claude's storage pinned here, listed through this home's lister, on
    /// test time, with this test's own session cache.
    pub(crate) fn environment(&self) -> Environment {
        self.environment_on(Arc::new(TestClock::default()))
    }

    /// [`Self::environment`] on `clock`.
    pub(crate) fn environment_on(&self, clock: Arc<dyn Clock>) -> Environment {
        Environment::new(
            vec![self.storage],
            crate::cache_file::test_support::test_cache_directory(),
            ProcessIdentity::default(),
            clock,
            self.lister.clone(),
        )
    }

    /// [`Self::environment`] with the session cache under `cache_base`, as
    /// the rearview `process` reaches it, on `clock`.
    pub(crate) fn environment_with_cache(
        &self,
        cache_base: &Path,
        process: ProcessIdentity,
        clock: Arc<dyn Clock>,
    ) -> Environment {
        Environment::new(
            vec![self.storage],
            cache_base.to_path_buf(),
            process,
            clock,
            self.lister.clone(),
        )
    }

    /// Every session here, newest first, as a load through `environment`
    /// lists them.
    pub(crate) fn rows(&self, environment: &Environment) -> Vec<Conversation> {
        let operation = waiting_operation(environment);
        let rows = load_sessions(
            environment,
            self.storage,
            false,
            None,
            AgentMeter::UNMETERED,
            LeftOutRows::NONE,
            &operation,
            &mut |_, _| {},
        )
        .unwrap()
        .conversations;
        operation.finish().unwrap();
        rows
    }

    /// An app listing every session here, loaded through this test's own
    /// session cache on test time.
    pub(crate) fn app(&self) -> App {
        let environment = self.environment();
        app_reading(self.rows(&environment), environment)
    }
}

/// Claude Code sessions in two homes, five in the first and two in the
/// second, cached by a load and each changed since. Set up once for a
/// sweep: each case reads the same transcripts over a copy of that cache.
pub(crate) struct ChangedSessionsTemplate {
    homes: Arc<[ClaudeHome; 2]>,
    transcripts: Vec<PathBuf>,
    cache: TemplateCache,
}

impl ChangedSessionsTemplate {
    pub(crate) fn new() -> Self {
        let homes = Arc::new([ClaudeHome::new(), ClaudeHome::new()]);
        let mut transcripts = Vec::new();
        for (root, (home, sessions)) in homes.iter().zip([5, 2]).enumerate() {
            for index in 0..sessions {
                let id = format!("0f000000-0000-4000-8000-0000000000{root}{index}");
                let minute = (root * 10 + index) as u32;
                transcripts.push((root, home.write(&id, minute, &["a question"])));
            }
        }
        let paths: Vec<PathBuf> = transcripts.iter().map(|(_, path)| path.clone()).collect();
        let sessions = ChangedSessionsInTwoRoots::over(Arc::clone(&homes), paths, admitted_cache());
        let environment = sessions.environment();
        let operation = waiting_operation(&environment);
        sessions.load(&environment, &operation);
        operation.finish().unwrap();
        for (root, transcript) in transcripts {
            homes[root].append(&transcript, 30, "the newest question");
        }
        Self {
            homes,
            transcripts: sessions.transcripts,
            cache: TemplateCache::new(sessions.cache),
        }
    }

    /// The sessions over a copy of the cache, read through a storage whose
    /// parses a new `watch` sees.
    pub(crate) fn case(&self) -> ChangedSessionsInTwoRoots {
        ChangedSessionsInTwoRoots::over(
            Arc::clone(&self.homes),
            self.transcripts.clone(),
            self.cache.copy(),
        )
    }
}

/// The sessions of a [`ChangedSessionsTemplate`] over a session cache of
/// their own, read as two roots of one storage whose parses `watch` sees.
pub(crate) struct ChangedSessionsInTwoRoots {
    _homes: Arc<[ClaudeHome; 2]>,
    pub(crate) transcripts: Vec<PathBuf>,
    pub(crate) cache: tempfile::TempDir,
    pub(crate) watch: ParseWatch,
    storage: &'static dyn SessionStorage,
}

impl ChangedSessionsInTwoRoots {
    /// The sessions of a template set up for this test alone.
    pub(crate) fn new() -> Self {
        ChangedSessionsTemplate::new().case()
    }

    fn over(
        homes: Arc<[ClaudeHome; 2]>,
        transcripts: Vec<PathBuf>,
        cache: tempfile::TempDir,
    ) -> Self {
        let watch = ParseWatch::default();
        let storage: &'static dyn SessionStorage = Box::leak(Box::new(WatchedStorage {
            inner: Source::Claude.provider().storage(),
            roots: homes
                .iter()
                .map(|home| SessionRoot::new(home.project().parent().unwrap()))
                .collect(),
            watch: watch.clone(),
        }));
        Self {
            _homes: homes,
            transcripts,
            cache,
            watch,
            storage,
        }
    }

    /// Every session here, newest first, as a full load under `operation`
    /// through `environment` lists them.
    pub(crate) fn load(
        &self,
        environment: &Environment,
        operation: &crate::history::OperationScope,
    ) -> Vec<Conversation> {
        load_sessions(
            environment,
            self.storage,
            false,
            None,
            AgentMeter::UNMETERED,
            LeftOutRows::NONE,
            operation,
            &mut |_, _| {},
        )
        .unwrap()
        .conversations
    }

    /// The storage reading both roots, on test time, with the session cache
    /// these sessions were cached in.
    pub(crate) fn environment(&self) -> Environment {
        Environment::new(
            vec![self.storage],
            self.cache.path().to_path_buf(),
            ProcessIdentity::default(),
            Arc::new(TestClock::default()),
            Arc::new(crate::history::provider::walk::SystemLister),
        )
    }
}

/// An app listing `rows`, reading sessions through `environment`.
pub(crate) fn app_reading(rows: Vec<Conversation>, environment: Environment) -> App {
    App::new_with_options(
        rows,
        ToolDisplayMode::Hidden,
        false,
        KeyBindings::default(),
        Vec::new(),
        TuiSearchOptions::default(),
        environment,
    )
}

/// The session cache under `cache_base` as another rearview reaches it, on
/// `clock`: its refresh ownership reads as another process's.
pub(crate) fn another_rearview(cache_base: &Path, clock: Arc<dyn Clock>) -> CacheBase {
    CacheBase::new(cache_base, ProcessIdentity::default(), clock)
}

/// A session cache directory holding an admitted database without sessions,
/// as a first build admits it: a load through it caches what it reads.
pub(crate) fn admitted_cache() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    SqliteCache::open(&another_rearview(
        directory.path(),
        Arc::new(TestClock::default()),
    ))
    .unwrap()
    .mark_admitted()
    .unwrap();
    directory
}

/// A session cache directory a sweep sets up once. Each case starts from a
/// copy, so the setup runs once per sweep, not once per case.
pub(crate) struct TemplateCache(tempfile::TempDir);

impl TemplateCache {
    /// `cache` as a template. Drop every connection to it first, so no
    /// write or checkpoint changes its files while a case copies them.
    pub(crate) fn new(cache: tempfile::TempDir) -> Self {
        Self(cache)
    }

    /// A new session cache directory holding a copy of every file here.
    pub(crate) fn copy(&self) -> tempfile::TempDir {
        let copy = tempfile::tempdir().unwrap();
        copy_directory(self.0.path(), copy.path());
        copy
    }
}

fn copy_directory(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir(&target).unwrap();
            copy_directory(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// One user message per text, sent `minute` past noon a second apart.
fn transcript_lines(minute: u32, texts: &[&str]) -> String {
    texts
        .iter()
        .enumerate()
        .map(|(second, text)| user_line(minute, second, text) + "\n")
        .collect()
}

fn user_line(minute: u32, second: usize, text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "timestamp": format!("2026-10-01T12:{minute:02}:{second:02}Z"),
        "message": {"role": "user", "content": text}
    })
    .to_string()
}
