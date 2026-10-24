use super::*;
use serde_json::json;

#[test]
#[ignore = "fixed in fix step 2"]
fn database_backed_sessions_validate_the_current_logical_fingerprint() {
    let session = every_record_sessions()
        .into_iter()
        .find(|session| session.row.source == Source::OpenCode)
        .unwrap();
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let storage = storage_of(Source::OpenCode);
    let root = root_of(&session);
    let stub = stub_of(&session, 1);
    cache.admit_storage(storage).unwrap();
    cache
        .write(
            &SessionRecord::listed(storage, Source::OpenCode, &root, &stub, &session.row).unwrap(),
        )
        .unwrap();
    let super::super::CatalogueSession::Listed(cached) = cache
        .catalogue_session(storage, Source::OpenCode, &root.path, CACHE_KEY, false)
        .unwrap()
        .unwrap()
    else {
        panic!("listed fixture")
    };
    let content = cached.cached_session.as_ref().unwrap();
    assert!(content.entries().unwrap().is_some());
    let source = rusqlite::Connection::open(&root.path).unwrap();
    assert!(source.execute("UPDATE part SET data = json_set(data, '$.text', 'fresh database text'), time_updated = time_updated + 1 WHERE json_extract(data, '$.type') = 'text'", []).unwrap() > 0);
    assert!(matches!(
        content.entries(),
        Err(AppError::SessionCacheSourceChanged(_))
    ));
    assert!(
        !cached
            .load_content()
            .unwrap()
            .full_text
            .contains("fresh database text"),
        "a changed session database's copy is read without parsing the database"
    );
}

#[test]
fn a_resumed_childs_earlier_files_are_dependencies_before_splicing() {
    let home = tempfile::tempdir().unwrap();
    let child = crate::history::format::codex::test_support::write_edited_thread_with_subagents(
        home.path(),
        "019f0000-0000-7000-8000-000000000098",
    );
    let parent = every_record_sessions()
        .into_iter()
        .find(|session| session.row.source == Source::Codex)
        .unwrap();
    let root = root_of(&parent);
    let mut stub = stub_of(&parent, 1);
    stub.subagents.push(child.newest.clone());
    let source = Source::Codex;
    let row = source
        .provider()
        .storage()
        .parse_session(
            &stub,
            &root,
            None,
            &|| {},
            &crate::clock::TestClock::default(),
        )
        .unwrap()
        .unwrap();
    let record = SessionRecord::listed(storage_of(source), source, &root, &stub, &row).unwrap();
    assert!(
        record
            .catalogue
            .files
            .iter()
            .any(|file| Path::new(&file.path) == files::key(&child.root)
                && file.kind == "continuation")
    );
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    cache.write(&record).unwrap();
    let newest = files::fingerprint(&child.newest).unwrap();
    let earlier = std::fs::read_to_string(&child.root).unwrap();
    std::fs::write(&child.root, earlier.replace("alpha", "omega") + "\n").unwrap();
    assert_eq!(files::fingerprint(&child.newest).unwrap(), newest);
    assert!(matches!(
        cache.entries(storage_of(source), &root.path, CACHE_KEY),
        Err(AppError::SessionCacheSourceChanged(_))
    ));
}

