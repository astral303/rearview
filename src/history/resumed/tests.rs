use crate::agent::message_numbering::ThreadIdentity;
use crate::clock::TestClock;
use crate::history::Conversation;
use crate::history::format::RecordLine;
use crate::history::parser::{fold_tail, folded_row};
use crate::history::provider::SessionRoot;
use crate::history::provider::contract_tests::every_record_sessions;
use crate::history::row_fold::{FoldCheckpoint, FoldedRow, RowFold};
use crate::log_entry::LogEntry;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The facts the list shows of `row`, folded.
fn folded_facts(row: &FoldedRow) -> String {
    format!(
        "{:?}",
        (
            &row.preview_first,
            &row.preview_last,
            &row.summary,
            &row.custom_title,
            &row.model,
            row.duration_minutes,
            row.message_count,
            row.assistant_messages,
            row.latest_activity,
        )
    )
}

/// The same facts of `row`, parsed.
fn parsed_facts(
    row: &Conversation,
    latest_activity: Option<chrono::DateTime<chrono::FixedOffset>>,
) -> String {
    format!(
        "{:?}",
        (
            &row.preview_first,
            &row.preview_last,
            &row.summary,
            &row.custom_title,
            &row.model,
            row.duration_minutes,
            row.message_count,
            row.assistant_messages,
            latest_activity,
        )
    )
}

fn finished(fold: &RowFold, path: &Path) -> String {
    folded_row(fold, path, None)
        .map_or_else(|reason| format!("{reason:?}"), |row| folded_facts(&row))
}

/// Each reply's id with its thread, among `entries`.
fn reply_ids(entries: &[(RecordLine, LogEntry)]) -> HashSet<(ThreadIdentity, String)> {
    entries
        .iter()
        .filter_map(|(record, entry)| {
            let id = match entry {
                LogEntry::Assistant { message, .. } => message.id.clone(),
                LogEntry::Progress { data, .. } => data
                    .pointer("/message/message/id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                _ => None,
            }?;
            Some((ThreadIdentity::of(entry, record.transcript.as_deref()), id))
        })
        .collect()
}

#[test]
fn row_fold_resumed_at_every_split_equals_whole_fold_for_every_agent() {
    for session in every_record_sessions() {
        let source = session.row.source;
        let root = SessionRoot::new(session.row.path.parent().unwrap());
        let stub = crate::history::provider::SessionStub {
            locator: session.row.path.clone(),
            subagents: session.row.subagents.clone(),
            cache_key: "every-record".to_owned(),
            fingerprint: crate::history::provider::Fingerprint::UNKNOWN,
        };
        let listed = source
            .provider()
            .storage()
            .parse_session(
                crate::history::FOR_FIXTURES,
                &stub,
                &root,
                None,
                &|| {},
                &TestClock::default(),
            )
            .unwrap()
            .expect("the session is listed");
        let row = listed.row;
        let (view, _) = listed.transcripts.into_entries_and_subagent_metadata();
        let subagents: HashSet<&Path> = stub.subagents.iter().map(PathBuf::as_path).collect();
        // The stored view doesn't hold a leading title. A resumed read
        // removes the titles from the leading entries, as here.
        let mut leading = source
            .provider()
            .storage()
            .transcript_reader()
            .leading_entries(
                &stub.locator,
                &crate::history::provider::TranscriptRead::full_read(),
            )
            .unwrap();
        let titles = crate::history::parser::take_leading_titles(&mut leading);
        let entries = &view.entries;
        let start = FoldCheckpoint::default();
        let (whole, _) = fold_tail(&start, &titles, entries, &subagents, None);
        let whole_row = folded_row(&whole, &stub.locator, None)
            .unwrap_or_else(|_| panic!("{}: the whole fold lists a row", source.label()));
        assert_eq!(
            folded_facts(&whole_row),
            parsed_facts(&row, whole_row.latest_activity),
            "{}: the fold over the stored view lists the parse's row",
            source.label()
        );
        let whole_facts = finished(&whole, &stub.locator);
        let whole_times = whole.message_times(None);

        let mut tested = 0;
        for split in 0..=entries.len() {
            if !reply_ids(&entries[..split]).is_disjoint(&reply_ids(&entries[split..])) {
                continue;
            }
            let (_, checkpoint) = fold_tail(&start, &titles, entries, &subagents, Some(split));
            let checkpoint = checkpoint.expect("a fold keeps its checkpoint at every split");
            let (resumed, _) = fold_tail(&checkpoint, &titles, &entries[split..], &subagents, None);
            assert_eq!(
                finished(&resumed, &stub.locator),
                whole_facts,
                "{}: the fold resumed at {split} lists the whole fold's row",
                source.label()
            );
            assert_eq!(
                format!(
                    "{:?}",
                    resumed.message_times(Some((&whole_times, checkpoint.message_count())))
                ),
                format!("{whole_times:?}"),
                "{}: the fold resumed at {split} times each message as the whole fold does",
                source.label()
            );
            tested += 1;
        }
        assert!(
            tested > entries.len() / 2,
            "{}: most splits fall outside a reply: {tested} of {}",
            source.label(),
            entries.len() + 1
        );
    }
}
