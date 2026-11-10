use super::*;
use serde_json::json;

#[test]
fn database_backed_sessions_validate_current_logical_fingerprint() {
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
        .write(&parsed_record(Source::OpenCode, &root, &stub))
        .unwrap();
    let super::super::CatalogueSession::Listed(cached) = cache
        .catalogue_session(storage, Source::OpenCode, &root, CACHE_KEY, false)
        .unwrap()
        .unwrap()
    else {
        panic!("listed fixture")
    };
    let content = cached.cached_session.as_ref().unwrap();
    let currency = || {
        content
            .read_copy(|held| held.currency(&cached.publication))
            .unwrap()
    };
    assert_eq!(currency(), super::super::Currency::Current);
    let source = rusqlite::Connection::open(&root.path).unwrap();
    assert!(source.execute("UPDATE part SET data = json_set(data, '$.text', 'fresh database text'), time_updated = time_updated + 1 WHERE json_extract(data, '$.type') = 'text'", []).unwrap() > 0);
    assert_eq!(
        currency(),
        super::super::Currency::Older,
        "a changed session database makes its copy older"
    );
    assert!(
        !cached
            .load_held_content()
            .unwrap()
            .row
            .full_text
            .contains("fresh database text"),
        "a changed session database's copy is read without parsing the database"
    );
}

#[test]
fn resumed_childs_earlier_files_are_dependencies_before_splicing() {
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
    let record = parsed_record(source, &root, &stub);
    assert!(
        record
            .catalogue
            .files
            .iter()
            .any(|file| file.path == child.root && file.kind == "continuation")
    );
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    cache.write(&record).unwrap();
    let newest = CachedFingerprint::of_file(&child.newest).unwrap();
    let earlier = std::fs::read_to_string(&child.root).unwrap();
    std::fs::write(&child.root, earlier.replace("alpha", "omega") + "\n").unwrap();
    assert_eq!(CachedFingerprint::of_file(&child.newest).unwrap(), newest);
    assert!(matches!(
        cache.entries(storage_of(source), &root, CACHE_KEY),
        Err(AppError::SessionCacheSourceChanged(_))
    ));
}

/// A catalogue read compares the stamps discovery took of a session's
/// transcript and sub-agent transcripts with the cache's, and stats only the
/// session's other files.
#[test]
fn catalogue_read_stats_only_files_discovery_didnt_stamp() {
    let home = tempfile::tempdir().unwrap();
    let child = crate::history::format::codex::test_support::write_edited_thread_with_subagents(
        home.path(),
        "019f0000-0000-7000-8000-000000000097",
    );
    let parent = every_record_sessions()
        .into_iter()
        .find(|session| session.row.source == Source::Codex)
        .unwrap();
    let root = root_of(&parent);
    let discovered_at = |modified_secs| {
        let mut stub = stub_of(&parent, modified_secs);
        stub.subagents.push(child.newest.clone());
        stub
    };
    let discovered = discovered_at(1);
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    cache
        .write(&parsed_record(Source::Codex, &root, &discovered))
        .unwrap();
    let storage = storage_of(Source::Codex);
    let is_current = |discovered: &[SessionStub]| {
        cache
            .current_fingerprints(storage, &root, discovered)
            .unwrap()
            .contains_key(CACHE_KEY)
    };
    let newest = std::fs::read_to_string(&child.newest).unwrap();
    std::fs::write(&child.newest, newest + "\n").unwrap();

    assert!(
        !is_current(&[]),
        "a stat finds the sub-agent transcript changed"
    );
    assert!(
        is_current(std::slice::from_ref(&discovered)),
        "the read compares discovery's stamp of the sub-agent transcript instead of a stat"
    );
    assert!(
        !is_current(&[discovered_at(2)]),
        "a session whose discovery stamp differs from the cache's is stat as before"
    );
    let earlier = std::fs::read_to_string(&child.root).unwrap();
    std::fs::write(&child.root, earlier.replace("alpha", "omega") + "\n").unwrap();
    assert!(
        !is_current(std::slice::from_ref(&discovered)),
        "the read stats the continued transcript discovery didn't stamp"
    );
}

