//! The load loop shared by every provider that stores sessions under roots.

use super::storage::SessionStub;
use super::{SessionRoot, SessionStorage};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::Result;
use crate::history::cache::{
    SessionCacheEntry, SessionCacheStore, conversation_from_entry, entry_from_conversation,
    entry_matches,
};
use crate::history::{Conversation, format_short_name_from_path};
use std::collections::HashMap;
use std::path::PathBuf;

/// Every session `storage` holds, newest first.
///
/// Each root carries its own cache, so a session that has not changed since the
/// last run is rebuilt from cached metadata instead of reparsed.
pub fn load_sessions(
    storage: &dyn SessionStorage,
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    load_sessions_with_cache(
        storage,
        &SessionCacheStore::in_user_cache(storage.cache()),
        show_last,
        debug_level,
    )
}

fn load_sessions_with_cache(
    storage: &dyn SessionStorage,
    cache: &SessionCacheStore,
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    let mut conversations = Vec::new();
    for root in storage.roots()? {
        conversations.extend(load_root(storage, cache, &root, show_last, debug_level)?);
    }
    conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.timestamp));
    for (index, conversation) in conversations.iter_mut().enumerate() {
        conversation.index = index;
    }
    Ok(conversations)
}

fn load_root(
    storage: &dyn SessionStorage,
    cache: &SessionCacheStore,
    root: &SessionRoot,
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    let cached = cache.read(&root.path);
    let mut refreshed_cache = HashMap::new();
    let mut conversations = Vec::new();

    for stub in storage.discover(root)? {
        let SessionStub {
            locator,
            cache_key,
            fingerprint,
        } = stub;
        if exceeds_size_limit(storage, fingerprint.size, &locator, debug_level) {
            continue;
        }

        let cached_entry = fingerprint.modified.and_then(|mtime| {
            cached
                .get(&cache_key)
                .filter(|entry| entry_matches(&entry.metadata, fingerprint.size, mtime))
        });
        let conversation = match cached_entry {
            Some(entry) => Some(restore_from_cache(
                storage,
                entry,
                locator.clone(),
                show_last,
            )),
            None => parse_session(storage, &locator, root, fingerprint.modified, debug_level),
        };
        let Some(mut conversation) = conversation else {
            continue;
        };

        conversation.preview = if show_last {
            conversation.preview_last.clone()
        } else {
            conversation.preview_first.clone()
        };
        let project_path = conversation
            .cwd
            .clone()
            .unwrap_or_else(|| PathBuf::from("unknown"));
        conversation.project_name = Some(format_short_name_from_path(&project_path));
        conversation.project_path = Some(project_path.clone());

        if let Some(mtime) = fingerprint.modified {
            refreshed_cache.insert(
                cache_key,
                SessionCacheEntry {
                    metadata: entry_from_conversation(&conversation, fingerprint.size, mtime),
                    session_id: conversation.session_id.clone(),
                    project_path,
                },
            );
        }
        conversations.push(conversation);
    }

    cache.write(&root.path, refreshed_cache);
    Ok(conversations)
}

fn exceeds_size_limit(
    storage: &dyn SessionStorage,
    size: u64,
    locator: &std::path::Path,
    debug_level: Option<DebugLevel>,
) -> bool {
    let Some(limit) = storage.max_session_bytes() else {
        return false;
    };
    if size <= limit {
        return false;
    }
    debug::warn(
        debug_level,
        &format!(
            "Skipping {}: {size} bytes exceeds the {limit} byte session limit",
            locator.display()
        ),
    );
    true
}

fn restore_from_cache(
    storage: &dyn SessionStorage,
    entry: &SessionCacheEntry,
    locator: PathBuf,
    show_last: bool,
) -> Conversation {
    let mut conversation = conversation_from_entry(&entry.metadata, locator, show_last);
    conversation.source = storage.source();
    conversation.session_id = entry.session_id.clone();
    conversation.cwd = Some(entry.project_path.clone());
    conversation.project_path = Some(entry.project_path.clone());
    conversation.project_name = Some(format_short_name_from_path(&entry.project_path));
    conversation
}

