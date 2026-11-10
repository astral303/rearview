use super::*;

#[test]
fn unlisted_or_unfinished_session_doesnt_become_empty_successful_content_read() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let session = every_record_sessions().remove(0);
    let row = &session.row;
    let storage = storage_of(row.source);
    let root = root_of(&session);
    let stub = stub_of(&session, 1);
    for status in [SessionStatus::Empty, SessionStatus::Unreadable] {
        let reason = "skipped transcript has no visible messages".to_owned();
        cache
            .write(
                &SessionRecord::unlisted(storage, row.source, &root, &stub, status, reason)
                    .unwrap(),
            )
            .unwrap();
        assert!(
            cache
                .find_session(row.source, &row.path, &row.subagents)
                .unwrap()
                .is_none()
        );
    }
    let record = parsed_record(row.source, &root, &stub);
    let keys = cache.catalogue_keys(&record.catalogue);
    cache.write_catalogue(&record, &keys, None).unwrap();
    assert!(
        cache
            .find_session(row.source, &row.path, &row.subagents)
            .unwrap()
            .is_none()
    );
}

#[test]
fn failed_entry_read_keeps_catalogue_but_requires_another_read() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let session = every_record_sessions().remove(0);
    let storage = storage_of(session.row.source);
    let root = root_of(&session);
    let mut record = parsed_record(session.row.source, &root, &stub_of(&session, 1));
    let entries = record.entries.take().unwrap();
    record.entries = Some(Err("temporary entry read failure".to_owned()));
    cache.write(&record).unwrap();

    assert_eq!(
        cache
            .connection
            .query_row("SELECT status FROM sessions", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "listed"
    );
    assert!(
        cache
            .current_fingerprints(storage, &root.path, &[])
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        cache.entries(storage, &root.path, CACHE_KEY),
        Err(AppError::SessionCacheNotReady { .. })
    ));

    record.entries = Some(entries);
    cache.write(&record).unwrap();
    assert!(
        cache
            .current_fingerprints(storage, &root.path, &[])
            .unwrap()
            .contains_key(CACHE_KEY)
    );
    assert!(
        !cache
            .entries(storage, &root.path, CACHE_KEY)
            .unwrap()
            .unwrap()
            .entries
            .is_empty()
    );
}

#[test]
fn superseded_writer_doesnt_add_children_or_set_replacements_final_status() {
    let base = tempfile::tempdir().unwrap();
    let mut older = admitted_cache(base.path());
    let owner = older.token().clone();
    let mut newer = SqliteCache::open_for_refresh(&owner).unwrap();
    let session = every_record_sessions().remove(0);
    let storage = storage_of(session.row.source);
    let root = root_of(&session);
    let old = parsed_record(session.row.source, &root, &stub_of(&session, 1));
    let mut new = parsed_record(session.row.source, &root, &stub_of(&session, 2));
    new.entries = Some(Ok(message_schema::transcript(vec![
        serde_json::json!({"type":"user","message":{"role":"user","content":"winning generation"}}),
    ])));
    let keys = older.catalogue_keys(&old.catalogue);
    let previous = older.write_catalogue(&old, &keys, None).unwrap();
    newer.write(&new).unwrap();
    let replacement = entries::listed_session_of(
        &newer.connection,
        storage.directory,
        &path_text(&root.path),
        CACHE_KEY,
    )
    .unwrap()
    .unwrap();
    assert_ne!(previous, replacement.id);

    let old_rows = entries::SessionRows::of(
        old.entries.as_ref().unwrap().as_ref().unwrap(),
        &std::collections::HashSet::new(),
    );
    assert!(
        matches!(entries::insert(&mut older.connection, previous, &old_rows, &|| Ok(()), &owner), Err(AppError::SessionCacheSuperseded(id)) if id == previous)
    );
    assert!(
        matches!(older.set_final_status(previous, SessionStatus::Listed, None), Err(AppError::SessionCacheSuperseded(id)) if id == previous)
    );
    assert_eq!(
        newer
            .current_fingerprints(storage, &root.path, &[])
            .unwrap()[CACHE_KEY]
            .mtime_secs,
        2
    );
    assert_eq!(
        newer
            .entries(storage, &root.path, CACHE_KEY)
            .unwrap()
            .unwrap()
            .entries
            .len(),
        new.entries
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .entries
            .len()
    );
    assert!(
        format!(
            "{:?}",
            newer
                .entries(storage, &root.path, CACHE_KEY)
                .unwrap()
                .unwrap()
                .entries
        )
        .contains("winning generation")
    );
}

#[test]
fn read_snapshot_survives_replacement_between_identity_and_reconstruction() {
    let base = tempfile::tempdir().unwrap();
    let mut reader = admitted_cache(base.path());
    let mut writer = SqliteCache::open_for_refresh(reader.token()).unwrap();
    let (storage, root) = write_test_entries(
        &mut reader,
        message_schema::transcript(vec![
            serde_json::json!({"type":"user","message":{"role":"user","content":"old content"}}),
        ]),
    );
    let old = reader.with_read_snapshot(|connection| {
        let id = entries::listed_session_of(connection, storage.directory, &path_text(&root.path), CACHE_KEY)?.unwrap().id;
        write_test_entries(&mut writer, message_schema::transcript(vec![
            serde_json::json!({"type":"user","message":{"role":"user","content":"new content"}}),
        ]));
        entries::read(connection, id, None)
    }).unwrap();
    assert!(format!("{:?}", old.entries).contains("old content"));
    let new = reader
        .entries(storage, &root.path, CACHE_KEY)
        .unwrap()
        .unwrap();
    assert!(format!("{:?}", new.entries).contains("new content"));
}

#[test]
fn unfinished_session_doesnt_become_reusable_or_leave_deletion_set() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let (storage, root) = write_test_entries(&mut cache, message_schema::transcript(Vec::new()));
    cache
        .connection
        .execute("UPDATE sessions SET status = ?1", [catalogue::WRITING])
        .unwrap();
    assert!(
        cache
            .current_fingerprints(storage, &root.path, &[])
            .unwrap()
            .is_empty()
    );
    assert_eq!(cache.stored_keys(storage, &root.path).unwrap(), [CACHE_KEY]);
    assert!(matches!(
        cache.entries(storage, &root.path, CACHE_KEY),
        Err(AppError::SessionCacheNotReady { .. })
    ));
    let stored = cache.stored_keys(storage, &root.path).unwrap();
    cache.remove(storage, &root.path, &stored).unwrap();
    assert!(cache.stored_keys(storage, &root.path).unwrap().is_empty());
}

#[test]
fn blocked_checkpoint_doesnt_report_that_sample_reached_database() {
    let base = tempfile::tempdir().unwrap();
    let mut reader = admitted_cache(base.path());
    let mut writer = SqliteCache::open_for_refresh(reader.token()).unwrap();
    writer.connection.busy_timeout(Duration::ZERO).unwrap();
    write_test_entries(&mut reader, message_schema::transcript(Vec::new()));
    let snapshot = reader.connection.transaction().unwrap();
    snapshot
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    write_test_entries(&mut writer, message_schema::transcript(Vec::new()));
    assert!(writer.checkpoint().is_err());
    snapshot.commit().unwrap();
    writer.checkpoint().unwrap();
}
