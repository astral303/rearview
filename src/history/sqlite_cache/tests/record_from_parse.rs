use super::*;
use crate::history::Conversation;
use crate::history::format::codex::test_support::{
    EditedThreadWithSubagents, write_edited_thread_with_subagents,
};
use crate::history::parser::SubagentContribution;
use std::collections::BTreeSet;

/// The parts of a record compared with fresh reads.
#[derive(Debug, PartialEq)]
struct RecordContent {
    entries: String,
    malformed_lines: Vec<(usize, Option<String>)>,
    search_parts: Vec<(usize, String)>,
    files: BTreeSet<(&'static str, PathBuf)>,
}

impl RecordContent {
    fn of(record: &SessionRecord) -> Self {
        let entries = record
            .entries
            .as_ref()
            .expect("a listed record holds entries")
            .written()
            .expect("the record writes its entries");
        assert!(
            record
                .catalogue
                .files
                .iter()
                .all(|file| file.kind != "continuation" || file.fingerprint.is_some()),
            "every continuation file in the record has a fingerprint"
        );
        Self {
            entries: format!("{:?}", entries.entries),
            malformed_lines: malformed_lines(entries),
            search_parts: record
                .search_parts
                .iter()
                .map(|part| (part.file_position, part.text.clone()))
                .collect(),
            files: record
                .catalogue
                .files
                .iter()
                .map(|file| (file.kind, file.path.clone()))
                .collect(),
        }
    }