/// A session another provider owns is not an error: roots can overlap, and a
/// redirected session directory can hold a sibling agent's files.
fn parse_session(
    storage: &dyn SessionStorage,
    locator: &std::path::Path,
    root: &SessionRoot,
    modified: Option<std::time::SystemTime>,
    debug_level: Option<DebugLevel>,
) -> Option<Conversation> {
    match storage.parse_session(locator.to_path_buf(), root, modified, debug_level) {
        Ok(Some(conversation)) if conversation.source == storage.source() => Some(conversation),
        Ok(_) => None,
        Err(error) => {
            debug::warn(
                debug_level,
                &format!(
                    "Failed to parse {} session {}: {error}",
                    storage.source().list_label(),
                    locator.display()
                ),
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::provider::{Fingerprint, walk};
    use crate::history::{Source, cache};
    use std::sync::Mutex;
    use std::time::{Duration, UNIX_EPOCH};

    /// Records which transcripts the loop offered it, so a test can assert what
    /// the loop filtered before parsing was ever attempted.
    struct RecordingStorage {
        root: SessionRoot,
        max_session_bytes: Option<u64>,
        parsed: Mutex<Vec<PathBuf>>,
    }

    impl RecordingStorage {
        fn new(root: PathBuf, max_session_bytes: Option<u64>) -> Self {
            Self {
                root: SessionRoot::new(root),
                max_session_bytes,
                parsed: Mutex::new(Vec::new()),
            }
        }

        fn parsed_file_names(&self) -> Vec<String> {
            let mut names = self
                .parsed
                .lock()
                .unwrap()
                .iter()
                .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            names.sort();
            names
        }
    }

    impl SessionStorage for RecordingStorage {
        fn source(&self) -> Source {
            Source::Pi
        }

        fn cache(&self) -> super::super::SessionCache {
            super::super::SessionCache {
                directory: "recording-storage",
                magic: *b"RECORD01",
                schema_version: 1,
            }
        }

        fn roots(&self) -> Result<Vec<SessionRoot>> {
            Ok(vec![self.root.clone()])
        }

        fn discover(&self, root: &SessionRoot) -> Result<Vec<SessionStub>> {
            Ok(walk::file_stubs(
                root,
                walk::jsonl_files_at_depth(&root.path, 0)?,
            ))
        }

        fn parse_session(
            &self,
            locator: PathBuf,
            _root: &SessionRoot,
            _modified: Option<std::time::SystemTime>,
            _debug_level: Option<DebugLevel>,
        ) -> Result<Option<Conversation>> {
            self.parsed.lock().unwrap().push(locator);
            Ok(None)
        }

        fn max_session_bytes(&self) -> Option<u64> {
            self.max_session_bytes
        }
    }

    /// A storage whose locators name no file on disk. What it pins: the load
    /// loop consumes stubs as given — it never stats, opens, or interprets a
    /// locator — so a provider can back sessions with something other than
    /// files and still get listing and caching from the shared loop.
    struct VirtualStorage {
        root: SessionRoot,
        stubs: Vec<SessionStub>,
        parse_count: Mutex<usize>,
    }

    impl VirtualStorage {
        fn new(stubs: Vec<SessionStub>) -> Self {
            Self {
                root: SessionRoot::new("container.db"),
                stubs,
                parse_count: Mutex::new(0),
            }
        }

        fn parse_count(&self) -> usize {
            *self.parse_count.lock().unwrap()
        }
    }

    impl SessionStorage for VirtualStorage {
        fn source(&self) -> Source {
            Source::Pi
        }

        fn cache(&self) -> super::super::SessionCache {
            super::super::SessionCache {
                directory: "virtual-storage",
                magic: *b"VIRTUAL1",
                schema_version: 1,
            }
        }

        fn roots(&self) -> Result<Vec<SessionRoot>> {
            Ok(vec![self.root.clone()])
        }

        fn discover(&self, _root: &SessionRoot) -> Result<Vec<SessionStub>> {
            Ok(self.stubs.clone())
        }

        fn parse_session(
            &self,
            locator: PathBuf,
            _root: &SessionRoot,
            _modified: Option<std::time::SystemTime>,
            _debug_level: Option<DebugLevel>,
        ) -> Result<Option<Conversation>> {
            *self.parse_count.lock().unwrap() += 1;
            let mut conversation = cache::conversation_from_entry(
                &cache::empty_entry(0, UNIX_EPOCH),
                PathBuf::new(),
                false,
            );
            conversation.session_id = locator.to_string_lossy().into_owned();
            conversation.source = Source::Pi;
            conversation.path = locator;
            conversation.message_count = 1;
            Ok(Some(conversation))
        }

        fn max_session_bytes(&self) -> Option<u64> {
            None
        }
    }

    fn virtual_stub(session_id: &str, size: u64, modified_secs: u64) -> SessionStub {
        SessionStub {
            locator: PathBuf::from("container.db").join(format!("{session_id}.jsonl")),
            cache_key: session_id.to_owned(),
            fingerprint: Fingerprint {
                size,
                modified: Some(UNIX_EPOCH + Duration::from_secs(modified_secs)),
            },
        }
    }

    fn write_transcript(directory: &std::path::Path, name: &str, bytes: usize) {
        std::fs::write(directory.join(name), "x".repeat(bytes)).unwrap();
    }

    fn parsed_files_for_limit(max_session_bytes: Option<u64>) -> Vec<String> {
        let directory = tempfile::tempdir().unwrap();
        let cache_base = tempfile::tempdir().unwrap();
        write_transcript(directory.path(), "small.jsonl", 10);
        write_transcript(directory.path(), "huge.jsonl", 5_000);

        let storage = RecordingStorage::new(directory.path().to_path_buf(), max_session_bytes);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());
        load_sessions_with_cache(&storage, &cache, false, None).unwrap();

        storage.parsed_file_names()
    }

    #[test]
    fn transcripts_over_the_size_limit_are_never_parsed() {
        assert_eq!(
            parsed_files_for_limit(Some(1_000)),
            vec!["small.jsonl".to_string()]
        );
    }

    #[test]
    fn no_size_limit_offers_every_transcript() {
        assert_eq!(
            parsed_files_for_limit(None),
            vec!["huge.jsonl".to_string(), "small.jsonl".to_string()]
        );
    }

    #[test]
    fn sessions_that_are_not_files_load_and_cache_through_the_shared_loop() {
        let cache_base = tempfile::tempdir().unwrap();
        let storage = VirtualStorage::new(vec![
            virtual_stub("ses_first", 100, 1_000),
            virtual_stub("ses_second", 200, 2_000),
        ]);
        let cache = SessionCacheStore::under(cache_base.path(), storage.cache());

        let cold = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert_eq!(cold.len(), 2);
        assert_eq!(storage.parse_count(), 2);
        assert_eq!(
            cold.iter()
                .map(|conversation| conversation.path.clone())
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from("container.db").join("ses_first.jsonl"),
                PathBuf::from("container.db").join("ses_second.jsonl"),
            ],
            "locators reach the conversations untouched"
        );

        let warm = load_sessions_with_cache(&storage, &cache, false, None).unwrap();
        assert_eq!(warm.len(), 2);
        assert_eq!(
            storage.parse_count(),
            2,
            "unchanged fingerprints must restore from the cache, not reparse"
        );

        let mut changed = VirtualStorage::new(vec![
            virtual_stub("ses_first", 100, 1_000),
            virtual_stub("ses_second", 250, 3_000),
        ]);
        changed.parse_count = Mutex::new(storage.parse_count());
        let after_change = load_sessions_with_cache(&changed, &cache, false, None).unwrap();
        assert_eq!(after_change.len(), 2);
        assert_eq!(
            changed.parse_count(),
            3,
            "only the session whose fingerprint changed is reparsed"
        );
    }
}