#[test]
fn changing_or_removing_an_earlier_codex_rollout_invalidates_ordinary_text() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let thread = crate::history::format::codex::test_support::write_edited_thread_with_subagents(
        home.path(),
        "019f0000-0000-7000-8000-000000000099",
    );
    let mut cache = admitted_cache(base.path());
    let storage = storage_of(Source::Codex);
    let newest_stamp = files::fingerprint(&thread.newest).unwrap();
    let original = std::fs::read_to_string(&thread.root).unwrap();
    let (record, root) = source_record(Source::Codex, &thread.newest);
    assert!(
        record
            .catalogue
            .files
            .iter()
            .any(|file| Path::new(&file.path) == files::key(&thread.root)
                && file.fingerprint.is_some()
                && file.kind == "continuation")
    );
    cache.write(&record).unwrap();
    let kinds: Vec<String> = cache
        .connection
        .prepare("SELECT kind FROM session_files ORDER BY position")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(kinds, ["locator", "continuation"]);
    assert!(
        cache
            .current_fingerprints(storage, &root.path)
            .unwrap()
            .contains_key(CACHE_KEY)
    );

    std::fs::write(&thread.root, original.replace("alpha", "omega") + "\n").unwrap();
    assert_eq!(files::fingerprint(&thread.newest).unwrap(), newest_stamp);
    assert!(
        cache
            .current_fingerprints(storage, &root.path)
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        cache.entries(storage, &root.path, CACHE_KEY),
        Err(AppError::SessionCacheSourceChanged(_))
    ));
    let (rebuilt, _) = source_record(Source::Codex, &thread.newest);
    cache.write(&rebuilt).unwrap();
    let read = cache
        .entries(storage, &root.path, CACHE_KEY)
        .unwrap()
        .unwrap();
    let fresh = normalized_session(Source::Codex, &thread.newest, &[]).unwrap();
    assert_eq!(
        format!("{:?}", read.entries),
        format!("{:?}", fresh.entries)
    );
    assert!(format!("{:?}", read.entries).contains("omega"));

    std::fs::remove_file(&thread.root).unwrap();
    assert_eq!(files::fingerprint(&thread.newest).unwrap(), newest_stamp);
    assert!(
        cache
            .current_fingerprints(storage, &root.path)
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        cache.entries(storage, &root.path, CACHE_KEY),
        Err(AppError::SessionCacheSourceChanged(_))
    ));
    let (missing, _) = source_record(Source::Codex, &thread.newest);
    let fresh = normalized_session(Source::Codex, &thread.newest, &[]).unwrap();
    assert_eq!(
        format!(
            "{:?}",
            missing.entries.as_ref().unwrap().as_ref().unwrap().entries
        ),
        format!("{:?}", fresh.entries)
    );
    cache.write(&missing).unwrap();
    assert!(
        cache
            .current_fingerprints(storage, &root.path)
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        cache.entries(storage, &root.path, CACHE_KEY),
        Err(AppError::SessionCacheNotReady { .. })
    ));

    std::fs::write(&thread.root, original).unwrap();
    let (recovered, _) = source_record(Source::Codex, &thread.newest);
    cache.write(&recovered).unwrap();
    assert!(
        cache
            .current_fingerprints(storage, &root.path)
            .unwrap()
            .contains_key(CACHE_KEY)
    );
}

#[test]
fn a_bounded_read_doesnt_restore_an_unrelated_result() {
    let base = tempfile::tempdir().unwrap();
    let source = base.path().join("source.jsonl");
    let lines = [
        json!({"type":"user","message":{"role":"user","content":"requested text"}}),
        json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"actual","content":{"opaque":"original"}}
        ]}}),
    ]
    .map(|line| line.to_string())
    .join("\n");
    std::fs::write(&source, lines).unwrap();
    let (mut record, root) = source_record(Source::Claude, &source);
    record
        .entries
        .as_mut()
        .unwrap()
        .as_mut()
        .unwrap()
        .malformed_lines
        .push(crate::history::MalformedLine {
            line_number: 3,
            detail: None,
        });
    let (_, crate::log_entry::LogEntry::User { message, .. }) =
        &mut record.entries.as_mut().unwrap().as_mut().unwrap().entries[1]
    else {
        panic!("user result")
    };
    let crate::log_entry::UserContent::Blocks(blocks) = &mut message.content else {
        panic!("blocks")
    };
    let crate::log_entry::ContentBlock::ToolResult { tool_use_id, .. } = &mut blocks[0] else {
        panic!("result")
    };
    *tool_use_id = "no-longer-present".to_owned();
    let mut cache = admitted_cache(&base.path().join("cache"));
    cache.write(&record).unwrap();
    let storage = storage_of(Source::Claude);
    let selected = cache
        .message_entries(storage, &root.path, CACHE_KEY, 1..=1)
        .unwrap()
        .unwrap();
    assert_eq!(selected.entries.len(), 1);
    assert_eq!(selected.malformed_lines[0].line_number, 3);
    assert!(format!("{:?}", selected.entries).contains("requested text"));
    cache.admit_storage(storage).unwrap();
    cache.write(&record).unwrap();
    let super::super::CatalogueSession::Listed(cached) = cache
        .catalogue_session(storage, Source::Claude, &root.path, CACHE_KEY, false)
        .unwrap()
        .unwrap()
    else {
        panic!("listed fixture")
    };
    let bounded = cached
        .cached_session
        .as_ref()
        .unwrap()
        .agent_range(&source, crate::agent::refs::MessageRange::single(1))
        .unwrap()
        .unwrap();
    assert_eq!(bounded.messages.len(), 2);
    assert_eq!(bounded.malformed_lines, [3]);
    assert!(format!("{:?}", bounded.messages[0]).contains("requested text"));
    assert!(matches!(
        cache.entries(storage, &root.path, CACHE_KEY),
        Err(AppError::SessionCacheSourceChanged(_))
    ));
}