    /// The record of `row`'s session as `normalized_session` and a fresh
    /// parse of each sub-agent transcript read it. The record's entries don't
    /// hold a title the agent keeps outside the transcript.
    fn of_fresh_reads(row: &Conversation) -> Self {
        let format = row.source.provider().format();
        let mut entries = normalized_session(
            crate::history::FOR_FIXTURES,
            row.source,
            &row.path,
            &row.subagents,
        )
        .unwrap();
        crate::history::parser::take_leading_titles(&mut entries.entries);
        let mut search_parts = Vec::new();
        let mut transcripts: BTreeSet<PathBuf> = entries
            .entries
            .iter()
            .filter_map(|(record, _)| record.transcript.as_deref().map(Path::to_path_buf))
            .collect();
        for (index, subagent) in row.subagents.iter().enumerate() {
            let Some(projection) = format
                .parse_transcript(crate::history::FOR_FIXTURES, subagent)
                .unwrap()
            else {
                continue;
            };
            transcripts.extend(
                projection
                    .entries
                    .iter()
                    .filter_map(|(record, _)| record.transcript.as_deref().map(Path::to_path_buf)),
            );
            search_parts.extend(
                SubagentContribution::search_parts(
                    projection.entries.iter().map(|(_, entry)| entry),
                )
                .into_iter()
                .map(|text| (index + 1, text)),
            );
        }
        let mut files: BTreeSet<_> = std::iter::once(("locator", row.path.clone()))
            .chain(
                row.subagents
                    .iter()
                    .map(|subagent| ("subagent", subagent.clone())),
            )
            .collect();
        let listed: BTreeSet<PathBuf> = files.iter().map(|(_, path)| path.clone()).collect();
        files.extend(
            transcripts
                .into_iter()
                .filter(|path| !listed.contains(path))
                .map(|path| ("continuation", path)),
        );
        Self {
            entries: format!("{:?}", entries.entries),
            malformed_lines: malformed_lines(&entries),
            search_parts,
            files,
        }
    }
}

fn malformed_lines(entries: &TranscriptEntries) -> Vec<(usize, Option<String>)> {
    entries
        .malformed_lines
        .iter()
        .map(|line| {
            (
                line.line_number,
                line.detail.as_ref().map(|detail| {
                    format!(
                        "{} {} {:?} {:?}",
                        detail.line_content,
                        detail.error_message,
                        detail.context_before,
                        detail.context_after
                    )
                }),
            )
        })
        .collect()
}

/// The root and stub of the Codex session at `locator`, with its sub-agent
/// transcripts `subagents`.
fn codex_stub(locator: &Path, subagents: Vec<PathBuf>) -> (SessionRoot, SessionStub) {
    // The deepest directory holding every file, so the record places each
    // below the root.
    let root = locator
        .ancestors()
        .skip(1)
        .find(|directory| subagents.iter().all(|path| path.starts_with(directory)))
        .unwrap();
    let root = SessionRoot::new(root);
    let stub = SessionStub {
        locator: locator.to_path_buf(),
        subagents,
        cache_key: CACHE_KEY.to_owned(),
        fingerprint: Fingerprint {
            size: 1_000,
            modified: Some(UNIX_EPOCH + Duration::from_secs(1)),
        },
    };
    (root, stub)
}

/// The record a load writes for the Codex session at `stub` under `root`,
/// with `after_parse` run between the parse and the record, and the number
/// of transcript reads the parse announced.
fn codex_record_changed_after_parse(
    root: &SessionRoot,
    stub: &SessionStub,
    after_parse: impl FnOnce(),
) -> (SessionRecord, usize) {
    let reads = std::sync::atomic::AtomicUsize::new(0);
    let stamps = StubStamps::current(stub);
    let listed = Source::Codex
        .provider()
        .storage()
        .parse_session(
            crate::history::FOR_FIXTURES,
            stub,
            root,
            None,
            &|| {
                reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
            &TestClock::default(),
        )
        .unwrap()
        .expect("the session is listed");
    after_parse();
    let ListedRecord::Built(record) = SessionRecord::listed(
        &crate::history::sqlite_cache::RecordPlace {
            storage: storage_of(Source::Codex),
            source: Source::Codex,
            roots: crate::history::sqlite_cache::SessionRoots::of_system(root),
            directories: &crate::history::path::CanonicalDirectories::of_system(),
            linker: Source::Codex.provider().storage().thread_linker(),
        },
        stub,
        stamps,
        &listed.row,
        listed.transcripts,
    ) else {
        panic!("the session has a record");
    };
    (*record, reads.into_inner())
}

/// The record of the Codex session at `locator`, its sub-agent transcripts
/// `subagents`, compared with fresh reads of the row's transcripts.
fn assert_codex_record_matches_fresh_reads(locator: &Path, subagents: Vec<PathBuf>) {
    let (root, stub) = codex_stub(locator, subagents);
    let (record, row) = parsed_record_and_row(Source::Codex, &root, &stub);
    let content = RecordContent::of(&record);
    assert!(
        content
            .files
            .iter()
            .any(|(kind, _)| *kind == "continuation"),
        "the session reads a continuation file: {:?}",
        content.files
    );
    assert_eq!(content, RecordContent::of_fresh_reads(&row));
}

#[test]
fn every_agents_record_matches_fresh_reads_of_its_transcripts() {
    for session in every_record_sessions() {
        let source = session.row.source;
        let record = parsed_record(source, &root_of(&session), &stub_of(&session, 1));
        assert_eq!(
            RecordContent::of(&record),
            RecordContent::of_fresh_reads(&session.row),
            "{source:?}'s record holds what fresh reads of its transcripts hold"
        );
    }
}

#[test]
fn edited_codex_threads_record_matches_fresh_reads_of_its_transcripts() {
    let home = tempfile::tempdir().unwrap();
    let thread = crate::history::format::codex::test_support::write_edited_thread_with_subagents(
        home.path(),
        "019f0000-0000-7000-8000-0000000000c1",
    );
    assert_codex_record_matches_fresh_reads(&thread.newest, thread.subagents());
}

#[test]
fn codex_record_with_resumed_subagent_matches_fresh_reads_of_its_transcripts() {
    let home = tempfile::tempdir().unwrap();
    let child = crate::history::format::codex::test_support::write_edited_thread_with_subagents(
        home.path(),
        "019f0000-0000-7000-8000-000000000097",
    );
    let parent = every_record_sessions()
        .into_iter()
        .find(|session| session.row.source == Source::Codex)
        .unwrap();
    let mut subagents = parent.row.subagents.clone();
    subagents.push(child.newest.clone());
    assert_codex_record_matches_fresh_reads(&parent.row.path, subagents);
}

/// Rewrites the file at `path` with `rewrite`, which keeps its length, and
/// sets its modification time back: only a second read finds the new text.
fn rewrite_keeping_fingerprint(path: &Path, rewrite: impl Fn(&str) -> String) {
    let fingerprint = CachedFingerprint::of_file(path).unwrap();
    let modified = std::fs::metadata(path).unwrap().modified().unwrap();
    let text = std::fs::read_to_string(path).unwrap();
    let rewritten = rewrite(&text);
    assert_eq!(rewritten.len(), text.len(), "the rewrite keeps the size");
    std::fs::write(path, rewritten).unwrap();
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(CachedFingerprint::of_file(path).unwrap(), fingerprint);
}

#[test]
fn record_holds_its_parses_text_and_parse_reads_each_transcript_once() {
    let home = tempfile::tempdir().unwrap();
    let thread =
        write_edited_thread_with_subagents(home.path(), "019f0000-0000-7000-8000-0000000000c2");
    let (root, stub) = codex_stub(&thread.newest, thread.subagents());
    let transcripts: Vec<PathBuf> = [&thread.root, &thread.first_edit, &thread.newest]
        .into_iter()
        .cloned()
        .chain(thread.subagents())
        .collect();
    let originals = ["alpha", "echo", "report"];

    let (record, reads) = codex_record_changed_after_parse(&root, &stub, || {
        for path in &transcripts {
            rewrite_keeping_fingerprint(path, |text| {
                originals.iter().fold(text.to_owned(), |text, original| {
                    text.replace(original, &original.to_uppercase())
                })
            });
        }
    });

    let shown_transcripts = record
        .catalogue
        .files
        .iter()
        .filter(|file| file.kind == "locator" || file.kind == "subagent")
        .count();
    let Some(RecordEntries::Written(entries)) = record.entries else {
        panic!("the listed record writes its entries");
    };
    let entries = format!("{:?}", entries.entries);
    for original in originals {
        assert!(
            entries.contains(original),
            "the entries hold the parse's `{original}`"
        );
        assert!(
            !entries.contains(&original.to_uppercase()),
            "the entries leave out `{original}` rewritten after the parse"
        );
    }
    assert_eq!(
        reads, shown_transcripts,
        "the parse reads the session's own transcript and each shown sub-agent's once"
    );
}

/// One of an edited thread's transcripts.
type ThreadTranscript = fn(&EditedThreadWithSubagents) -> PathBuf;

#[test]
fn transcript_changed_after_parse_fails_record_and_keeps_the_parsed_entries() {
    let changes: [(&str, ThreadTranscript); 3] = [
        ("the session's own transcript", |thread| {
            thread.newest.clone()
        }),
        ("a sub-agent transcript", |thread| thread.kept.clone()),
        ("an earlier transcript the session continues", |thread| {
            thread.root.clone()
        }),
    ];
    for (name, changed) in changes {
        let home = tempfile::tempdir().unwrap();
        let thread =
            write_edited_thread_with_subagents(home.path(), "019f0000-0000-7000-8000-0000000000c3");
        let changed = changed(&thread);
        let (root, stub) = codex_stub(&thread.newest, thread.subagents());

        let (record, _) = codex_record_changed_after_parse(&root, &stub, || {
            let text = std::fs::read_to_string(&changed).unwrap();
            std::fs::write(&changed, text + "\n").unwrap();
        });

        let error = match &record.entries {
            Some(RecordEntries::Unwritten { reason, .. }) => reason,
            Some(RecordEntries::Written(_)) => {
                panic!("{name} changed after the parse fails the record")
            }
            None => panic!("a listed record holds entries"),
        };
        // The record spells the path as the parse found it, which can differ
        // from the fixture's spelling in its separators.
        let listed = record
            .catalogue
            .files
            .iter()
            .find(|file| file.path == changed)
            .unwrap_or_else(|| panic!("the record lists {name}"));
        assert_eq!(
            *error,
            AppError::SessionCacheSourceChanged(listed.path.clone()).to_string(),
            "{name} changed after the parse fails the record"
        );
        let kept = record
            .into_parsed_entries()
            .unwrap_or_else(|| panic!("the record of {name} changed keeps the parsed entries"));
        assert!(
            format!("{:?}", kept.entries).contains("alpha"),
            "the kept entries are the parse's"
        );
    }
}
