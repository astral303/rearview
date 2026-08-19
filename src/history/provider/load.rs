//! The load loop shared by every provider that stores sessions under roots.

use super::{SessionRoot, SessionStorage};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::Result;
use crate::history::cache::{
    self, SessionCacheEntry, conversation_from_entry, entry_from_conversation, entry_matches,
};
use crate::history::{Conversation, Source, format_short_name_from_path};
use std::collections::HashMap;
use std::path::PathBuf;

/// Every session `storage` holds, newest first.
///
/// Each root carries its own cache, so a session that has not changed since the
/// last run is rebuilt from cached metadata instead of reparsed.
pub fn load_sessions(
    source: Source,
    storage: &dyn SessionStorage,
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    let mut conversations = Vec::new();
    for root in storage.roots()? {
        conversations.extend(load_root(source, storage, &root, show_last, debug_level)?);
    }
    conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.timestamp));
    for (index, conversation) in conversations.iter_mut().enumerate() {
        conversation.index = index;
    }
    Ok(conversations)
}

fn load_root(
    source: Source,
    storage: &dyn SessionStorage,
    root: &SessionRoot,
    show_last: bool,
    debug_level: Option<DebugLevel>,
) -> Result<Vec<Conversation>> {
    let cached = cache::read_session_cache(source, &root.path).unwrap_or_default();
    let mut refreshed_cache = HashMap::new();
    let mut conversations = Vec::new();

    for path in root.discover_files()? {
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if exceeds_size_limit(storage, metadata.len(), &path, debug_level) {
            continue;
        }
        let modified = metadata.modified().ok();
        let cache_key = cache_key_for(root, &path);

        let cached_entry = modified.and_then(|mtime| {
            cached
                .get(&cache_key)
                .filter(|entry| entry_matches(&entry.metadata, metadata.len(), mtime))
        });
        let conversation = match cached_entry {
            Some(entry) => Some(restore_from_cache(source, entry, path.clone(), show_last)),
            None => parse_session(source, storage, &path, root, modified, debug_level),
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

        if let Some(mtime) = modified {
            refreshed_cache.insert(
                cache_key,
                SessionCacheEntry {
                    metadata: entry_from_conversation(&conversation, metadata.len(), mtime),
                    session_id: conversation.session_id.clone(),
                    project_path,
                },
            );
        }
        conversations.push(conversation);
    }

    cache::write_session_cache(source, &root.path, refreshed_cache);
    Ok(conversations)
}

/// Cache entries are keyed by location within the root, so a root that moves
/// misses cleanly rather than colliding with its old contents.
fn cache_key_for(root: &SessionRoot, path: &std::path::Path) -> String {
    path.strip_prefix(&root.path)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn exceeds_size_limit(
    storage: &dyn SessionStorage,
    file_size: u64,
    path: &std::path::Path,
    debug_level: Option<DebugLevel>,
) -> bool {
    let Some(limit) = storage.max_session_bytes() else {
        return false;
    };
    if file_size <= limit {
        return false;
    }
    debug::warn(
        debug_level,
        &format!(
            "Skipping {}: {file_size} bytes exceeds the {limit} byte session limit",
            path.display()
        ),
    );
    true
}

fn restore_from_cache(
    source: Source,
    entry: &SessionCacheEntry,
    path: PathBuf,
    show_last: bool,
) -> Conversation {
    let mut conversation = conversation_from_entry(&entry.metadata, path, show_last);
    conversation.source = source;
    conversation.session_id = entry.session_id.clone();
    conversation.cwd = Some(entry.project_path.clone());
    conversation.project_path = Some(entry.project_path.clone());
    conversation.project_name = Some(format_short_name_from_path(&entry.project_path));
    conversation
}

/// A transcript another provider owns is not an error: roots can overlap, and a
/// redirected session directory can hold a sibling agent's files.
fn parse_session(
    source: Source,
    storage: &dyn SessionStorage,
    path: &std::path::Path,
    root: &SessionRoot,
    modified: Option<std::time::SystemTime>,
    debug_level: Option<DebugLevel>,
) -> Option<Conversation> {
    match storage.parse_session(path.to_path_buf(), root, modified, debug_level) {
        Ok(Some(conversation)) if conversation.source == source => Some(conversation),
        Ok(_) => None,
        Err(error) => {
            debug::warn(
                debug_level,
                &format!(
                    "Failed to parse {} session {}: {error}",
                    source.list_label(),
                    path.display()
                ),
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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
                root: SessionRoot::flat(root),
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
        fn cache(&self) -> super::super::SessionCache {
            super::super::SessionCache {
                directory: "pi",
                magic: *b"PIHIST01",
                schema_version: 1,
            }
        }

        fn roots(&self) -> Result<Vec<SessionRoot>> {
            Ok(vec![self.root.clone()])
        }

        fn parse_session(
            &self,
            path: PathBuf,
            _root: &SessionRoot,
            _modified: Option<std::time::SystemTime>,
            _debug_level: Option<DebugLevel>,
        ) -> Result<Option<Conversation>> {
            self.parsed.lock().unwrap().push(path);
            Ok(None)
        }

        fn max_session_bytes(&self) -> Option<u64> {
            self.max_session_bytes
        }
    }

    fn write_transcript(directory: &std::path::Path, name: &str, bytes: usize) {
        std::fs::write(directory.join(name), "x".repeat(bytes)).unwrap();
    }

    /// Loading writes a cache keyed by the root, which for a temp root lands in
    /// the real cache tree. Remove it so a test run leaves nothing behind.
    fn discard_cache_for(root: &std::path::Path) {
        if let Some(path) = cache::session_cache_path(Source::Pi, root)
            && let Some(directory) = path.parent()
        {
            let _ = std::fs::remove_dir_all(directory);
        }
    }

    fn parsed_files_for_limit(max_session_bytes: Option<u64>) -> Vec<String> {
        let directory = tempfile::tempdir().unwrap();
        write_transcript(directory.path(), "small.jsonl", 10);
        write_transcript(directory.path(), "huge.jsonl", 5_000);

        let storage = RecordingStorage::new(directory.path().to_path_buf(), max_session_bytes);
        load_sessions(Source::Pi, &storage, false, None).unwrap();
        discard_cache_for(directory.path());

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
}
