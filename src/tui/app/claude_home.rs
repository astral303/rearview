//! Claude Code sessions written to a temporary configuration directory, for
//! app tests that read sessions again through the production entry points:
//! Claude's storage pinned to that directory, a lister the test can make
//! refuse a directory, and the test's own session cache unless it gives one.

use super::{App, ToolDisplayMode, TuiSearchOptions};
use crate::clock::{Clock, TestClock};
use crate::config::KeyBindings;
use crate::history::environment::test_support::waiting_operation;
use crate::history::load_meter::AgentMeter;
use crate::history::provider::walk::test_support::ScriptedLister;
use crate::history::provider::{
    LeftOutRows, RootedStorage, SessionRoot, SessionStorage, load_sessions,
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
}

impl ClaudeHome {
    pub(crate) fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let storage: &'static dyn SessionStorage = Box::leak(Box::new(RootedStorage {
            inner: Source::Claude.provider().storage(),
            root: SessionRoot::new(directory.path().join("projects")),
        }));
        Self {
            directory,
            lister: Arc::new(ScriptedLister::default()),
            storage,
        }
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
        let lines: String = texts
            .iter()
            .enumerate()
            .map(|(second, text)| user_line(minute, second, text) + "\n")
            .collect();
        std::fs::write(&path, lines).unwrap();
        path
    }

    /// Add a user message with `text`, sent `minute` past noon, to the
    /// transcript at `path`.
    pub(crate) fn append(&self, path: &Path, minute: u32, text: &str) {
        let mut transcript = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        let line = user_line(minute, 0, text) + "\n";
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

    /// How many times discovery listed the project directory.
    pub(crate) fn times_project_listed(&self) -> usize {
        self.lister.times_listed(&self.project())
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

fn user_line(minute: u32, second: usize, text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "timestamp": format!("2026-10-01T12:{minute:02}:{second:02}Z"),
        "message": {"role": "user", "content": text}
    })
    .to_string()
}
