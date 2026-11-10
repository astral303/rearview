use super::*;

#[test]
fn unlisted_or_unfinished_session_doesnt_become_empty_successful_content_read() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let session = every_record_sessions().remove(0);
    let row = &session.row;
    let storage = storage_of(row.source);
    cache.admit_storage(storage).unwrap();
    let root = root_of(&session);
    let stub = stub_of(&session, 1);
    let address = crate::history::sqlite_cache::SessionAddress::new(
        storage,
        row.source,
        crate::history::sqlite_cache::SessionRoots::of_system(&root),
        stub.cache_key.clone(),
    );
    for status in [SessionStatus::Empty, SessionStatus::Unreadable] {
        let reason = "skipped transcript has no visible messages".to_owned();
        cache
            .write(
                &SessionRecord::unlisted(
                    &crate::history::sqlite_cache::RecordPlace {
                        storage,
                        source: row.source,
                        roots: crate::history::sqlite_cache::SessionRoots::of_system(&root),
                        directories: &crate::history::path::CanonicalDirectories::of_system(),
                        linker: row.source.provider().storage().thread_linker(),
                    },
                    &stub,
                    status,
                    reason,
                )
                .unwrap()
                .unwrap(),
            )
            .unwrap();
        let read = cache.read_copy_at(&address, |_| Ok(()));
        assert!(
            matches!(read, Err(CopyMiss::NotThere)),
            "{status:?}: a read of the session finds no copy: {read:?}"
        );
    }
    let record = parsed_record(row.source, &root, &stub);
    cache.write_catalogue(&record, None).unwrap();
    let read = cache.read_copy_at(&address, |_| Ok(()));
    assert!(
        matches!(read, Err(CopyMiss::NotReady)),
        "a read of a session being written finds its copy not ready: {read:?}"
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
    let Some(RecordEntries::Written(entries)) = record.entries.take() else {
        panic!("a parsed record writes its entries");
    };
    record.entries = Some(RecordEntries::Unwritten {
        reason: "temporary entry read failure".to_owned(),
        entries,
    });
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
            .current_fingerprints(storage, &root, &[])
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        cache.entries(storage, &root, CACHE_KEY),
        Err(AppError::SessionCacheNotReady { .. })
    ));

    let Some(RecordEntries::Unwritten { entries, .. }) = record.entries.take() else {
        unreachable!("the record was given unwritten entries above");
    };
    record.entries = Some(RecordEntries::Written(entries));
    cache.write(&record).unwrap();
    assert!(
        cache
            .current_fingerprints(storage, &root, &[])
            .unwrap()
            .contains_key(CACHE_KEY)
    );
    assert!(
        !cache
            .entries(storage, &root, CACHE_KEY)
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
    new.entries = Some(RecordEntries::Written(message_schema::transcript(vec![
        serde_json::json!({"type":"user","message":{"role":"user","content":"winning generation"}}),
    ])));
    let (previous, keys) = older.write_catalogue(&old, None).unwrap();
    newer.write(&new).unwrap();
    let replacement = copy_state::listed_session_of(
        &newer.connection,
        storage.directory,
        newer.session_roots(&root).key(),
        CACHE_KEY,
    )
    .unwrap()
    .unwrap();
    assert_ne!(previous, replacement.id);

    let old_rows = entries::SessionRows::of(
        old.entries.as_ref().unwrap().written().unwrap(),
        &std::collections::HashSet::new(),
    );
    assert!(
        matches!(entries::insert(&mut older.connection, previous, &keys.places, &old_rows, &|| Ok(()), &owner), Err(AppError::SessionCacheSuperseded(id)) if id == previous)
    );
    assert!(
        matches!(older.set_final_status(previous, SessionStatus::Listed, None), Err(AppError::SessionCacheSuperseded(id)) if id == previous)
    );
    assert_eq!(
        newer.current_fingerprints(storage, &root, &[]).unwrap()[CACHE_KEY].mtime_secs,
        2
    );
    assert_eq!(
        newer
            .entries(storage, &root, CACHE_KEY)
            .unwrap()
            .unwrap()
            .entries
            .len(),
        new.entries
            .as_ref()
            .unwrap()
            .written()
            .unwrap()
            .entries
            .len()
    );
    assert!(
        format!(
            "{:?}",
            newer
                .entries(storage, &root, CACHE_KEY)
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
    let roots = reader.session_roots(&root);
    let old = reader.with_read_snapshot(|connection| {
        let id = copy_state::listed_session_of(connection, storage.directory, roots.key(), CACHE_KEY)?.unwrap().id;
        let places = super::super::roots::Places::stored(connection, &roots)?;
        write_test_entries(&mut writer, message_schema::transcript(vec![
            serde_json::json!({"type":"user","message":{"role":"user","content":"new content"}}),
        ]));
        entries::read(connection, id, &places, None)
    }).unwrap();
    assert!(format!("{:?}", old.entries).contains("old content"));
    let new = reader.entries(storage, &root, CACHE_KEY).unwrap().unwrap();
    assert!(format!("{:?}", new.entries).contains("new content"));
}

#[test]
fn read_of_session_not_in_cache_fails_naming_its_row() {
    let base = tempfile::tempdir().unwrap();
    let mut cache = admitted_cache(base.path());
    let (storage, root) = write_test_entries(&mut cache, message_schema::transcript(Vec::new()));
    let roots = cache.session_roots(&root);
    let failure = cache
        .with_read_snapshot(|connection| {
            let id = copy_state::listed_session_of(
                connection,
                storage.directory,
                roots.key(),
                CACHE_KEY,
            )?
            .unwrap()
            .id;
            let places = super::super::roots::Places::stored(connection, &roots)?;
            entries::read(connection, id + 1, &places, None).map(|_| ())
        })
        .unwrap_err()
        .to_string();
    assert!(
        failure.contains("session row") && failure.contains("not in the session cache"),
        "the failure names the session row as not in the session cache: {failure}"
    );
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
            .current_fingerprints(storage, &root, &[])
            .unwrap()
            .is_empty()
    );
    assert_eq!(cache.stored_keys(storage, &root).unwrap(), [CACHE_KEY]);
    assert!(matches!(
        cache.entries(storage, &root, CACHE_KEY),
        Err(AppError::SessionCacheNotReady { .. })
    ));
    let stored = cache.stored_keys(storage, &root).unwrap();
    cache.remove(storage, &root, &stored).unwrap();
    assert!(cache.stored_keys(storage, &root).unwrap().is_empty());
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