#[test]
fn changing_or_removing_earlier_codex_rollout_invalidates_ordinary_text() {
    let home = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let thread = crate::history::format::codex::test_support::write_edited_thread_with_subagents(
        home.path(),
        "019f0000-0000-7000-8000-000000000099",
    );
    let mut cache = admitted_cache(base.path());
    let storage = storage_of(Source::Codex);
    let newest_stamp = CachedFingerprint::of_file(&thread.newest).unwrap();
    let original = std::fs::read_to_string(&thread.root).unwrap();
    let (record, root) = source_record(Source::Codex, &thread.newest);
    assert!(
        record
            .catalogue
            .files
            .iter()
            .any(|file| file.path == thread.root
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
            .current_fingerprints(storage, &root, &[])
            .unwrap()
            .contains_key(CACHE_KEY)
    );

    std::fs::write(&thread.root, original.replace("alpha", "omega") + "\n").unwrap();
    assert_eq!(
        CachedFingerprint::of_file(&thread.newest).unwrap(),
        newest_stamp
    );
    assert!(
        cache
            .current_fingerprints(storage, &root, &[])
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        cache.entries(storage, &root, CACHE_KEY),
        Err(AppError::SessionCacheSourceChanged(_))
    ));
    let (rebuilt, _) = source_record(Source::Codex, &thread.newest);
    cache.write(&rebuilt).unwrap();
    let read = cache.entries(storage, &root, CACHE_KEY).unwrap().unwrap();
    let fresh = normalized_session(
        crate::history::FOR_FIXTURES,
        Source::Codex,
        &thread.newest,
        &[],
    )
    .unwrap();
    assert_eq!(
        format!("{:?}", read.entries),
        format!("{:?}", fresh.entries)
    );
    assert!(format!("{:?}", read.entries).contains("omega"));

    std::fs::remove_file(&thread.root).unwrap();
    assert_eq!(
        CachedFingerprint::of_file(&thread.newest).unwrap(),
        newest_stamp
    );
    assert!(
        cache
            .current_fingerprints(storage, &root, &[])
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        cache.entries(storage, &root, CACHE_KEY),
        Err(AppError::SessionCacheSourceChanged(_))
    ));
    let (missing, _) = source_record(Source::Codex, &thread.newest);
    let fresh = normalized_session(
        crate::history::FOR_FIXTURES,
        Source::Codex,
        &thread.newest,
        &[],
    )
    .unwrap();
    assert_eq!(
        format!(
            "{:?}",
            missing.entries.as_ref().unwrap().written().unwrap().entries
        ),
        format!("{:?}", fresh.entries)
    );
    cache.write(&missing).unwrap();
    assert!(
        cache
            .current_fingerprints(storage, &root, &[])
            .unwrap()
            .contains_key(CACHE_KEY),
        "a session missing earlier turns is current while its files are unchanged"
    );
    let read = cache.entries(storage, &root, CACHE_KEY).unwrap().unwrap();
    assert_eq!(
        format!("{:?}", read.entries),
        format!("{:?}", fresh.entries),
        "the entries of a session missing earlier turns read as its parse"
    );

    std::fs::write(&thread.root, original).unwrap();
    let (recovered, _) = source_record(Source::Codex, &thread.newest);
    cache.write(&recovered).unwrap();
    assert!(
        cache
            .current_fingerprints(storage, &root, &[])
            .unwrap()
            .contains_key(CACHE_KEY)
    );
}

#[test]
fn bounded_read_doesnt_restore_unrelated_result() {
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
    let Some(RecordEntries::Written(written)) = &mut record.entries else {
        panic!("the record writes its entries")
    };
    written.malformed_lines.push(crate::history::MalformedLine {
        line_number: 3,
        detail: None,
    });
    let (_, crate::log_entry::LogEntry::User { message, .. }) = &mut written.entries[1] else {
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
        .message_entries(storage, &root, CACHE_KEY, 1..=1)
        .unwrap()
        .unwrap();
    assert_eq!(selected.entries.len(), 1);
    assert_eq!(selected.malformed_lines[0].line_number, 3);
    assert!(format!("{:?}", selected.entries).contains("requested text"));
    cache.admit_storage(storage).unwrap();
    cache.write(&record).unwrap();
    let super::super::CatalogueSession::Listed(cached) = cache
        .catalogue_session(storage, Source::Claude, &root, CACHE_KEY, false)
        .unwrap()
        .unwrap()
    else {
        panic!("listed fixture")
    };
    let bounded = cached
        .cached_session
        .as_ref()
        .unwrap()
        .read_copy(|held| held.agent_range(&source, crate::agent::refs::MessageRange::single(1)))
        .unwrap();
    assert_eq!(bounded.messages.len(), 2);
    assert_eq!(bounded.malformed_lines, [3]);
    assert!(format!("{:?}", bounded.messages[0]).contains("requested text"));
    let full = cache.entries(storage, &root, CACHE_KEY).unwrap().unwrap();
    assert!(
        format!("{:?}", full.entries).contains("\"original\""),
        "a full read holds the stored result"
    );
}

#[test]
fn text_only_result_arrays_reconstruct_without_fetching_source() {
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
        serde_json::to_value(&record.entries.as_ref().unwrap().written().unwrap().entries[0].1)
            .unwrap();
    let mut cache = admitted_cache(&base.path().join("cache"));
    cache.write(&record).unwrap();
    std::fs::remove_file(&source).unwrap();
    let read = cache
        .entries(storage_of(Source::Claude), &root, CACHE_KEY)
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::to_value(&read.entries[0].1).unwrap(), expected);
}

#[test]
fn object_result_reads_from_cache_without_its_source() {
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
    let expected =
        serde_json::to_value(&record.entries.as_ref().unwrap().written().unwrap().entries[0].1)
            .unwrap();
    let mut cache = admitted_cache(&base.path().join("cache"));
    cache.write(&record).unwrap();
    std::fs::remove_file(&source).unwrap();
    let read = cache
        .entries(storage_of(Source::Claude), &root, CACHE_KEY)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&read.entries[0].1).unwrap(),
        expected,
        "the object result passes through the cache unchanged"
    );
}
