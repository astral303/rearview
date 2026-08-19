//! The load loop shared by every provider that stores sessions under roots.

use super::{SessionRoot, SessionStorage};
use crate::cli::DebugLevel;
use crate::debug;
use crate::error::Result;
use crate::history::cache::{
    SessionCacheEntry, SessionCacheStore, conversation_from_entry, entry_from_conversation,
    entry_matches,
};
use crate::history::{Conversation, format_short_name_from_path};
use std::collections::{HashMap, HashSet};
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
    conversations = fold_subagent_sessions(conversations);
    conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.timestamp));
    for (index, conversation) in conversations.iter_mut().enumerate() {
        conversation.index = index;
    }
    Ok(conversations)
}

/// Fold every sub-agent thread into the session it was spawned from, and drop it
/// from the list.
///
/// Agents that spawn sub-agents into their own transcript files would otherwise
/// contribute one top-level row per thread, burying the sessions a user actually
/// started. Claude reaches the same result by writing sub-agent turns inside the
/// parent transcript, where they stay out of the list and out of `full_text` but
/// remain searchable through the agent CLI. Folding reproduces that: the child's
/// searchable text, message count and tokens join the row it folds into.
///
/// A thread folds into the far end of its chain of parents, so a sub-agent that
/// spawned a sub-agent still lands on the session the user started. Every
/// session whose chain cannot be followed that far — an absent parent, or a
/// chain that loops back on itself — keeps its own row. Nothing is dropped
/// without being merged somewhere: a session no row lists is a session the user
/// cannot open.
///
/// Overlapping roots can surface one session id on more than one row. Both rows
/// stay, and threads fold into the first of them.
fn fold_subagent_sessions(conversations: Vec<Conversation>) -> Vec<Conversation> {
    if conversations
        .iter()
        .all(|conversation| conversation.parent_session_id.is_none())
    {
        return conversations;
    }

    let targets = fold_targets(&conversations);
    let mut rows = conversations.into_iter().map(Some).collect::<Vec<_>>();
    for (index, target) in targets.into_iter().enumerate() {
        let Some(target) = target else {
            continue;
        };
        let thread = rows[index].take().expect("a row is folded at most once");
        let session = rows[target]
            .as_mut()
            .expect("a fold target is a session that keeps its own row");
        merge_subagent_thread(session, thread);
    }
    rows.into_iter().flatten().collect()
}

/// For each conversation, the row it folds into, or `None` when it keeps its own.
fn fold_targets(conversations: &[Conversation]) -> Vec<Option<usize>> {
    let mut row_of = HashMap::new();
    for (index, conversation) in conversations.iter().enumerate() {
        row_of
            .entry(conversation.session_id.as_str())
            .or_insert(index);
    }

    let mut parent_of: HashMap<&str, &str> = HashMap::new();
    for conversation in conversations {
        let Some(parent) = conversation.parent_session_id.as_deref() else {
            continue;
        };
        if row_of.contains_key(parent) {
            parent_of
                .entry(conversation.session_id.as_str())
                .or_insert(parent);
        }
    }

    conversations
        .iter()
        .map(|conversation| {
            let session_id = conversation.session_id.as_str();
            let root = root_ancestor(session_id, &parent_of);
            (root != session_id).then(|| row_of[root])
        })
        .collect()
}

/// The session at the far end of `session_id`'s chain of parents, or `session_id`
/// itself when the chain ends there or loops back on it.
fn root_ancestor<'a>(session_id: &'a str, parent_of: &HashMap<&'a str, &'a str>) -> &'a str {
    let mut walked = HashSet::new();
    let mut current = session_id;
    while let Some(parent) = parent_of.get(current) {
        if !walked.insert(current) {
            return session_id;
        }
        current = parent;
    }
    current
}

/// The thread is a whole conversation of its own, so its dialogue lives in
/// `full_text` — from the session's point of view all of it is sub-agent
/// content, which is why it lands in `agent_search_text` and not in the
/// session's own index.
fn merge_subagent_thread(session: &mut Conversation, thread: Conversation) {
    for text in [thread.full_text, thread.agent_search_text] {
        if text.is_empty() {
            continue;
        }
        if !session.agent_search_text.is_empty() {
            session.agent_search_text.push('\n');
        }
        session.agent_search_text.push_str(&text);
    }
    session.message_count += thread.message_count;
    session.total_tokens += thread.total_tokens;
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
            Some(entry) => Some(restore_from_cache(storage, entry, path.clone(), show_last)),
            None => parse_session(storage, &path, root, modified, debug_level),
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
            // One entry per transcript, holding the session as parsed. Folding
            // runs after every root is loaded and is redone on each load, so a
            // folded message count must never reach the cache — it would be
            // added to again on the next run.
            refreshed_cache.insert(
                cache_key,
                SessionCacheEntry {
                    metadata: entry_from_conversation(&conversation, metadata.len(), mtime),
                    session_id: conversation.session_id.clone(),
                    parent_session_id: conversation.parent_session_id.clone(),
                    project_path,
                },
            );
        }
        conversations.push(conversation);
    }

    cache.write(&root.path, refreshed_cache);
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
    storage: &dyn SessionStorage,
    entry: &SessionCacheEntry,
    path: PathBuf,
    show_last: bool,
) -> Conversation {
    let mut conversation = conversation_from_entry(&entry.metadata, path, show_last);
    conversation.source = storage.source();
    conversation.session_id = entry.session_id.clone();
    conversation.parent_session_id = entry.parent_session_id.clone();
    conversation.cwd = Some(entry.project_path.clone());
    conversation.project_path = Some(entry.project_path.clone());
    conversation.project_name = Some(format_short_name_from_path(&entry.project_path));
    conversation
}