#[test]
fn text_only_result_arrays_reconstruct_without_fetching_the_source() {
    let base = tempfile::tempdir().unwrap();
    let source = base.path().join("source.jsonl");
    let result = json!(["bare", {"type":"text","text":"content"}, {}, {"type":"text"}]);
    std::fs::write(
        &source,
        json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"call","content":result},
            {"type":"text","text":"question"}
        ]}})
        .to_string(),
    )
    .unwrap();
    let (mut record, root) = source_record(Source::Claude, &source);
    record.catalogue.files.clear();
    let expected =
        serde_json::to_value(&record.entries.as_ref().unwrap().as_ref().unwrap().entries[0].1)
            .unwrap();
    let mut cache = admitted_cache(&base.path().join("cache"));
    cache.write(&record).unwrap();
    std::fs::remove_file(&source).unwrap();
    let read = cache
        .entries(storage_of(Source::Claude), &root.path, CACHE_KEY)
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::to_value(&read.entries[0].1).unwrap(), expected);
}

#[test]
fn omitted_results_require_a_cached_source_generation_even_when_the_file_exists() {
    let base = tempfile::tempdir().unwrap();
    let source = base.path().join("source.jsonl");
    std::fs::write(
        &source,
        json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"call","content":{"opaque":"original"}},
            {"type":"text","text":"question"}
        ]}})
        .to_string(),
    )
    .unwrap();
    let (mut record, root) = source_record(Source::Claude, &source);
    record.catalogue.files.clear();
    let mut cache = admitted_cache(&base.path().join("cache"));
    cache.write(&record).unwrap();
    assert!(matches!(
        cache.entries(storage_of(Source::Claude), &root.path, CACHE_KEY),
        Err(AppError::SessionCacheSourceChanged(_))
    ));
}

#[cfg(windows)]
#[test]
fn alternate_windows_path_spelling_matches_the_cached_generation() {
    use crate::history::format::RecordLine;
    use std::sync::Arc;
    let base = tempfile::tempdir().unwrap();
    let source = base.path().join("source.jsonl");
    std::fs::write(
        &source,
        json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"call","content":{"opaque":"original"}},
            {"type":"text","text":"question"}
        ]}})
        .to_string(),
    )
    .unwrap();
    let (mut record, root) = source_record(Source::Claude, &source);
    let alternate = base.path().join(".").join("SOURCE.JSONL");
    record.entries.as_mut().unwrap().as_mut().unwrap().entries[0].0 = RecordLine {
        transcript: Some(Arc::from(alternate.as_path())),
        line: 1,
    };
    let mut cache = admitted_cache(&base.path().join("cache"));
    cache.write(&record).unwrap();
    let read = cache
        .entries(storage_of(Source::Claude), &root.path, CACHE_KEY)
        .unwrap()
        .unwrap();
    assert!(format!("{:?}", read.entries).contains("original"));
}