/// A transcript another provider owns is not an error: roots can overlap, and a
/// redirected session directory can hold a sibling agent's files.
fn parse_session(
    storage: &dyn SessionStorage,
    path: &std::path::Path,
    root: &SessionRoot,
    modified: Option<std::time::SystemTime>,
    debug_level: Option<DebugLevel>,
) -> Option<Conversation> {
    match storage.parse_session(path.to_path_buf(), root, modified, debug_level) {
        Ok(Some(conversation)) if conversation.source == storage.source() => Some(conversation),
        Ok(_) => None,
        Err(error) => {
            debug::warn(
                debug_level,
                &format!(
                    "Failed to parse {} session {}: {error}",
                    storage.source().list_label(),
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
    use crate::history::{Source, cache};
    use std::sync::Mutex;
    use std::time::UNIX_EPOCH;

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

    fn session(id: &str, parent: Option<&str>, agent_text: &str) -> Conversation {
        let mut conversation = cache::conversation_from_entry(
            &cache::empty_entry(0, UNIX_EPOCH),
            PathBuf::new(),
            false,
        );
        conversation.session_id = id.to_owned();
        conversation.parent_session_id = parent.map(str::to_owned);
        conversation.agent_search_text = agent_text.to_owned();
        conversation.message_count = 1;
        conversation.total_tokens = 10;
        conversation
    }

    fn session_ids(conversations: &[Conversation]) -> Vec<&str> {
        conversations
            .iter()
            .map(|conversation| conversation.session_id.as_str())
            .collect()
    }

    #[test]
    fn a_sub_agent_session_folds_into_its_parent() {
        let folded = fold_subagent_sessions(vec![
            session("parent", None, "parent text"),
            session("child", Some("parent"), "child text"),
        ]);

        assert_eq!(session_ids(&folded), vec!["parent"]);
        assert_eq!(folded[0].agent_search_text, "parent text\nchild text");
        assert_eq!(folded[0].message_count, 2);
        assert_eq!(folded[0].total_tokens, 20);
    }

    /// Folding into the immediate parent would drop the deeper thread: its parent
    /// is no longer a row by the time it is looked up.
    #[test]
    fn a_chain_of_sub_agents_folds_into_the_session_at_its_root() {
        let folded = fold_subagent_sessions(vec![
            session("root", None, "root text"),
            session("middle", Some("root"), "middle text"),
            session("leaf", Some("middle"), "leaf text"),
        ]);

        assert_eq!(session_ids(&folded), vec!["root"]);
        assert_eq!(
            folded[0].agent_search_text,
            "root text\nmiddle text\nleaf text"
        );
        assert_eq!(folded[0].message_count, 3);
        assert_eq!(folded[0].total_tokens, 30);
    }

    /// A thread parsed from its own transcript carries its dialogue in
    /// `full_text`; folding must move it into the parent's agent text or the
    /// thread's content would be unsearchable from anywhere.
    #[test]
    fn a_threads_dialogue_is_searchable_through_its_parent() {
        let mut thread = session("child", Some("parent"), "child agent text");
        thread.full_text = "child dialogue text".to_owned();

        let folded = fold_subagent_sessions(vec![session("parent", None, ""), thread]);

        assert_eq!(
            folded[0].agent_search_text,
            "child dialogue text\nchild agent text"
        );
        assert!(
            folded[0].full_text.is_empty(),
            "sub-agent content stays out of the parent's own index, as for Claude"
        );
    }

    /// Hiding a session whose parent is missing would make it unreachable, since
    /// nothing else lists it.
    #[test]
    fn a_sub_agent_session_with_no_parent_present_stays_listed() {
        let folded = fold_subagent_sessions(vec![
            session("orphan", Some("absent"), "orphan text"),
            session("other", None, ""),
        ]);

        assert_eq!(session_ids(&folded), vec!["orphan", "other"]);
    }

    #[test]
    fn sessions_whose_parents_loop_back_all_stay_listed() {
        let folded = fold_subagent_sessions(vec![
            session("first", Some("second"), "first text"),
            session("second", Some("first"), "second text"),
            session("its_own_parent", Some("its_own_parent"), "solo text"),
        ]);

        assert_eq!(
            session_ids(&folded),
            vec!["first", "second", "its_own_parent"]
        );
        assert_eq!(
            folded[0].message_count, 1,
            "a loop has no root to fold into, so nothing merges"
        );
    }

    #[test]
    fn a_repeated_session_id_takes_the_fold_on_its_first_row() {
        let folded = fold_subagent_sessions(vec![
            session("parent", None, "first row"),
            session("parent", None, "second row"),
            session("child", Some("parent"), "child text"),
        ]);

        assert_eq!(session_ids(&folded), vec!["parent", "parent"]);
        assert_eq!(folded[0].agent_search_text, "first row\nchild text");
        assert_eq!(folded[1].agent_search_text, "second row");
    }

    #[test]
    fn sessions_without_parents_pass_through_unchanged() {
        let folded = fold_subagent_sessions(vec![
            session("first", None, "one"),
            session("second", None, "two"),
        ]);

        assert_eq!(session_ids(&folded), vec!["first", "second"]);
        assert_eq!(folded[0].message_count, 1);
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
